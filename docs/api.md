[← WebAssembly](wasm.md) · [Back to README](../README.md) · [Security Rules →](security.md)

# Protocol and HTTP API

narsil-mcp speaks MCP (JSON-RPC 2.0). It always serves **stdio**; with `--http` the same server is also mounted over HTTP together with a small REST surface used by the visualization frontend.

## Transports

| Transport | How to enable | Use it for |
|-----------|---------------|------------|
| stdio | default | one client per process (Claude Code, Cursor, Zed, ...) |
| streamable HTTP (`POST /mcp`) | `--http` (`NARSIL_HTTP=1`), port `--http-port` (default `3000`) | many clients sharing one indexed server |

In stdio mode stdout carries JSON-RPC only; logs go to stderr.

## MCP methods

| Method | Purpose |
|--------|---------|
| `initialize` | handshake; over HTTP also creates a session |
| `tools/list` | tools enabled for this client (preset, feature flags, `--preset`) |
| `tools/call` | run a tool: `{"name": "...", "arguments": {...}}` |
| `resources/list`, `resources/read` | `list` currently returns an empty list; `read` returns a file by URI as `text/plain` |
| `prompts/list`, `prompts/get` | built-in prompts |

Example (stdio):

```bash
(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_repos","arguments":{}}}'; sleep 3) \
  | narsil-mcp --repos . 2>/dev/null
```

Keep stdin open until the answers arrive: on EOF the server exits.

## HTTP endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/mcp` | MCP over streamable HTTP. `initialize` without a session returns an `Mcp-Session-Id` header; send it on every later request |
| `DELETE` | `/mcp` | close the session named by `Mcp-Session-Id` |
| `GET` | `/health` | liveness: `{"status":"ok","version":"..."}` |
| `GET` | `/ready` | readiness: `200` + `"status":"ready"` once initial indexing finished, `503` + `"initializing"` before that; `initialization` carries progress |
| `GET` | `/tools` | enabled tools with metadata (category, required flags, schema) |
| `POST` | `/tools/call` | run a tool without MCP framing: `{"tool": "search_code", "args": {...}}` (`name`/`arguments` also accepted) |
| `GET` | `/graph` | graph data for the frontend: `repo`, `view`, `root`, `depth` (max 20), `max_nodes` (max 5000) |
| `GET` | `/` | embedded frontend (builds with the `frontend` feature) |

### Sessions

- An HTTP session expires after **30 minutes** without requests. A request with an expired or unknown id gets `Unknown Mcp-Session-Id`; the client must `initialize` again.
- A request without the header (other than `initialize`) gets `Missing Mcp-Session-Id header: call initialize first`.
- Sessions live in memory: a server restart drops all of them.

### Security

The HTTP transport has **no authentication**. Bind it only to trusted networks or put an authenticating reverse proxy in front of it.

### Readiness vs. answers

Until `/ready` returns `200`, search tools answer from a partially built index and can return empty or fast-but-incomplete results. Wait for readiness (or `get_index_status` → Complete) before measuring latency or trusting an empty answer.

## See Also

- [Integrations](integrations.md) — client configuration for Claude Code, Cursor, VS Code and others
- [Configuration](configuration.md) — CLI flags, presets, environment variables
- [Available Tools](tools.md) — what `tools/call` can run
