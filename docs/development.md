[← Performance](performance.md) · [Back to README](../README.md) · [Troubleshooting →](troubleshooting.md)

# Development

## Building and testing

```bash
# Run the full test suite
cargo test

# Run benchmarks (criterion.rs)
cargo bench

# Run with debug logging
RUST_LOG=debug cargo run -- --repos ./test-fixtures

# Format code
cargo fmt

# Lint
cargo clippy

# Test with MCP Inspector
npx @modelcontextprotocol/inspector ./target/release/narsil-mcp --repos ./path/to/repo
```

## Make targets

`make help` lists everything. The ones used day to day:

| Command | What it does |
|---------|--------------|
| `make ci` | fmt check, clippy (`-D warnings`), nextest, doc tests, `cargo deny` — must be green before a commit |
| `make rs-check` | fast compile check |
| `make rs-build` | release build with `RUST_FEATURES` (default `graph,neural`) |
| `make rs-run REPOS=/path` | run the server on a repository |
| `make rs-fmt-fix` | format the code |
| `make frontend-build` / `make rs-build-frontend` | build the UI / release binary with the embedded UI |
| `make eval-onec ONEC_REPO=/path/CommonModules` | search quality and latency on a 1C dump (`eval/ragdiag`) |
| `make target-size` / `make clean` | watch and reclaim `target/` (it has grown past 140 GB) |

## See Also

- [Architecture](architecture.md) — code structure
- [Getting Started](getting-started.md) — building from source
