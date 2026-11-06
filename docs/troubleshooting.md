[← Development](development.md) · [Back to README](../README.md) · [Roadmap →](roadmap.md)

# Troubleshooting

## Common problems

### Tree-sitter Build Errors

If you see errors about missing C compilers or tree-sitter during build:

```bash
# macOS
xcode-select --install

# Ubuntu/Debian
sudo apt install build-essential

# For WASM builds
brew install emscripten  # macOS
```

### Neural Search API Errors

```bash
# Check your API key is set
echo $VOYAGE_API_KEY  # or $OPENAI_API_KEY

# Common issue: wrong key format
export VOYAGE_API_KEY="pa-..."  # Voyage keys start with "pa-"
export OPENAI_API_KEY="sk-..."  # OpenAI keys start with "sk-"
```

### Index Not Finding Files

```bash
# Check .gitignore isn't excluding files
narsil-mcp --repos /path --verbose  # Shows skipped files

# Force reindex
narsil-mcp --repos /path --reindex
```

### Memory Issues with Large Repos

```bash
# For very large repos (>50K files), increase stack size
RUST_MIN_STACK=8388608 narsil-mcp --repos /path/to/huge-repo

# Or index specific subdirectories
narsil-mcp --repos /path/to/repo/src --repos /path/to/repo/lib
```

### Graph Feature Not Working

If you pass `--graph` and see a warning like:
```
WARN: --graph flag was passed but the binary was built without the 'graph' feature.
SPARQL and CCG tools will not be available.
```

This means you're using a binary that wasn't compiled with the `graph` feature. To fix:

```bash
# Build from source with the graph feature
cargo build --release --features graph

# Or with multiple features
cargo build --release --features graph,frontend

# Then run with --graph
./target/release/narsil-mcp --repos ~/project --graph
```

**Why is this a separate feature?** The `graph` feature adds the Oxigraph RDF database (~5MB additional binary size) which isn't needed for most use cases. It's kept optional to keep the default binary smaller.

**How to check if graph is enabled:** Look at the startup logs:
- `graph=true` means the feature is compiled in AND enabled
- `graph=false` means either the feature isn't compiled, OR `--graph` wasn't passed

## See Also

- [Installation Guide](INSTALL.md) — installation details
- [Configuration](configuration.md) — flags and config
- [Neural Search](neural-search.md) — neural setup
