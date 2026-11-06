[← Troubleshooting](troubleshooting.md) · [Back to README](../README.md) · [CCG Specification →](ccg-spec.md)

# Roadmap and Release Highlights

## Roadmap

### Completed

- [x] Multi-language symbol extraction (32 languages)
- [x] Full-text search with Tantivy (BM25 ranking)
- [x] Hybrid search (BM25 + TF-IDF with RRF)
- [x] AST-aware code chunking
- [x] Git blame/history integration
- [x] Call graph analysis with complexity metrics
- [x] Control flow graph (CFG) analysis
- [x] Data flow analysis (DFG) with reaching definitions
- [x] Dead code and dead store detection
- [x] Taint analysis for injection vulnerabilities
- [x] Security rules engine (OWASP, CWE, crypto, secrets)
- [x] SBOM generation (CycloneDX, SPDX)
- [x] Dependency vulnerability checking (OSV)
- [x] License compliance analysis
- [x] Import graph with circular dependency detection
- [x] Cross-language symbol resolution
- [x] Incremental indexing with Merkle trees
- [x] Index persistence
- [x] Watch mode for file changes
- [x] LSP integration
- [x] Remote repository support
- [x] Streaming responses

## What's New

### v1.6.x (Current)

- **Crash-proof chunking** - Fixed unsafe byte-level string slicing in `chunk_file()` and `extract_signature()` that caused `hybrid_search`, `search_chunks`, and `get_chunk_stats` to crash when processing files with multi-byte UTF-8 characters (emoji, CJK, accented chars). All byte slicing now uses safe `content.get()` with fallback.
- **NaN-safe sort operations** - Fixed 5 locations across search, embeddings, git, index, and extract modules where `partial_cmp().unwrap()` would panic on NaN float values. All sorts now use `unwrap_or(Ordering::Equal)`.
- **Defense-in-depth chunking** - Added `catch_unwind` wrappers around all repo-wide `chunk_file()` loops so a panic in one file skips it instead of crashing the entire MCP server.
- **Visualization frontend overhaul** - Full SPA with HashRouter routing, file tree sidebar, syntax-highlighted code viewer, dashboard, and per-repo overview pages
- **Graph view performance** - Import graph now uses cached index data instead of filesystem walks; symbol graph iterates file cache directly instead of markdown round-trips; all views respect `max_nodes` for early termination
- **Real control flow graphs** - Flow view now uses the real CFG builder (`cfg::analyze_function`) with proper basic blocks, branch conditions, and loop back-edges instead of the single-block stub
- **Hybrid graph budget splitting** - Hybrid view allocates 60/40 node budget between call and import graphs for balanced results
- **Fix [#14](https://github.com/postrv/narsil-mcp/issues/14): configurable embedding dimensions** - Added `--neural-dimension` CLI arg and `default_dimension_for_model()` lookup so models like `text-embedding-3-large` use correct dimensions (3072) instead of hardcoded 1536
- **Fix [#13](https://github.com/postrv/narsil-mcp/issues/13): Nix frontend build** - Added `frontendDist` derivation in `flake.nix` using `buildNpmPackage` so `nix profile install github:postrv/narsil-mcp#with-frontend` works
- **Rust security rules** - 18 new rules (RUST-004 to RUST-021) covering command injection, transmute, FFI boundaries, TOCTOU, ReDoS, static mut, SSRF, and more
- **Elixir security rules** - 18 new rules (EX-001 to EX-018) covering atom exhaustion, binary_to_term deserialization, Code.eval injection, Ecto SQL injection, Phoenix XSS, Erlang distribution security
- **Migrated from `serde_yaml` to `serde-saphyr`** - Deprecated `serde_yaml` replaced with actively maintained, panic-free YAML library
- **Custom favicon** - Frontend now uses narsil-mcp branding icon instead of default Vite logo
- **Dependency security** - Updated `time` to 0.3.47 (RUSTSEC-2026-0009), `bytes` to 1.11.1 (RUSTSEC-2026-0007)
- **Test count** increased from 1,611 to 1,763 (+152 tests)

### v1.5.x

- **Deterministic call graph resolution** - Scope-hint propagation disambiguates callee resolution (e.g., `App::run()` correctly resolves to `src/app/mod.rs::run`)
- **8 graph analysis fixes** - Qualified node keys, hotspot filtering, CFG expression handling, import path parsing
- **Nix flake improvements** - DRY `mkPkg` helper, removed unnecessary macOS frameworks, `--lib` test strategy for sandbox builds

### v1.4.x

- **SPARQL / RDF Knowledge Graph** - Query code intelligence data with SPARQL via Oxigraph
- **Code Context Graph (CCG)** - 12 tools for standardized, AI-consumable codebase representations with tiered layers (L0-L3)
- **Type-aware security analysis** - Enhanced taint tracking with type inference and trait implementations
- **Multi-language CFG/DFG** - Control flow and data flow analysis extended to Go, Java, C#, Kotlin
- **Infrastructure as Code scanning** - New `iac.yaml` rules for Terraform, CloudFormation, Kubernetes
- **Language-specific security rules** - New rules for Go, Java, C#, Kotlin, Bash
- **6 new languages** - Erlang, Elm, Fortran, PowerShell, Nix, Groovy
- **90 tools total** - Up from 79 with new SPARQL, CCG, and analysis capabilities

### v1.2.x

- **`exclude_tests` parameter** - 22 tools support filtering out test files
- **npm package** - Install via `npm install -g narsil-mcp`

### v1.1.x

- **Multi-platform distribution** - Install via Homebrew, Scoop, npm, Cargo, or direct download
- **Configurable tool presets** - Minimal, balanced, full, and security-focused presets
- **Automatic editor detection** - Optimal defaults for Zed, VS Code, Claude Desktop
- **Interactive setup wizard** - `narsil-mcp config init` for easy configuration
- **32 language support** - Added Dart, Julia, R, Perl, Zig, and more
- **Improved performance** - Faster startup with background indexing

### v1.0.x

- **Neural semantic search** - Find similar code using Voyage AI or OpenAI embeddings
- **Type inference** - Infer types in Python/JavaScript/TypeScript without external tools
- **Multi-language taint analysis** - Security scanning for PHP, Java, C#, Ruby, Kotlin
- **WASM build** - Run in browser for code playgrounds and educational tools
- **147 bundled security rules** - OWASP, CWE, crypto, secrets, Rust, Elixir detection
- **IDE configs included** - Claude Desktop, Cursor, VS Code, Zed templates

## See Also

- [Architecture](architecture.md) — current design
