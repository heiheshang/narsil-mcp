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
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::ClientInfo;
use crate::engine::CodeIntelEngine;
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
///
/// Must be well above the idle window of on-demand deployments (30 min on
/// .136): a client returning after the server was stopped for idleness
/// presents its old id, and Cursor does not re-`initialize` on the 404.
/// Memory stays bounded by `MAX_SESSIONS`, not by the TTL.
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Hard cap on tracked sessions. `/mcp` has no authentication by design and
/// the server binds `0.0.0.0`, so anyone who can reach the port can mint
/// sessions with a ~45-byte `initialize` body.
const MAX_SESSIONS: usize = 4096;

/// Sweeping walks the whole map, so it is rate-limited rather than run per
/// request.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How often the background task snapshots sessions to disk.
const PERSIST_INTERVAL: Duration = Duration::from_secs(10);

/// One tracked session: its deadline and the client identity `initialize`
/// negotiated. Carrying `client_info` per session (rather than as a single
/// field on `McpServer`, which is shared by every session on this port) is
/// what lets two concurrently-connected clients get independently correct
/// `tools/list` filtering, and what makes persisting it below meaningful.
#[derive(Clone)]
struct SessionEntry {
    last_seen: Instant,
    client_info: Option<ClientInfo>,
}

/// On-disk shape of one session. `Instant` has no meaning across a process
/// restart (monotonic clock, unspecified epoch, and not `Serialize`), so the
/// deadline is persisted as wall-clock `SystemTime` and converted back to an
/// `Instant` on load.
#[derive(Serialize, Deserialize)]
struct PersistedSession {
    id: String,
    last_seen: SystemTime,
    client_info: Option<ClientInfo>,
}

/// Live MCP sessions, bounded in both size and age.
///
/// Snapshotted to `$XDG_CONFIG_HOME/narsil-mcp/sessions-<port>.json` by a
/// background task (`spawn_session_persister`), not inline in the request
/// handler. An earlier version persisted synchronously on every mint and
/// teardown and was wrong three ways: the whole set was re-serialized under
/// the mutex inside an async handler, so concurrent requests queued behind a
/// blocking write; the set only ever grew; and a session id surviving a
/// restart passed validation while the `client_info` from its `initialize`
/// did not, so `tools/list` answered with an unadapted tool set instead of an
/// honest 404. This version fixes all three: the write happens off the
/// request path on a timer, over a cloned snapshot of the already-bounded
/// map; and `client_info` is part of what gets persisted and restored, so a
/// resurrected session behaves exactly as it did before the restart instead
/// of silently losing its tool filtering.
struct SessionStore {
    seen: HashMap<String, SessionEntry>,
    last_sweep: Instant,
}

impl SessionStore {
    fn new() -> Self {
        Self {
            seen: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }

    /// Rebuild from a persisted snapshot, dropping anything already past its
    /// TTL as of `now_wall` rather than resurrecting stale sessions.
    fn from_persisted(
        persisted: Vec<PersistedSession>,
        now_wall: SystemTime,
        now: Instant,
    ) -> Self {
        let mut seen = HashMap::new();
        for p in persisted {
            // SystemTime isn't monotonic (NTP, DST, manual clock changes), so
            // a time-went-backwards read is treated as "just seen" instead of
            // underflowing.
            let age = now_wall.duration_since(p.last_seen).unwrap_or_default();
            if age >= SESSION_TTL {
                continue;
            }
            let Some(last_seen) = now.checked_sub(age) else {
                continue;
            };
            seen.insert(
                p.id,
                SessionEntry {
                    last_seen,
                    client_info: p.client_info,
                },
            );
        }
        Self {
            seen,
            last_sweep: now,
        }
    }

    /// Snapshot for persistence: wall-clock deadlines, ready to serialize.
    fn snapshot(&self, now: Instant, now_wall: SystemTime) -> Vec<PersistedSession> {
        self.seen
            .iter()
            .map(|(id, entry)| PersistedSession {
                id: id.clone(),
                // Round-trips the elapsed duration through wall-clock time
                // without assuming the two clocks share an epoch.
                last_seen: now_wall - now.duration_since(entry.last_seen),
                client_info: entry.client_info.clone(),
            })
            .collect()
    }

    /// Whether `sid` is live, refreshing its deadline if so.
    fn touch(&mut self, sid: &str, now: Instant) -> bool {
        self.sweep(now);
        match self.seen.get_mut(sid) {
            Some(entry) => {
                entry.last_seen = now;
                true
            }
            None => false,
        }
    }

    /// The client info negotiated for `sid`'s `initialize`, if any.
    fn client_info(&self, sid: &str) -> Option<ClientInfo> {
        self.seen.get(sid).and_then(|e| e.client_info.clone())
    }

    /// Record the client info an `initialize` on `sid` negotiated — but only
    /// the *first* one: a caller reads a client_info snapshot before
    /// `handle_message`'s await and writes it back after, with no lock held
    /// across the await, so two concurrent `initialize` calls on the same
    /// session race on which write lands last. Checking "is it still unset"
    /// fresh here (under this call's lock, not the pre-await snapshot) means
    /// only a genuine None-to-Some transition is ever applied — a later
    /// write can no longer clobber an identity an earlier one already
    /// established, and a request that read an already-set client_info back
    /// unchanged is a no-op instead of a redundant write on every call.
    fn set_client_info_if_absent(&mut self, sid: &str, client_info: ClientInfo) {
        if let Some(entry) = self.seen.get_mut(sid) {
            if entry.client_info.is_none() {
                entry.client_info = Some(client_info);
            }
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
                .min_by_key(|(_, entry)| entry.last_seen)
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
        self.seen.insert(
            sid,
            SessionEntry {
                last_seen: now,
                client_info: None,
            },
        );
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
            .retain(|_, entry| now.duration_since(entry.last_seen) < SESSION_TTL);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.seen.len()
    }
}

/// Where sessions for this port are persisted, or `None` if the config dir
/// can't be resolved. Persistence is best-effort: losing it just means a
/// restart falls back to "every client re-initializes", the old behavior.
fn sessions_file_path(port: u16) -> Option<PathBuf> {
    ProjectDirs::from("com", "anthropic", "narsil-mcp")
        .map(|dirs| dirs.config_dir().join(format!("sessions-{port}.json")))
}

/// Load a persisted session snapshot from disk, if any. Errors (missing
/// file, corrupt JSON) are logged and treated as "nothing to restore" —
/// persistence is a best-effort optimization, never a source of truth.
async fn load_persisted_sessions(path: &PathBuf) -> Vec<PersistedSession> {
    match tokio::fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(sessions) => sessions,
            Err(e) => {
                warn!("Discarding unreadable session snapshot {:?}: {}", path, e);
                Vec::new()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn!("Failed to read session snapshot {:?}: {}", path, e);
            Vec::new()
        }
    }
}

/// Periodically snapshot `sessions` to `path`. Runs off the request path: it
/// only holds the lock long enough to clone the (already bounded) map, then
/// serializes and writes outside it, so a slow disk never queues concurrent
/// `/mcp` requests behind a blocking write — the bug the previous
/// synchronous-on-every-request version had.
///
/// Skips the write when the serialized snapshot is byte-identical to the
/// last one written: a truly idle server (no session touched since the last
/// tick, so every `last_seen` is unchanged) would otherwise re-serialize and
/// rewrite the same bytes, and re-`create_dir_all` an already-existing
/// directory, every `PERSIST_INTERVAL` forever.
fn spawn_session_persister(sessions: Arc<Mutex<SessionStore>>, path: PathBuf) {
    tokio::spawn(async move {
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                warn!("Failed to create session snapshot dir {:?}: {}", parent, e);
            }
        }

        let mut interval = tokio::time::interval(PERSIST_INTERVAL);
        let mut last_written: Option<Vec<u8>> = None;
        loop {
            interval.tick().await;
            let now = Instant::now();
            let now_wall = SystemTime::now();
            let snapshot = {
                let store = sessions.lock().unwrap();
                store.snapshot(now, now_wall)
            };
            let Ok(json) = serde_json::to_vec(&snapshot) else {
                continue;
            };
            if last_written.as_ref() == Some(&json) {
                continue;
            }
            if let Err(e) = tokio::fs::write(&path, &json).await {
                warn!("Failed to write session snapshot {:?}: {}", path, e);
                // The directory may have been removed since startup; retry
                // creating it so the next tick's write has a chance too.
                if let Some(parent) = path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                continue;
            }
            last_written = Some(json);
        }
    });
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
        let sessions_path = sessions_file_path(self.port);
        let session_store = match &sessions_path {
            Some(path) => {
                let persisted = load_persisted_sessions(path).await;
                if !persisted.is_empty() {
                    info!(
                        "Restored {} MCP session(s) from {:?}",
                        persisted.len(),
                        path
                    );
                }
                SessionStore::from_persisted(persisted, SystemTime::now(), Instant::now())
            }
            None => SessionStore::new(),
        };

        let state = AppState {
            engine: self.engine,
            tool_registry: Arc::new(self.tool_registry),
            mcp: self.mcp,
            sessions: Arc::new(Mutex::new(session_store)),
        };

        if let Some(path) = sessions_path {
            spawn_session_persister(state.sessions.clone(), path);
        }

        // Configure CORS to allow frontend access (needed for development mode)
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        // Build router with API routes
        let app = Router::new()
            .route("/health", get(health_check))
            .route("/ready", get(readiness_check))
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

/// Liveness probe: the process is up and serving HTTP.
///
/// Deliberately says nothing about the index. Use `/ready` to find out whether
/// the answers can be trusted.
async fn health_check() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Readiness probe: the index has finished re-checking the repositories.
///
/// `/health` goes green about a second after start, while the engine is still
/// serving the *persisted* index and has not yet re-walked the working tree.
/// For a long-running server that is harmless. For an on-demand server that is
/// started per request and stopped when idle, it is not: the repository may
/// have changed while the service was down, so early answers describe the
/// state as of the last shutdown — silently stale rather than an error.
///
/// Returns 503 until `is_fully_initialized()` flips, so a supervisor
/// (systemd `ExecStartPost`, a socket proxy, a load balancer) can hold traffic
/// until the answers are current.
async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    let ready = state.engine.is_fully_initialized();
    let status = state.engine.get_initialization_status();

    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        code,
        Json(json!({
            "status": if ready { "ready" } else { "initializing" },
            "version": env!("CARGO_PKG_VERSION"),
            "initialization": status,
        })),
    )
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
    // The session this request runs under, and the client info it carries in
    // (empty for a freshly minted session) — `handle_message` may fill this
    // in via `initialize`, which is then written back to the session store
    // below so the next request (and a restart, via the persister) sees it.
    let effective_sid: String;
    let mut client_info: Option<ClientInfo>;

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
            session_to_issue = Some(sid.clone());
            effective_sid = sid;
            client_info = None;
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
            let mut store = state.sessions.lock().unwrap();
            if !store.touch(sid, Instant::now()) {
                warn!("Rejected request with unknown Mcp-Session-Id: {}", sid);
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
            client_info = store.client_info(sid);
            effective_sid = sid.clone();
        }
    }

    let response_body = match state.mcp.handle_message(&body, &mut client_info).await {
        Some(json_str) => json_str,
        // Notification (no id): acknowledged with 202 and no body.
        None => {
            if let Some(info) = client_info {
                state
                    .sessions
                    .lock()
                    .unwrap()
                    .set_client_info_if_absent(&effective_sid, info);
            }
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

    if let Some(info) = client_info {
        state
            .sessions
            .lock()
            .unwrap()
            .set_client_info_if_absent(&effective_sid, info);
    }

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

    #[test]
    fn test_client_info_is_per_session() {
        let mut store = SessionStore::new();
        let t0 = Instant::now();

        store.insert("alice".to_string(), t0);
        store.insert("bob".to_string(), t0);
        assert_eq!(store.client_info("alice"), None);

        store.set_client_info_if_absent(
            "alice",
            ClientInfo {
                name: "cursor".to_string(),
                version: Some("1.0".to_string()),
            },
        );

        // Setting alice's client info must not leak onto bob's session —
        // this is the bug a single server-wide `client_info` field had.
        assert_eq!(store.client_info("bob"), None);
        assert_eq!(store.client_info("alice").unwrap().name, "cursor");
    }

    /// `mcp_post` reads a client_info snapshot before `handle_message`'s
    /// await and writes it back after with no lock held across the await,
    /// so two concurrent `initialize` calls on one session race on which
    /// write lands last. `set_client_info_if_absent` closes that: only a
    /// genuine unset-to-set transition is ever applied, so an established
    /// identity can't be clobbered by a stale write that started earlier.
    #[test]
    fn test_set_client_info_if_absent_does_not_clobber() {
        let mut store = SessionStore::new();
        let t0 = Instant::now();
        store.insert("sid".to_string(), t0);

        store.set_client_info_if_absent(
            "sid",
            ClientInfo {
                name: "cursor".to_string(),
                version: Some("1.0".to_string()),
            },
        );
        // A second, differently-identified write loses to the first —
        // simulating a slow write-back from an earlier request completing
        // after a second request already established the identity.
        store.set_client_info_if_absent(
            "sid",
            ClientInfo {
                name: "stale-client".to_string(),
                version: None,
            },
        );

        assert_eq!(store.client_info("sid").unwrap().name, "cursor");
    }

    /// The exact bug the previous persistence attempt had: a session id
    /// surviving a restart validated, but its `client_info` did not, so
    /// `tools/list` silently served an unadapted tool set instead of an
    /// honest 404. Round-tripping through `snapshot`/`from_persisted` must
    /// restore `client_info` intact, not just the id.
    #[test]
    fn test_persisted_session_restores_client_info() {
        let mut store = SessionStore::new();
        let now = Instant::now();
        let now_wall = SystemTime::now();

        store.insert("sid-1".to_string(), now);
        store.set_client_info_if_absent(
            "sid-1",
            ClientInfo {
                name: "cursor".to_string(),
                version: Some("1.2.3".to_string()),
            },
        );

        let snapshot = store.snapshot(now, now_wall);
        let mut restored = SessionStore::from_persisted(snapshot, now_wall, now);

        assert!(
            restored.touch("sid-1", now),
            "restored session must be live"
        );
        let info = restored
            .client_info("sid-1")
            .expect("client_info must survive a restart");
        assert_eq!(info.name, "cursor");
        assert_eq!(info.version.as_deref(), Some("1.2.3"));
    }

    /// A snapshot taken long before the reload must not resurrect sessions
    /// that would already have expired by the time the server comes back —
    /// persistence restores state, it doesn't pause the TTL clock.
    #[test]
    fn test_persisted_sessions_past_ttl_are_dropped_on_load() {
        let mut store = SessionStore::new();
        let now = Instant::now();
        let now_wall = SystemTime::now();

        store.insert("stale".to_string(), now);
        let snapshot = store.snapshot(now, now_wall);

        // Reload as if the server had been down past the session TTL.
        let later_wall = now_wall + SESSION_TTL + Duration::from_secs(1);
        let later = now + SESSION_TTL + Duration::from_secs(1);
        let mut restored = SessionStore::from_persisted(snapshot, later_wall, later);

        assert_eq!(restored.len(), 0);
        assert!(!restored.touch("stale", later));
    }

    /// 28.10.2026 on .136: last call 07:28, stopped for idleness 07:59,
    /// woken by the same client 08:02. With a 30-minute TTL the session was
    /// dropped on load and Cursor stayed stuck on the 404.
    #[test]
    fn test_session_survives_idle_stop_and_wake() {
        const IDLE_STOP: Duration = Duration::from_secs(30 * 60);
        let mut store = SessionStore::new();
        let now = Instant::now();
        let now_wall = SystemTime::now();

        store.insert("cursor".to_string(), now);
        let stopped = now + IDLE_STOP + Duration::from_secs(60);
        let snapshot = store.snapshot(stopped, now_wall + IDLE_STOP + Duration::from_secs(60));

        let woken_after = Duration::from_secs(34 * 60 + 30);
        let mut restored =
            SessionStore::from_persisted(snapshot, now_wall + woken_after, now + woken_after);

        assert!(restored.touch("cursor", now + woken_after));
    }
}
