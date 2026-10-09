[← Integrations](integrations.md) · [Back to README](../README.md) · [Supported Languages →](languages.md)

# Available Tools

## Tools by category (90)

### Repository & File Management

| Tool | Description |
|------|-------------|
| `list_repos` | List all indexed repositories with metadata |
| `get_project_structure` | Get directory tree with file icons and sizes |
| `get_file` | Get file contents with optional line range |
| `get_excerpt` | Extract code around specific lines with context |
| `reindex` | Trigger re-indexing of repositories |
| `discover_repos` | Auto-discover repositories in a directory |
| `validate_repo` | Check if path is a valid repository |
| `get_index_status` | Show index stats and enabled features |

### Symbol Search & Navigation

| Tool | Description |
|------|-------------|
| `find_symbols` | Find structs, classes, functions by type/pattern |
| `get_symbol_definition` | Get symbol source with surrounding context |
| `find_references` | Find all references to a symbol |
| `get_dependencies` | Analyze imports and dependents |
| `workspace_symbol_search` | Fuzzy search symbols across workspace |
| `find_symbol_usages` | Cross-file symbol usage with imports |
| `get_export_map` | Get exported symbols from a file/module |

### Code Search

| Tool | Description |
|------|-------------|
| `search_code` | Keyword search with relevance ranking |
| `semantic_search` | BM25-ranked semantic search (pass `repo` to scope to one repository) |
| `hybrid_search` | Rank fusion (RRF) of BM25 over symbol cards, TF-IDF over signatures and token overlap with function bodies |
| `search_chunks` | Search over AST-aware code chunks |
| `find_similar_code` | Find code similar to a snippet (TF-IDF) |
| `find_similar_to_symbol` | Find code similar to a symbol |

### AST-Aware Chunking

| Tool | Description |
|------|-------------|
| `get_chunks` | Get AST-aware chunks for a file |
| `get_chunk_stats` | Statistics about code chunks |
| `get_embedding_stats` | Embedding index statistics |

### Neural Semantic Search (requires `--neural`)

| Tool | Description |
|------|-------------|
| `neural_search` | Semantic search using neural embeddings (finds similar code even with different names; pass `repo` to scope to one repository) |
| `find_semantic_clones` | Find Type-3/4 semantic clones of a function |
| `get_neural_stats` | Neural embedding index statistics |

### Call Graph Analysis (requires `--call-graph`)

| Tool | Description |
|------|-------------|
| `get_call_graph` | Get call graph for repository/function |
| `get_callers` | Find functions that call a function |
| `get_callees` | Find functions called by a function |
| `find_call_path` | Find path between two functions |
| `get_complexity` | Get cyclomatic/cognitive complexity |
| `get_function_hotspots` | Find highly connected functions |

Function arguments accept a bare name (`Handle`), a qualified method
(`Server.Handle`, `Server::Handle`) or a graph key. Methods are keyed
`file::Type::name` (`internal/api/api.go::Server::Handle`), free functions
`file::name`, so two same-named methods on different types are distinct nodes.

**Resolution limits.** A call is still resolved by name: an unqualified
`x.Handle()` cannot be tied to the type of `x` without type inference, so it
is attributed to a receiver type named by the qualifier (`Server::Handle()`),
else to a same-named function in the caller's own file, else to a module
matching the qualifier, else — deterministically — to the first candidate
alphabetically. When a name is ambiguous, `get_callers` / `get_callees` say
so and list the other candidates with their types instead of silently picking
one; query by `Type.name` to pin down the one you mean.

Every edge records what it was matched on, and the listings mark the weak
ones: `name match only` (several namesakes, nothing to choose between them),
`same-file match`, `not in graph` (third-party or standard library), plus a
count of each below the list.

### Control Flow Analysis

| Tool | Description |
|------|-------------|
| `get_control_flow` | Get CFG showing basic blocks and branches |
| `find_dead_code` | Find unreachable code blocks |

### Data Flow Analysis

| Tool | Description |
|------|-------------|
| `get_data_flow` | Variable definitions and uses |
| `get_reaching_definitions` | Which assignments reach each point |
| `find_uninitialized` | Variables used before initialization |
| `find_dead_stores` | Assignments that are never read |

### Type Inference (Python/JavaScript/TypeScript)

| Tool | Description |
|------|-------------|
| `infer_types` | Infer types for variables in a function without external type checkers |
| `check_type_errors` | Find potential type errors without running mypy/tsc |
| `get_typed_taint_flow` | Enhanced taint analysis combining data flow with type inference |

### Import/Dependency Graph

| Tool | Description |
|------|-------------|
| `get_import_graph` | Build and analyze import graph |
| `find_circular_imports` | Detect circular dependencies |
| `get_incremental_status` | Merkle tree and change statistics |

### Security Analysis - Taint Tracking

| Tool | Description |
|------|-------------|
| `find_injection_vulnerabilities` | Find SQL injection, XSS, command injection, path traversal |
| `trace_taint` | Trace tainted data flow from a source |
| `get_taint_sources` | List taint sources (user input, files, network) |
| `get_security_summary` | Comprehensive security risk assessment |

### Security Analysis - Rules Engine

| Tool | Description |
|------|-------------|
| `scan_security` | Scan with security rules (OWASP, CWE, crypto, secrets) |
| `check_owasp_top10` | Scan for OWASP Top 10 2021 vulnerabilities |
| `check_cwe_top25` | Scan for CWE Top 25 weaknesses |
| `explain_vulnerability` | Get detailed vulnerability explanation |
| `suggest_fix` | Get remediation suggestions for findings |

### Supply Chain Security

| Tool | Description |
|------|-------------|
| `generate_sbom` | Generate SBOM (CycloneDX/SPDX/JSON) |
| `check_dependencies` | Check for known vulnerabilities (OSV database) |
| `check_licenses` | Analyze licenses for compliance issues |
| `find_upgrade_path` | Find safe upgrade paths for vulnerable deps |

### Git Integration (requires `--git`)

| Tool | Description |
|------|-------------|
| `get_blame` | Git blame for file |
| `get_file_history` | Commit history for file |
| `get_recent_changes` | Recent commits in repository |
| `get_hotspots` | Files with high churn and complexity |
| `get_contributors` | Repository/file contributors |
| `get_commit_diff` | Diff for specific commit |
| `get_symbol_history` | Commits that changed a symbol |
| `get_branch_info` | Current branch and status |
| `get_modified_files` | Working tree changes |

### LSP Integration (requires `--lsp`)

| Tool | Description |
|------|-------------|
| `get_hover_info` | Type info and documentation |
| `get_type_info` | Precise type information |
| `go_to_definition` | Find definition location |

### Remote Repository Support (requires `--remote`)

| Tool | Description |
|------|-------------|
| `add_remote_repo` | Clone and index GitHub repository |
| `list_remote_files` | List files via GitHub API |
| `get_remote_file` | Fetch file via GitHub API |

### Metrics

| Tool | Description |
|------|-------------|
| `get_metrics` | Performance stats and timing |

### SPARQL / Knowledge Graph (requires `--graph`)

| Tool | Description |
|------|-------------|
| `sparql_query` | Execute SPARQL query against RDF knowledge graph |
| `list_sparql_templates` | List available SPARQL query templates |
| `run_sparql_template` | Execute predefined SPARQL template with parameters |

### Code Context Graph (CCG) (requires `--graph`)

CCG provides standardized, AI-consumable representations of codebases in tiered layers.

| Tool | Description |
|------|-------------|
| `get_ccg_manifest` | Layer 0 manifest (~1-2KB JSON-LD) - repo identity, counts |
| `export_ccg_manifest` | Export Layer 0 manifest to file |
| `export_ccg_architecture` | Layer 1 architecture (~10-50KB JSON-LD) - modules, API |
| `export_ccg_index` | Layer 2 symbol index (~100-500KB N-Quads gzipped) |
| `export_ccg_full` | Layer 3 full detail (~1-20MB N-Quads gzipped) |
| `export_ccg` | Export all CCG layers as a bundle |
| `query_ccg` | Query CCG using SPARQL |
| `get_ccg_acl` | Generate WebACL access control for CCG layers |
| `get_ccg_access_info` | Get CCG access tier information |
| `import_ccg` | Import CCG layer from URL or file |
| `import_ccg_from_registry` | Import CCG from codecontextgraph.com registry |

### Type Inference

Built-in type inference for Python, JavaScript, and TypeScript - no mypy or tsc required.

| Tool | Description |
|------|-------------|
| `infer_types` | Get inferred types for all variables in a function |
| `check_type_errors` | Find potential type mismatches |
| `get_typed_taint_flow` | Enhanced security analysis with type info |

```python
def process(data):
    result = data.split(",")  # result: list[str]
    count = len(result)       # count: int
    return count * 2          # returns: int
```

## See Also

- [Supported Languages](languages.md) — what each language extracts
- [Configuration](configuration.md) — enabling tools with flags and presets
- [Security Rules](security.md) — rules behind the security tools
