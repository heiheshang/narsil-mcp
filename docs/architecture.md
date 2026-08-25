# Architecture & Design Patterns

**Version:** 1.8.0 | **Language:** Rust (Edition 2021) | **Last updated:** 2025-08-20

## Overview

narsil-mcp is a high-performance MCP (Model Context Protocol) server written in Rust that provides deep code intelligence through 90 specialized tools. The architecture prioritizes:

- **Performance:** ~2 GiB/s parsing throughput, <1µs symbol lookup, <1ms full-text search
- **Scalability:** SQLite with WAL mode, streaming indexing, lock-free concurrent data structures
- **Modularity:** Trait-based tool dispatch, feature flags for optional functionality
- **Reliability:** Memory-safe Rust, panic-resilient operations, comprehensive test coverage (1,763 tests)

### System Layers

```
┌─────────────────────────────────────────────────────────────┐
│                    MCP Protocol Layer                        │
│         ┌──────────────────────────────────────┐             │
│         │  JSON-RPC 2.0 (stdio / HTTP)         │             │
│         └──────────────────────────────────────┘             │
└─────────────────────────────────────────────────────────────┘
                            │
┌─────────────────────────────────────────────────────────────┐
│                  McpServer & Tool Registry                   │
│  ┌────────────────────────────────────────────────────────┐ │
│  │  ToolRegistry (HashMap<name, Box<ToolHandler>>)       │ │
│  │  - 90 tools across 13 categories                       │ │
│  │  - Async trait-based dispatch                          │ │
│  │  - Tool metadata management                            │ │
│  └────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                            │
┌─────────────────────────────────────────────────────────────┐
│                 CodeIntelEngine (Main Core)                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  Symbol Index (DashMap<String, Vec<Symbol>>)         │  │
│  │  Call Graph Analysis                                 │  │
│  │  Search Index (Tantivy BM25)                         │  │
│  │  Taint Analysis Engine                               │  │
│  │  Type Inference (Python/JS/TS)                       │  │
│  │  Control/Data Flow Analysis                          │  │
│  │  Dead Code Detection                                 │  │
│  │  Query Result Cache (LRU with TTL)                   │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                            │
┌─────────────────────────────────────────────────────────────┐
│              Persistence & Storage Layer                     │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  SQLite Store (WAL mode, multi-process safe)         │  │
│  │  - Symbols table with secondary indexes              │  │
│  │  - Call graph edges                                  │  │
│  │  - File metadata for change detection                │  │
│  │  - Schema versioning                                 │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                            │
┌─────────────────────────────────────────────────────────────┐
│                Parser & Indexing Layer                       │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  Tree-sitter Parser (32 languages)                   │  │
│  │  - Parallel file processing (Rayon)                  │  │
│  │  - Streaming indexing (windowed processing)          │  │
│  │  - AST-aware code chunking                           │  │
│  │  - 1C:Enterprise (BSL) support                       │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

---

## Module Structure

### Core Modules (Always Available)

| Module | Responsibility |
|--------|-----------------|
| `lib.rs` | Library exports for integration tests |
| `main.rs` | CLI argument parsing, server initialization |
| `symbols.rs` | Symbol definitions, types, qualified names |
| `parser.rs` | Tree-sitter language abstraction |
| `extract.rs` | AST-aware symbol extraction |
| `search.rs` | Tantivy BM25 full-text search |
| `index.rs` | CodeIntelEngine (main orchestrator) |
| `store.rs` | SQLite persistence layer |
| `embeddings.rs` | TF-IDF vector space embeddings |
| `incremental.rs` | Merkle tree change detection |
| `validation.rs` | Input validation and sanitization |
| `supply_chain.rs` | SBOM, CVE, license analysis |

### Native-only Modules (feature = "native")

| Module | Responsibility |
|--------|-----------------|
| `mcp.rs` | JSON-RPC 2.0 protocol handler |
| `tool_handlers/` | 90 tool implementations (trait dispatch) |
| `git.rs` | Git blame, history, contributors |
| `lsp.rs` | Language Server Protocol integration |
| `neural.rs` | API-based & ONNX embedding backends |
| `remote.rs` | GitHub repository cloning & API access |
| `http_server.rs` | Frontend visualization HTTP server |
| `streaming.rs` | Large result set streaming |

### Optional Feature Modules

| Module | Feature | Purpose |
|--------|---------|---------|
| `persistence/` | `graph` | RDF knowledge graph, SPARQL queries |
| `ccg/` | `graph` | Code Context Graph (L0-L3 layers) |
| `wasm.rs` | `wasm` | Browser-compatible WebAssembly build |
| `taint/` | (core) | Taint tracking for injection detection |
| `config/` | (core) | Configuration system with presets |
| `cache/` | (core) | Query result caching with TTL |

---

## Key Components

### 1. MCP Server (`src/mcp.rs`)

**Pattern:** Facade + Builder

Implements JSON-RPC 2.0 protocol over stdio or HTTP. Handles:
- Protocol initialization (capabilities negotiation)
- Tool invocation with parameter validation
- Tool filtering based on user preset
- Error handling and response formatting

```rust
pub struct McpServer {
    engine: Arc<CodeIntelEngine>,
    registry: ToolRegistry,
    filter: ToolFilter,  // Applies preset filtering
}
```

### 2. Tool Registry (`src/tool_handlers/mod.rs`)

**Pattern:** Registry + Trait-based Dispatch

Provides dynamic tool registration and async execution:

```rust
#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn name(&self) -> &'static str;
    async fn execute(&self, engine: &CodeIntelEngine, args: Value) 
        -> Result<String>;
}

pub struct ToolRegistry {
    handlers: HashMap<&'static str, Box<dyn ToolHandler>>,
}
```

**13 Tool Categories:**
- **repo:** list, structure, files, discovery
- **symbols:** search, definitions, references, exports
- **search:** full-text, semantic, hybrid, chunking
- **callgraph:** graph visualization, complexity, hotspots
- **analysis:** CFG, DFG, dead code detection
- **security:** taint tracking, OWASP/CWE scanning
- **supply_chain:** SBOM, CVE, license checking
- **git:** blame, history, contributors, hotspots
- **lsp:** hover info, type inference, go-to-def
- **remote:** GitHub repo cloning & indexing
- **neural:** semantic search with embeddings
- **sparql:** RDF knowledge graph queries
- **ccg:** Code Context Graph export/import

### 3. CodeIntelEngine (`src/index.rs`)

**Pattern:** Facade + Singleton (Arc-wrapped)

The core engine managing all code analysis operations.

#### Symbol Index
```rust
symbol_index: Arc<DashMap<String, Vec<Symbol>>>
```
Lock-free concurrent hashmap for O(1) symbol lookups across multiple threads.

#### Call Graph Analysis
```rust
pub struct CallGraph {
    call_edges: HashMap<String, Vec<CallEdge>>,
    call_in: HashMap<String, Vec<CallEdge>>,
}

pub struct CallEdge {
    pub from: String,
    pub to: String,
    pub edge_type: String,     // "direct", "method", "dynamic"
    pub resolved: bool,         // Deterministic resolution
    pub location: Option<Location>,
}
```

Features:
- Bidirectional graph traversal
- Deterministic ambiguity resolution via hint propagation
- Per-edge resolution markers (name match only, same-file match, qualified)
- Support for methods, free functions, and dynamic calls

#### Search Index
```rust
pub struct ConcurrentSearchIndex {
    index: Index,  // Tantivy BM25 index
}
```

Supports three search algorithms:
- **BM25:** Full-text relevance ranking
- **TF-IDF:** Vector-space semantic similarity
- **Hybrid:** Reciprocal Rank Fusion (RRF) combines both

#### Taint Analysis
```rust
pub struct TaintAnalyzer {
    sources: Vec<TaintSource>,  // input, files, network
    sinks: Vec<TaintSink>,      // SQL, command, XSS
    propagation_rules: HashMap<..>,
}
```

Data flow analysis to find injection vulnerabilities (SQL injection, command injection, XSS, path traversal).

#### Type Inference
Supports Python, JavaScript, and TypeScript:
- Variable type inference within functions
- Type error detection
- Enhanced taint analysis with type information

#### Query Cache
```rust
pub struct AnalysisCache {
    cache: DashMap<AnalysisCacheKey, (Result<T>, SystemTime)>,
    ttl_seconds: u64,  // default: 1800s (30 minutes)
}
```

LRU-style cache with configurable TTL, reduces repeated computation on large codebases.

### 4. Persistence Layer (`src/persist.rs`, `src/store.rs`)

**Pattern:** Repository (data access abstraction)

#### SQLite Schema

```
TABLE: symbols
  - id (PK)
  - repo, name, file_path, kind
  - qualified_name (indexed)
  - symbol (BLOB - serialized Symbol)
  
TABLE: edges
  - id (PK)
  - caller_id, callee_id, edge_type, resolved
  - Indexes on caller_id, callee_id
  
TABLE: files
  - (repo, path) - PK for change detection
  - content_hash, modified_time, size
  
TABLE: meta
  - key, value (schema version, etc.)
```

**WAL Mode Benefits:**
- Readers don't block writers
- Multi-process safe access to shared index
- Better concurrency than standard sqlite locking

**Evolution from v1.7:**
- **Before:** Postcard whole-index snapshots (entire index in memory)
- **After:** SQLite point-lookups (only query what's needed)

### 5. Parser (`src/parser.rs`)

**Pattern:** Strategy (language-specific implementations)

Uses tree-sitter for accurate, incremental parsing of 32 languages:

Supported: Rust, Python, JS, TS, Go, C, C++, Java, C#, Bash, Ruby, Kotlin, PHP, Swift, Verilog/SystemVerilog, Scala, Lua, Haskell, Elixir, Clojure, Dart, Julia, R, Perl, Zig, Erlang, Elm, Fortran, PowerShell, Nix, Groovy, **1C:Enterprise (BSL)**

**Streaming Indexing:**
Files processed in windows; call graph built in two streamed passes. Peak memory independent of corpus size.

### 6. Neural Engine (`src/neural.rs`)

**Pattern:** Strategy (pluggable backends)

```rust
pub enum NeuralBackend {
    Api {
        provider: String,      // "voyage" or "openai"
        model: String,
        api_key: String,
    },
    Onnx {
        model_path: PathBuf,
        tokenizer: Tokenizer,
    },
}
```

Supports:
- **Voyage AI:** voyage-code-2 (1536 dims)
- **OpenAI:** text-embedding-3-small (1536) / -large (3072)
- **Local ONNX:** Custom models with tokenizer

Uses `usearch` for approximate nearest neighbor search in embedding space.

### 7. Configuration System (`src/config/`)

**Pattern:** Builder + Strategy

Priority order (highest to lowest):
1. CLI flags (`--repos`, `--preset`, etc.)
2. Environment variables (`NARSIL_REPOS`, `NARSIL_PRESET`)
3. Project config (`.narsil.yaml` in repo root)
4. User config (`~/.config/narsil-mcp/config.yaml`)
5. Hardcoded defaults

**Tool Presets:**
- `minimal` (26 tools) - Zed, Cursor
- `balanced` (51 tools) - VS Code
- `full` (90 tools) - Claude Desktop
- `security-focused` - All security category tools

---

## Design Patterns

### Architectural Patterns

| Pattern | Usage | Benefits |
|---------|-------|----------|
| **Facade** | McpServer, CodeIntelEngine | Hides complexity, provides simple interface |
| **Registry** | ToolRegistry, ToolMetadata | Dynamic registration, efficient dispatch |
| **Strategy** | LanguageParser, NeuralBackend | Pluggable implementations |
| **Builder** | EngineOptions, Config | Flexible construction of complex objects |
| **Repository** | SqliteStore | Abstraction for data access |
| **Observer** | File watcher (notify crate) | Incremental index updates on file changes |
| **Adapter** | ToolHandlers | Unified interface for heterogeneous tools |

### Concurrency Patterns

| Pattern | Usage | Tradeoff |
|---------|-------|----------|
| **DashMap** | Symbol index, cache | Lock-free reads/writes, slightly higher memory |
| **Arc<Mutex>** | SQLite connection | Safe multi-thread access, serialized writes |
| **Rayon** | Parallel file indexing | Data parallelism across cores, +overhead |
| **Tokio** | Async I/O, MCP handler | Non-blocking I/O, async runtime overhead |
| **Channels** | Cross-task communication | Safe message passing, bounded queues |

### Security Patterns

| Pattern | Usage |
|---------|-------|
| **Taint Tracking** | Follow untrusted data from source to sink |
| **CFG/DFG Analysis** | Understand code flow and data dependencies |
| **Rule Engine** | Match code against OWASP/CWE vulnerability patterns |
| **Type-aware Analysis** | Use type information for precise vulnerability detection |

---

## Data Flow: Example

### `search_code` Tool Invocation

```
1. JSON-RPC Request arrives
   │
   ├─ McpServer::handle_tool_call
   │  │
   │  ├─ Validate JSON params
   │  └─ Extract: { repo, query, limit }
   │
   ├─ ToolRegistry::dispatch → SearchHandler
   │
   ├─ SearchHandler::execute
   │  │
   │  ├─ Check QueryCache for (repo, query, options)
   │  │
   │  ├─ CACHE HIT? → Return immediately
   │  │
   │  └─ CACHE MISS:
   │     │
   │     ├─ CodeIntelEngine::search_code
   │     │  │
   │     │  ├─ Query Tantivy BM25 index
   │     │  │
   │     │  ├─ Rank results by relevance score
   │     │  │
   │     │  ├─ Fetch code excerpts (expand_excerpts)
   │     │  │  - Expand to complete syntactic scope
   │     │  │
   │     │  └─ Return Vec<SearchResult>
   │     │
   │     ├─ Cache result (TTL = 30 min)
   │     │
   │     └─ Return JSON serialized results
   │
   └─ JSON-RPC Response sent to client
```

---

## Feature Flags (Cargo Features)

```toml
[features]
default = ["native"]

native    = [tokio, async-trait, reqwest, lsp-server, notify, ...]
graph     = [oxigraph, flate2]           # RDF + SPARQL + CCG
frontend  = [rust-embed, mime_guess]    # Embedded visualization UI
neural    = [usearch, ndarray]           # TF-IDF embeddings
neural-onnx = [neural, ort]              # Local ONNX inference
wasm      = [wasm-bindgen, web-sys]     # Browser build
```

### Build Configurations

| Build Command | Size | Features |
|---------------|------|----------|
| `cargo build --release` | ~30MB | MCP server (default) |
| `--features graph` | ~35MB | + SPARQL, RDF, CCG tools |
| `--features frontend` | ~31MB | + Embedded visualization UI |
| `--features graph,frontend` | ~40MB | Fully featured server |
| `--target wasm32 --features wasm` | ~3MB | Browser/WASM build |

---

## Performance Characteristics

### Benchmarks (Apple M1)

**Parsing:**
```
Rust (278 KB file):      131 µs (1.98 GiB/s throughput)
Mixed (5 files, 15 KB):   57 µs
```

**Search:**
```
Symbol exact match:  483 ns
Symbol prefix match: 2.7 µs
BM25 (1000 docs):    80 µs
TF-IDF (1000 docs):  130 µs
Hybrid (BM25+TF-IDF): 151 µs
```

**Indexing:**
```
narsil-mcp (53 files, 1.7K symbols):      220 ms
rust-analyzer (2.8K files, 50K symbols):  2.1 s
Linux kernel (78K+ files):                45 s
```

### Memory Optimizations

- **MiMalloc allocator:** Reduces peak RSS by 2-4GB vs glibc default
- **Streaming indexing:** Constant memory with windowed processing
- **DashMap:** Lock-free concurrent access with bounded memory
- **SQLite:** Point-lookups never require full index in memory

---

## Integration Points

### Claude Code Plugin

Located in `narsil-plugin/`, provides:

```
narsil/                    # Main catalogue (90 tools, parameter naming)
├── narsil-search/         # Code search strategies
├── narsil-callgraph/      # Call graph analysis
├── narsil-static-analysis/ # CFG, DFG, type inference
├── narsil-security/       # Taint + vulnerability rules
└── narsil-repo-state/     # Git + diagnostics
```

Each skill:
- Maps questions to appropriate tools
- Explains output interpretation
- Knows when to stop analysis
- Handles feature flag awareness

### Ralph Integration

[Ralph](https://github.com/postrv/ralphing-la-vida-locum) (Claude Code CI/CD automation) uses narsil-mcp for:
- Security scanning (OWASP/CWE detection via `scan_security`)
- Type checking (Python/JS/TS via `check_type_errors`)
- Injection detection (via `find_injection_vulnerabilities`)
- Architecture analysis (CCG layers via `export_ccg_*`)

---

## Extensibility

### Adding a New Tool

1. Create handler struct in `src/tool_handlers/`:
```rust
pub struct MyNewHandler;

#[async_trait]
impl ToolHandler for MyNewHandler {
    fn name(&self) -> &'static str { "my_new_tool" }
    async fn execute(&self, engine: &CodeIntelEngine, args: Value) 
        -> Result<String> { ... }
}
```

2. Register in `ToolRegistry::new()`:
```rust
registry.register(Box::new(MyNewHandler));
```

3. Add metadata in `src/tool_metadata.rs`:
```rust
TOOL_METADATA.push(ToolMetadata {
    name: "my_new_tool",
    description: "...",
    category: "custom",
    ...
});
```

4. (Optional) Create skill in `narsil-plugin/my-skill/`

---

## Quality & Testing

- **1,763 unit + integration tests** across all modules
- **Criterion.rs benchmarks** for parsing, indexing, search performance
- **Property-based tests** (proptest) for symbol parsing
- **Panic resilience:** Key indexing operations wrapped in `catch_unwind`
- **NaN safety:** All float comparisons use `unwrap_or(Ordering::Equal)`

Run tests:
```bash
cargo test              # All tests
cargo bench             # Benchmarks
cargo clippy            # Linting
cargo fmt --check       # Code formatting
RUST_LOG=debug cargo run -- --repos ./test-fixtures
```

---

## Version History

### v1.8.0 (Current)
- Crash-proof chunking (UTF-8 safe byte slicing)
- NaN-safe sort operations in 5 locations
- Defense-in-depth chunking with panic wrappers
- Visualization frontend SPA overhaul

### v1.6.x → v1.8.0
Backward compatible (no breaking changes)

### Index Persistence
- Schema versioning (v2 current)
- Auto-migration for schema updates
- WAL mode supports zero-downtime upgrades

---

## Key Architectural Decisions

| Decision | Rationale | Tradeoff |
|----------|-----------|----------|
| SQLite instead of Postcard | Scalable point-lookups, bounded memory | +5% memory, -1% speed on some ops |
| Streaming indexing | O(1) peak memory, handles large repos | More complex logic |
| DashMap for indices | Lock-free concurrency, no mutex contention | Slightly higher memory overhead |
| Tantivy BM25 | Fast full-text search, proven algorithm | +30MB binary size |
| Tree-sitter | Accurate multi-language parsing, incremental | C compiler required for build |
| Feature flags | Optional components (WASM, minimal) | Code complexity in Cargo.toml |
| Async/Tokio | Scalable I/O without threads | Rust async runtime overhead |
| Trait-based tools | Easy to add new tools, testable | Dynamic dispatch vtable cost |

---

**This architecture demonstrates professional systems design with emphasis on performance, reliability, and extensibility across a complex code analysis platform.**

Last updated: 2025-08-20
