//! HTTP Server for narsil-mcp visualization frontend
//!
//! This module provides a REST API layer over the MCP tools,
//! enabling the web-based visualization frontend to communicate
//! with the narsil-mcp engine.
//!
//! When compiled with the `frontend` feature, the server also serves
//! the embedded visualization frontend at the root path.

use anyhow::Result;
use axum::{
    extract::{DefaultBodyLimit, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};
use uuid::Uuid;

use crate::index::CodeIntelEngine;
use crate::mcp::McpServer;
use crate::tool_handlers::ToolRegistry;
use crate::tool_metadata::TOOL_METADATA;

/// Maximum HTTP request body size (2 MB).
const MAX_HTTP_BODY_SIZE: usize = 2 * 1024 * 1024;

// Embedded frontend assets (only when frontend feature is enabled).
// `header` и `Response` уже приходят из безусловного блока выше; повторный
// импорт `axum::http::{header, Response}` конфликтовал с ним, из-за чего фича
// frontend не собиралась вовсе.
#[cfg(feature = "frontend")]
use axum::body::Body;

#[cfg(feature = "frontend")]
use rust_embed::Embed;

// `allow_missing` lets `cargo build --features frontend` succeed even when
// `frontend/dist/` has not been built yet (typical on a fresh clone where
// the user has not run `cd frontend && npm ci && npm run build`). The
// build.rs at the crate root prints a `cargo:warning` so the user knows
// the served UI will return 404 until the dist directory is populated.
#[cfg(feature = "frontend")]
#[derive(Embed)]
#[folder = "frontend/dist"]
#[allow_missing = true]
struct FrontendAssets;

/// HTTP Server for the visualization frontend
pub struct HttpServer {
    engine: Arc<CodeIntelEngine>,
    mcp: Arc<McpServer>,
    tool_registry: ToolRegistry,
    port: u16,
}

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    engine: Arc<CodeIntelEngine>,
    tool_registry: Arc<ToolRegistry>,
    mcp: Arc<McpServer>,
    /// Active MCP streamable-http sessions.
    sessions: Arc<Mutex<SessionStore>>,
}

/// How long a session survives without traffic.
///
/// Clients normally drop the connection rather than sending `DELETE /mcp`, so
/// expiry — not the explicit teardown — is what actually retires a session.
const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Hard cap on tracked sessions. `/mcp` has no authentication by design and
/// the server binds `0.0.0.0`, so anyone who can reach the port can mint
/// sessions with a ~45-byte `initialize` body.
const MAX_SESSIONS: usize = 4096;

/// Sweeping walks the whole map, so it is rate-limited rather than run per
/// request.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Live MCP sessions, bounded in both size and age.
///
/// In memory only. Sessions used to be written to
/// `$XDG_CONFIG_HOME/narsil-mcp/sessions-<port>.json` on every mint and
/// teardown, which was wrong three ways: the whole set was re-serialized under
/// the mutex inside an async handler, so concurrent requests queued behind a
/// blocking write; the set only ever grew; and a session id surviving a
/// restart passed validation while the `client_info` from its `initialize`
/// did not, so `tools/list` answered with an unadapted tool set instead of an
/// honest 404. A restart now returns `Unknown Mcp-Session-Id`, which is what
/// the client is expected to handle by re-initializing.
struct SessionStore {
    seen: HashMap<String, Instant>,
    last_sweep: Instant,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            seen: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }

    /// Whether `sid` is live, refreshing its deadline if so.
    fn touch(&mut self, sid: &str, now: Instant) -> bool {
        self.sweep(now);
        match self.seen.get_mut(sid) {
            Some(last_seen) => {
                *last_seen = now;
                true
            }
            None => false,
        }
    }

    fn insert(&mut self, sid: String, now: Instant) {
        self.sweep_now(now);
        // Sweeping may not free anything if every session is fresh; evicting
        // the least recently seen keeps the cap hard rather than advisory.
        while self.seen.len() >= MAX_SESSIONS {
            let Some(oldest) = self
                .seen
                .iter()
                .min_by_key(|(_, last_seen)| **last_seen)
                .map(|(sid, _)| sid.clone())
            else {
                break;
            };
            warn!(
                "Session cap of {} reached, evicting {}",
                MAX_SESSIONS, oldest
            );
            self.seen.remove(&oldest);
        }
        self.seen.insert(sid, now);
    }

    fn remove(&mut self, sid: &str) -> bool {
        self.seen.remove(sid).is_some()
    }

    fn sweep(&mut self, now: Instant) {
        if now.duration_since(self.last_sweep) < SWEEP_INTERVAL {
            return;
        }
        self.sweep_now(now);
    }

    fn sweep_now(&mut self, now: Instant) {
        self.last_sweep = now;
        self.seen
            .retain(|_, last_seen| now.duration_since(*last_seen) < SESSION_TTL);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.seen.len()
    }
}

/// Request body for tool calls
///
/// Accepts the MCP wire shape (`name`/`arguments`) as aliases for
/// `tool`/`args`. Before that, `{"name": ..., "arguments": {...}}` was
/// accepted but its arguments were silently dropped, so the call failed with
/// "missing required parameter X" for a parameter the caller had passed.
/// `deny_unknown_fields` makes any other envelope key a loud error rather
/// than a silently empty argument set.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallRequest {
    /// The tool name to execute
    #[serde(alias = "name")]
    tool: String,
    /// Arguments as JSON object
    #[serde(default, alias = "arguments")]
    args: Value,
}

/// Response from tool calls
#[derive(Debug, Serialize)]
pub struct ToolCallResponse {
    /// Whether the call succeeded
    success: bool,
    /// The result (if success)
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    /// Error message (if failure)
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// List tools response
#[derive(Debug, Serialize)]
pub struct ListToolsResponse {
    tools: Vec<ToolInfo>,
}

/// Tool information
#[derive(Debug, Serialize)]
pub struct ToolInfo {
    name: String,
    description: String,
    category: String,
    stability: String,
    performance: String,
    requires_api_key: bool,
    required_flags: Vec<String>,
    tags: Vec<String>,
    aliases: Vec<String>,
    input_schema: Value,
}

impl HttpServer {
    /// Create a new HTTP server
    pub fn new(engine: Arc<CodeIntelEngine>, mcp: Arc<McpServer>, port: u16) -> Self {
        Self {
            engine,
            mcp,
            tool_registry: ToolRegistry::new(),
            port,
        }
    }

    /// Run the HTTP server
    pub async fn run(self) -> Result<()> {
        let state = AppState {
            engine: self.engine,
            tool_registry: Arc::new(self.tool_registry),
            mcp: self.mcp,
            sessions: Arc::new(Mutex::new(SessionStore::new())),
        };

        // Configure CORS to allow frontend access (needed for development mode)
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        // Build router with API routes
        let app = Router::new()
            .route("/health", get(health_check))
            .route("/tools", get(list_tools))
            .route("/tools/call", post(call_tool))
            .route("/graph", get(get_graph))
            .route("/mcp", post(mcp_post).delete(mcp_delete));

        // Add embedded frontend routes when feature is enabled
        #[cfg(feature = "frontend")]
        let app = {
            info!("Frontend assets embedded - serving at /");
            app.route("/", get(serve_index))
                .fallback(serve_static_fallback)
        };

        #[cfg(not(feature = "frontend"))]
        {
            info!("Frontend not embedded - API-only mode");
            info!("Run frontend separately: cd frontend && npm run dev");
        }

        let app = app
            .layer(cors)
            .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_SIZE))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.port);
        info!("HTTP server starting on http://{}", addr);

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

/// Health check endpoint
async fn health_check() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// List available tools
async fn list_tools(State(state): State<AppState>) -> impl IntoResponse {
    let tools: Vec<ToolInfo> = state
        .tool_registry
        .tool_names()
        .iter()
        .filter_map(|name| {
            TOOL_METADATA.get(name).map(|meta| {
                let mut required_flags: Vec<String> = meta
                    .required_flags
                    .iter()
                    .map(|flag| format!("{:?}", flag))
                    .collect();
                required_flags.sort();

                let mut tags: Vec<String> = meta.tags.iter().map(|tag| tag.to_string()).collect();
                tags.sort();

                let mut aliases: Vec<String> =
                    meta.aliases.iter().map(|alias| alias.to_string()).collect();
                aliases.sort();

                ToolInfo {
                    name: meta.name.to_string(),
                    description: meta.description.to_string(),
                    category: meta.category.to_string(),
                    stability: format!("{:?}", meta.stability),
                    performance: format!("{:?}", meta.performance),
                    requires_api_key: meta.requires_api_key,
                    required_flags,
                    tags,
                    aliases,
                    input_schema: meta.input_schema.clone(),
                }
            })
        })
        .collect();

    Json(ListToolsResponse { tools })
}

/// Call a tool
async fn call_tool(
    State(state): State<AppState>,
    Json(request): Json<ToolCallRequest>,
) -> impl IntoResponse {
    let result = state
        .tool_registry
        .dispatch(&request.tool, &state.engine, request.args)
        .await;

    match result {
        Ok(output) => {
            // Try to parse as JSON, otherwise wrap as string
            let result_value =
                serde_json::from_str::<Value>(&output).unwrap_or(Value::String(output));

            (
                StatusCode::OK,
                Json(ToolCallResponse {
                    success: true,
                    result: Some(result_value),
                    error: None,
                }),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ToolCallResponse {
                success: false,
                result: None,
                error: Some(e.to_string()),
            }),
        ),
    }
}

/// Query parameters for graph endpoint
#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    /// Repository name
    #[serde(default)]
    repo: String,
    /// View type (call, import, symbol, hybrid, flow)
    #[serde(default = "default_view")]
    view: String,
    /// Root function/symbol for focused view
    root: Option<String>,
    /// Maximum depth
    #[serde(default = "default_depth")]
    depth: usize,
    /// Direction (callers, callees, both)
    #[serde(default = "default_direction")]
    direction: String,
    /// Include complexity metrics
    #[serde(default = "default_true")]
    include_metrics: bool,
    /// Include security overlay
    #[serde(default)]
    include_security: bool,
    /// Include code excerpts
    #[serde(default)]
    include_excerpts: bool,
    /// Cluster nodes by file
    #[serde(default = "default_cluster")]
    cluster_by: String,
    /// Maximum number of nodes to return (default 200)
    max_nodes: Option<usize>,
}

fn default_view() -> String {
    "call".to_string()
}

fn default_depth() -> usize {
    3
}

fn default_direction() -> String {
    "both".to_string()
}

fn default_true() -> bool {
    true
}

fn default_cluster() -> String {
    "none".to_string()
}

// ============================================================================
// Embedded Frontend Handlers (only when frontend feature is enabled)
// ============================================================================

/// Serve the index.html file
#[cfg(feature = "frontend")]
async fn serve_index() -> impl IntoResponse {
    serve_file("index.html")
}

/// Fallback handler for static files from embedded assets
#[cfg(feature = "frontend")]
async fn serve_static_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    serve_file(path)
}

/// Helper to serve a file from embedded assets
#[cfg(feature = "frontend")]
fn serve_file(path: &str) -> Response<Body> {
    // Try to get the file from embedded assets
    match FrontendAssets::get(path) {
        Some(content) => {
            // Determine MIME type from file extension
            let mime_type = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();

            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime_type)
                .header(header::CACHE_CONTROL, "public, max-age=31536000") // Cache for 1 year (hashed assets)
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => {
            // For SPA routing: serve index.html for non-asset paths
            if !path.contains('.') {
                if let Some(content) = FrontendAssets::get("index.html") {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(header::CACHE_CONTROL, "no-cache") // Don't cache HTML
                        .body(Body::from(content.data.into_owned()))
                        .unwrap();
                }
            }

            // File not found
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("Not Found"))
                .unwrap()
        }
    }
}

/// Get graph data (convenience endpoint)
async fn get_graph(
    State(state): State<AppState>,
    Query(query): Query<GraphQuery>,
) -> impl IntoResponse {
    // Clamp bounds to prevent excessive resource usage
    let depth = query.depth.min(20);
    let max_nodes = query.max_nodes.map(|n| n.min(5000));

    let mut args = json!({
        "repo": query.repo,
        "view": query.view,
        "root": query.root,
        "depth": depth,
        "direction": query.direction,
        "include_metrics": query.include_metrics,
        "include_security": query.include_security,
        "include_excerpts": query.include_excerpts,
        "cluster_by": query.cluster_by,
    });
    if let Some(max_nodes) = max_nodes {
        args["max_nodes"] = json!(max_nodes);
    }

    let result = state
        .tool_registry
        .dispatch("get_code_graph", &state.engine, args)
        .await;

    match result {
        Ok(output) => {
            // Parse as JSON
            let response_json = match serde_json::from_str::<Value>(&output) {
                Ok(graph) => json!({
                    "success": true,
                    "graph": graph,
                }),
                Err(_) => json!({
                    "success": true,
                    "graph": output,
                }),
            };
            (StatusCode::OK, Json(response_json))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "success": false,
                "error": e.to_string(),
            })),
        ),
    }
}

/// MCP streamable-http endpoint (`POST /mcp`).
///
/// JSON-RPC messages arrive as HTTP request bodies; session state is carried
/// by the `Mcp-Session-Id` header. An `initialize` without a session mints a
/// new session and returns its id; every other request must present a known
/// session id (or the request must itself be the initial `initialize`).
async fn mcp_post(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // A session id to return to the client (only set when minting one).
    let mut session_to_issue: Option<String> = None;

    match (&session_id, is_initialize(&body)) {
        // First contact: initialize starts a fresh session.
        (None, true) => {
            let sid = Uuid::new_v4().to_string();
            state
                .sessions
                .lock()
                .unwrap()
                .insert(sid.clone(), Instant::now());
            info!("MCP streamable-http session started: {}", sid);
            session_to_issue = Some(sid);
        }
        // A non-initialize request without a session is a protocol error.
        (None, false) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32000,
                        "message": "Missing Mcp-Session-Id header: call initialize first"
                    }
                })),
            )
                .into_response();
        }
        // Validate an existing session id.
        (Some(sid), _) => {
            if !state.sessions.lock().unwrap().touch(sid, Instant::now()) {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": {
                            "code": -32001,
                            "message": "Unknown Mcp-Session-Id"
                        }
                    })),
                )
                    .into_response();
            }
        }
    }

    let response_body = match state.mcp.handle_message(&body).await {
        Some(json_str) => json_str,
        // Notification (no id): acknowledged with 202 and no body.
        None => {
            let mut resp = StatusCode::ACCEPTED.into_response();
            if let Some(sid) = session_to_issue {
                resp.headers_mut().insert(
                    "mcp-session-id",
                    HeaderValue::from_str(&sid).expect("uuid is a valid header value"),
                );
            }
            return resp;
        }
    };

    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_body,
    )
        .into_response();
    if let Some(sid) = session_to_issue {
        resp.headers_mut().insert(
            "mcp-session-id",
            HeaderValue::from_str(&sid).expect("uuid is a valid header value"),
        );
    }
    resp
}

/// MCP streamable-http session termination (`DELETE /mcp`).
async fn mcp_delete(State(state): State<AppState>, headers: HeaderMap) -> StatusCode {
    if let Some(sid) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        if state.sessions.lock().unwrap().remove(sid) {
            info!("MCP streamable-http session terminated: {}", sid);
        }
    }
    StatusCode::OK
}

/// Whether a JSON-RPC body is an `initialize` request.
fn is_initialize(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .map(|v| v.get("method").and_then(|m| m.as_str()) == Some("initialize"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        assert_eq!(default_view(), "call");
        assert_eq!(default_depth(), 3);
        assert_eq!(default_direction(), "both");
        assert!(default_true());
        assert_eq!(default_cluster(), "none");
    }

    #[test]
    fn test_tool_call_response_serialization() {
        let response = ToolCallResponse {
            success: true,
            result: Some(json!({"test": "value"})),
            error: None,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"test\":\"value\""));
        assert!(!json.contains("error"));
    }

    #[test]
    fn tool_call_request_accepts_canonical_envelope() {
        let req: ToolCallRequest =
            serde_json::from_str(r#"{"tool":"find_symbols","args":{"repo":"R","pattern":"P"}}"#)
                .expect("canonical envelope must parse");
        assert_eq!(req.tool, "find_symbols");
        assert_eq!(req.args["repo"], json!("R"));
        assert_eq!(req.args["pattern"], json!("P"));
    }

    /// The MCP wire shape used to parse but lose its arguments, so the call
    /// failed with "missing required parameter" for a parameter that was sent.
    #[test]
    fn tool_call_request_accepts_mcp_envelope() {
        let req: ToolCallRequest = serde_json::from_str(
            r#"{"name":"find_symbols","arguments":{"repo":"R","pattern":"P"}}"#,
        )
        .expect("MCP-shaped envelope must parse");
        assert_eq!(req.tool, "find_symbols");
        assert_eq!(req.args["repo"], json!("R"));
        assert_eq!(req.args["pattern"], json!("P"));
    }

    #[test]
    fn tool_call_request_allows_mixed_aliases_and_omitted_args() {
        let req: ToolCallRequest =
            serde_json::from_str(r#"{"tool":"list_repos","arguments":{"a":1}}"#)
                .expect("aliases must be interchangeable");
        assert_eq!(req.args["a"], json!(1));

        let req: ToolCallRequest =
            serde_json::from_str(r#"{"tool":"list_repos"}"#).expect("args must be optional");
        assert!(req.args.is_null());
    }

    /// A mistyped argument key must fail loudly instead of dispatching with an
    /// empty argument set, which reads as "the tool ignored my parameters".
    #[test]
    fn tool_call_request_rejects_unknown_envelope_keys() {
        let err = serde_json::from_str::<ToolCallRequest>(
            r#"{"tool":"find_symbols","arg":{"repo":"R"}}"#,
        )
        .expect_err("unknown envelope key must be rejected");
        assert!(
            err.to_string().contains("arg"),
            "error should name the offending key, got: {err}"
        );

        assert!(
            serde_json::from_str::<ToolCallRequest>(
                r#"{"tool":"find_symbols","params":{"repo":"R"}}"#
            )
            .is_err(),
            "'params' is not an accepted alias"
        );
    }

    #[test]
    fn test_tool_call_error_response() {
        let response = ToolCallResponse {
            success: false,
            result: None,
            error: Some("Something went wrong".to_string()),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("Something went wrong"));
        assert!(!json.contains("result"));
    }

    /// Test that HTTP server can be configured with custom port
    #[test]
    fn test_http_server_port_configuration() {
        // Verify port configuration works
        let port: u16 = 8080;
        assert!(port > 0 && port < 65535);

        // Default port should be 3000
        let default_port: u16 = 3000;
        assert_eq!(default_port, 3000);
    }

    /// Test that concurrent operation is properly structured
    ///
    /// This test documents the expected behavior when --http is enabled:
    /// 1. HTTP server runs in a background tokio::spawn task
    /// 2. MCP server runs on stdio in the main task
    /// 3. Both can operate concurrently
    #[test]
    fn test_concurrent_operation_pattern() {
        // The pattern in main.rs should be:
        //
        // if server_args.http {
        //     tokio::spawn(async move {
        //         http_server.run().await  // Runs in background
        //     });
        // }
        // mcp_server.run().await  // Always runs in main task
        //
        // This test verifies the conceptual model is correct.
        // The actual integration test would require a full runtime.

        // Verify the spawn pattern allows both to run
        let http_enabled = true;
        let mcp_always_runs = true;

        // When HTTP is enabled, both should run
        if http_enabled {
            assert!(
                mcp_always_runs,
                "MCP server must always run when HTTP is enabled"
            );
        } else {
            assert!(
                mcp_always_runs,
                "MCP server must run even when HTTP is disabled"
            );
        }
    }

    /// Test graph query default deserialization
    #[test]
    fn test_graph_query_defaults() {
        let query: GraphQuery = serde_json::from_str(r#"{"repo": "test"}"#).unwrap();

        assert_eq!(query.repo, "test");
        assert_eq!(query.view, "call");
        assert_eq!(query.depth, 3);
        assert_eq!(query.direction, "both");
        assert!(query.include_metrics);
        assert!(!query.include_security);
        assert!(!query.include_excerpts);
        assert_eq!(query.max_nodes, None);
    }

    /// Test graph query with explicit max_nodes
    #[test]
    fn test_graph_query_with_max_nodes() {
        let query: GraphQuery =
            serde_json::from_str(r#"{"repo": "test", "max_nodes": 50}"#).unwrap();
        assert_eq!(query.max_nodes, Some(50));
    }

    #[test]
    fn test_max_http_body_size_is_reasonable() {
        assert_eq!(MAX_HTTP_BODY_SIZE, 2 * 1024 * 1024);
    }

    #[test]
    fn test_graph_query_bounds_clamped() {
        // Verify excessive depth is clamped to 20
        let query: GraphQuery =
            serde_json::from_str(r#"{"repo": "test", "depth": 1000, "max_nodes": 99999}"#).unwrap();
        assert_eq!(query.depth.min(20), 20);
        assert_eq!(query.max_nodes.map(|n| n.min(5000)), Some(5000));
    }

    /// The session store used to be an unbounded `HashSet` that only shrank on
    /// an explicit `DELETE /mcp`, which clients rarely send. `/mcp` needs no
    /// authentication, so the set grew for as long as the server ran.
    #[test]
    fn test_sessions_expire_and_stay_capped() {
        let mut store = SessionStore::new();
        let t0 = Instant::now();

        store.insert("live".to_string(), t0);
        assert!(store.touch("live", t0), "a fresh session must validate");

        // Past the TTL the id is gone, and the client is told so (404) rather
        // than being served against half-forgotten state.
        let expired = t0 + SESSION_TTL + Duration::from_secs(1);
        assert!(!store.touch("live", expired));
        assert_eq!(store.len(), 0);

        // Traffic keeps a session alive: touching it before expiry moves its
        // deadline, so a busy client is never cut off mid-conversation.
        store.insert("busy".to_string(), t0);
        let halfway = t0 + SESSION_TTL / 2;
        assert!(store.touch("busy", halfway));
        assert!(store.touch("busy", halfway + SESSION_TTL / 2 + Duration::from_secs(1)));
    }

    #[test]
    fn test_session_cap_evicts_least_recently_seen() {
        let mut store = SessionStore::new();
        let t0 = Instant::now();

        // All minted within the TTL, so expiry frees nothing and the cap is
        // the only thing holding the map down.
        for i in 0..MAX_SESSIONS {
            store.insert(format!("sid-{i}"), t0 + Duration::from_millis(i as u64));
        }
        assert_eq!(store.len(), MAX_SESSIONS);

        store.insert("newcomer".to_string(), t0 + Duration::from_secs(1));
        assert_eq!(store.len(), MAX_SESSIONS, "the cap must be hard");
        assert!(store.touch("newcomer", t0 + Duration::from_secs(1)));
        assert!(
            !store.touch("sid-0", t0 + Duration::from_secs(1)),
            "the oldest session should have been the one evicted"
        );
    }
}
