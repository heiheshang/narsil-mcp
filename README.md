# narsil-mcp

> The blazing-fast, privacy-first MCP server for deep code intelligence

[![License](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org)
[![Tests](https://img.shields.io/badge/tests-passing-brightgreen.svg)](https://github.com/postrv/narsil-mcp)
[![MCP](https://img.shields.io/badge/MCP-compatible-blue.svg)](https://modelcontextprotocol.io)
[![1C (BSL)](https://img.shields.io/badge/1C-BSL-yellow.svg)](https://1c.ru)

A Rust-powered MCP (Model Context Protocol) server providing AI assistants with deep code understanding through 90 specialized tools.

## Fork features

This is a fork of [postrv/narsil-mcp](https://github.com/postrv/narsil-mcp). On top of upstream it adds:

- **MCP over HTTP** — a `streamable-http` transport (`POST /mcp`) alongside stdio, so one server process can serve many clients.
- **SQLite-backed index store** — a real on-disk store (WAL, one `.db` per repo) replacing the whole-index postcard snapshot; the index is auto-saved after indexing.
- **Streaming indexing** — files are processed in windows and the call graph is built in two streamed passes, so peak memory no longer scales with the whole corpus.
- **1C:Enterprise (BSL) support** — parses 1С/BSL modules (`.bsl`) via tree-sitter-bsl and normalizes 1С XML metadata into searchable documents.
- **Leaner search** — the full-text index and TF-IDF embeddings store only what's needed, keeping memory bounded on large repositories.

## Why narsil-mcp?

| Feature | narsil-mcp | XRAY | Serena | GitHub MCP |
|---------|------------|------|--------|------------|
| **Languages** | 32 | 4 | 30+ (LSP) | N/A |
| **Neural Search** | Yes | No | No | No |
| **Taint Analysis** | Yes | No | No | No |
| **SBOM/Licenses** | Yes | No | No | Partial |
| **Offline/Local** | Yes | Yes | Yes | No |
| **WASM/Browser** | Yes | No | No | No |
| **Call Graphs** | Yes | Partial | No | No |
| **Type Inference** | Yes | No | No | No |

## Key Features

- **Code Intelligence** - Symbol extraction, semantic search, call graph analysis
- **Neural Semantic Search** - Find similar code using embeddings (Voyage AI, OpenAI)
- **Security Analysis** - Taint analysis, vulnerability scanning, OWASP/CWE coverage
- **Supply Chain Security** - SBOM generation, dependency auditing, license compliance
- **Advanced Analysis** - Control flow graphs, data flow analysis, dead code detection

### Why Choose narsil-mcp?

- **Written in Rust** - Blazingly fast, memory-safe, single binary (~30MB)
- **Tree-sitter powered** - Accurate, incremental parsing for 32 languages ([full list](docs/languages.md))
- **Zero config** - Point at repos and go
- **MCP compliant** - Works with Claude, Cursor, VS Code Copilot, Zed, and any MCP client
- **Privacy-first** - Fully local, no data leaves your machine
- **Parallel indexing** - Uses all cores via Rayon
- **Smart excerpts** - Expands to complete syntactic scopes
- **Security-first** - Built-in vulnerability detection and taint analysis
- **Neural embeddings** - Optional semantic search with Voyage AI or OpenAI
- **WASM support** - Run in browser with WebAssembly build
- **Real-time streaming** - Results as indexing progresses for large repos

## Quick Start

```bash
# Install (Homebrew, Scoop, npm, Nix and source builds: see Getting Started)
cargo install narsil-mcp

# Index a repository
narsil-mcp --repos /path/to/your/project --git --call-graph
```

**Claude Code** — `.mcp.json` in the project root:

```json
{
  "mcpServers": {
    "narsil-mcp": {
      "command": "narsil-mcp",
      "args": ["--repos", ".", "--git", "--call-graph"]
    }
  }
}
```

Then run `claude` in the project: the assistant gets the code intelligence tools. Cursor, VS Code, Zed and Claude Desktop setups are in [Integrations](docs/integrations.md).

---

## Documentation

| Guide | Description |
|-------|-------------|
| [Getting Started](docs/getting-started.md) | Installation, feature builds, first run |
| [Installation Guide](docs/INSTALL.md) | Platform-specific install details |
| [Configuration](docs/configuration.md) | CLI flags, presets, config files, env vars |
| [Integrations](docs/integrations.md) | MCP clients, Claude Code plugin, Forgemax, Ralph |
| [Available Tools](docs/tools.md) | All 90 tools by category |
| [Supported Languages](docs/languages.md) | Languages and extracted symbols |
| [Neural Search](docs/neural-search.md) | Embedding backends and setup |
| [Visualization Frontend](docs/frontend.md) | Web UI for graphs |
| [WebAssembly](docs/wasm.md) | Browser build |
| [Protocol and HTTP API](docs/api.md) | MCP methods, HTTP endpoints, sessions |
| [Security Rules](docs/security.md) | Built-in rule sets |
| [Architecture](docs/architecture.md) | Layers and design patterns |
| [Performance](docs/performance.md) | Benchmarks |
| [Profiling](docs/profiling.md) | samply profiles and tracing spans |
| [Development](docs/development.md) | Building and testing from source |
| [Troubleshooting](docs/troubleshooting.md) | Common problems and fixes |
| [Roadmap](docs/roadmap.md) | Completed work and release highlights |
| [CCG Specification](docs/ccg-spec.md) | Code Context Graph format |
| [Licensing](docs/LICENSING.md) | Open core model |
| [Repository Structure](docs/REPO_STRUCTURE.md) | Open core separation |

Design notes: [1C embedding plan](docs/1c-embedding-implementation-plan.md), [call graph resolution plan](docs/callgraph-resolution-plan.md). Release history: [CHANGELOG.md](CHANGELOG.md).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Credits

Built with:
- [tree-sitter](https://tree-sitter.github.io/) - Incremental parsing
- [tantivy](https://github.com/quickwit-oss/tantivy) - Full-text search
- [tokio](https://tokio.rs/) - Async runtime
- [rayon](https://github.com/rayon-rs/rayon) - Data parallelism
- [serde](https://serde.rs/) - Serialization
