# Neural Semantic Search

Neural embeddings enable true semantic code search - finding functionally similar code even when variable names, comments, and structure differ. This is powered by code-specialized embedding models.

## Supported Backends

| Backend | Flag | Models | Best For |
|---------|------|--------|----------|
| Voyage AI | `--neural-backend api` | `voyage-code-2`, `voyage-code-3` | Code-specific embeddings, best accuracy |
| OpenAI | `--neural-backend api` | `text-embedding-3-small`, `text-embedding-3-large` | General embeddings, wide availability |
| Custom/Self-hosted | `--neural-backend api` | Any compatible | Local deployment, custom models (use `EMBEDDING_SERVER_ENDPOINT`) |
| ONNX | `--neural-backend onnx` | Local models | Offline usage, no API costs |

## Quick Setup

The easiest way to set up neural embeddings is with the interactive wizard:

```bash
narsil-mcp config init --neural
```

The wizard will:
- Detect your editor (Claude Desktop, Claude Code, Zed, VS Code, JetBrains)
- Prompt for your API provider (Voyage AI, OpenAI, or custom)
- Validate your API key
- Automatically add it to your editor's MCP config

## Manual Setup

### Using Voyage AI (recommended for code)

```bash
export VOYAGE_API_KEY="your-key-here"
narsil-mcp --repos ~/project --neural --neural-backend api --neural-model voyage-code-2
```

### Using OpenAI

```bash
export OPENAI_API_KEY="your-key-here"
narsil-mcp --repos ~/project --neural --neural-backend api --neural-model text-embedding-3-small
```

### Using Custom/Self-hosted Embedding Server

```bash
export EMBEDDING_SERVER_ENDPOINT="http://localhost:8080/v1/embeddings"
export EMBEDDING_API_KEY="your-optional-api-key"  # Optional, depends on your server
narsil-mcp --repos ~/project --neural --neural-backend api --neural-model custom-model
```

### Using Local ONNX Model

```bash
# Build with ONNX support
cargo build --release --features neural-onnx

# No API key needed
narsil-mcp --repos ~/project --neural --neural-backend onnx
```

## Security Notes for Custom Endpoints

- Use HTTPS for production endpoints to prevent credential exposure
- HTTP is allowed for local development (localhost/private IPs) but will generate warnings
- The endpoint must use `http://` or `https://` scheme - other protocols are rejected for security
- API keys are optional for custom endpoints if your server doesn't require authentication

## Environment Variables

| Variable | Description |
|----------|-------------|
| `EMBEDDING_API_KEY` | Generic API key for any provider |
| `VOYAGE_API_KEY` | Voyage AI specific API key |
| `OPENAI_API_KEY` | OpenAI specific API key |
| `EMBEDDING_SERVER_ENDPOINT` | Custom embedding API endpoint URL |

## Use Cases

- **Semantic clone detection**: Find copy-pasted code that was renamed/refactored
- **Similar function search**: "Find functions that do pagination" even if named differently
- **Code deduplication**: Identify candidates for extracting shared utilities
- **Learning from examples**: Find similar patterns to code you're working with

## Available Tools

| Tool | Description |
|------|-------------|
| `neural_search` | Semantic search using neural embeddings |
| `find_semantic_clones` | Find Type-3/4 semantic clones of a function |
| `get_neural_stats` | Neural embedding index statistics |

## Example Queries

```
# These find similar code even with different naming:
neural_search("function that validates email addresses")
neural_search("error handling with retry logic")
find_semantic_clones("my_function")  # Find Type-3/4 clones
```

## Troubleshooting

### API Errors

```bash
# Check your API key is set
echo $VOYAGE_API_KEY  # or $OPENAI_API_KEY

# Common issue: wrong key format
export VOYAGE_API_KEY="pa-..."  # Voyage keys start with "pa-"
export OPENAI_API_KEY="sk-..."  # OpenAI keys start with "sk-"
```

### Slow Embedding Generation

Neural embeddings require API calls which add latency. For large codebases:
- Use `--persist` to cache embeddings
- Consider using the `balanced` or `minimal` preset to disable neural search for general use
- Use neural tools explicitly when you need semantic similarity

## Feature Builds

```bash
# Standard build (no neural features)
cargo build --release

# With TF-IDF similarity (no API needed)
cargo build --release --features neural

# With ONNX model support (local inference)
cargo build --release --features neural-onnx
```

| Feature | Description | Size |
|---------|-------------|------|
| `neural` | TF-IDF vector search, API embeddings | ~32MB |
| `neural-onnx` | Local ONNX model inference | ~50MB |
