[Back to README](../README.md) · [Installation Guide →](INSTALL.md)

# Getting Started

## Installation

### Via Package Managers (Recommended)

**macOS / Linux (Homebrew):**
```bash
brew tap postrv/narsil
brew install narsil-mcp
```

**Windows (Scoop):**
```powershell
scoop bucket add narsil https://github.com/postrv/scoop-narsil
scoop install narsil-mcp
```

**Rust/Cargo (all platforms):**
```bash
cargo install narsil-mcp
```

**Node.js/npm (all platforms):**
```bash
npm install -g narsil-mcp
# or
yarn global add narsil-mcp
# or
pnpm add -g narsil-mcp
```

**Nix:**
```bash
# Run directly without installing
nix run github:postrv/narsil-mcp -- --repos ./my-project

# Install to profile
nix profile install github:postrv/narsil-mcp

# With web visualization frontend
nix profile install github:postrv/narsil-mcp#with-frontend

# Development shell
nix develop github:postrv/narsil-mcp
```

### One-Click Install Script

**macOS / Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/postrv/narsil-mcp/main/install.sh | bash
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/postrv/narsil-mcp/main/install.ps1 | iex
```

**Windows (Git Bash / MSYS2):**
```bash
curl -fsSL https://raw.githubusercontent.com/postrv/narsil-mcp/main/install.sh | bash
```

> **Note for Windows users:** The PowerShell installer provides better error messages and native Windows integration. It will automatically configure your PATH and check for required build tools if building from source.

### From Source

**Prerequisites:**
- Rust 1.70 or later
- On Windows: [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022) with "Desktop development with C++"

```bash
# Clone and build
git clone git@github.com:postrv/narsil-mcp.git
cd narsil-mcp
cargo build --release

# Binary will be at:
# - macOS/Linux: target/release/narsil-mcp
# - Windows: target/release/narsil-mcp.exe
```

### Feature Builds

narsil-mcp supports different feature sets for different use cases:

```bash
# Default build - native MCP server (~30MB)
cargo build --release

# With RDF knowledge graph and CCG tools (~35MB) - SPARQL queries, Code Context Graph
cargo build --release --features graph

# With neural vector search (~32MB) - adds TF-IDF similarity
cargo build --release --features neural

# With ONNX model support (~50MB) - adds local neural embeddings
cargo build --release --features neural-onnx

# With embedded visualization frontend (~31MB)
cargo build --release --features frontend

# Full-featured build with graph + frontend (~40MB)
cargo build --release --features graph,frontend

# For browser/WASM usage
cargo build --release --target wasm32-unknown-unknown --features wasm
```

| Feature | Description | Size |
|---------|-------------|------|
| `native` (default) | Full MCP server with all tools | ~30MB |
| `graph` | + RDF knowledge graph, SPARQL, CCG tools | ~35MB |
| `frontend` | + Embedded visualization web UI | ~31MB |
| `neural` | + TF-IDF vector search, API embeddings | ~32MB |
| `neural-onnx` | + Local ONNX model inference | ~50MB |
| `wasm` | Browser build (no file system, git) | ~3MB |

> **Important:** The `--graph` CLI flag requires the binary to be built with `--features graph`. If you pass `--graph` to a binary built without this feature, you'll see a warning and SPARQL/CCG tools won't be available. See [Troubleshooting](#graph-feature-not-working) below.

> **For detailed installation instructions, troubleshooting, and platform-specific guides**, see [docs/INSTALL.md](INSTALL.md).

## Usage

### Basic Usage

**macOS / Linux:**
```bash
# Index a single repository
narsil-mcp --repos /path/to/your/project

# Index multiple repositories
narsil-mcp --repos ~/projects/project1 --repos ~/projects/project2

# Enable verbose logging
narsil-mcp --repos /path/to/project --verbose

# Force re-index on startup
narsil-mcp --repos /path/to/project --reindex
```

**Windows (PowerShell / CMD):**
```powershell
# Index a single repository
narsil-mcp --repos C:\Users\YourName\Projects\my-project

# Index multiple repositories
narsil-mcp --repos C:\Projects\project1 --repos C:\Projects\project2

# Enable verbose logging
narsil-mcp --repos C:\Projects\my-project --verbose

# Force re-index on startup
narsil-mcp --repos C:\Projects\my-project --reindex
```

### Full Feature Set

```bash
narsil-mcp \
  --repos ~/projects/my-app \
  --git \           # Enable git blame, history, contributors
  --call-graph \    # Enable function call analysis
  --persist \       # Cache symbols on disk: a restart skips re-parsing unchanged files
  --watch \         # Auto-reindex on file changes
  --lsp \           # Enable LSP for hover, go-to-definition
  --streaming \     # Stream large result sets
  --remote \        # Enable GitHub remote repo support
  --neural \        # Enable neural semantic embeddings
  --neural-backend api \  # Backend: "api" (Voyage/OpenAI) or "onnx"
  --neural-model voyage-code-2 \  # Model to use
  --neural-dimension 3072 \  # Override embedding dimensions (auto-detected per model)
  --graph           # Enable SPARQL/RDF knowledge graph and CCG tools (requires --features graph build)
```

> **Note about `--graph`:** This flag enables SPARQL queries and Code Context Graph (CCG) tools, but **only if the binary was built with `--features graph`**. The default binary does not include this feature. If you need SPARQL/CCG capabilities, build from source with:
> ```bash
> cargo build --release --features graph
> ```
> If you pass `--graph` to a binary without the feature, you'll see a warning at startup and the server will continue without SPARQL/CCG tools.

**Note:** Neural embeddings require an API key (or custom endpoint). The easiest way to set this up is with the interactive wizard:

```bash
# Run the neural API key setup wizard
narsil-mcp config init --neural
```

The wizard will:
- Detect your editor (Claude Desktop, Claude Code, Zed, VS Code, JetBrains)
- Prompt for your API provider (Voyage AI, OpenAI, or custom)
- Validate your API key
- Automatically add it to your editor's MCP config

Alternatively, you can manually set one of these environment variables:
- `EMBEDDING_API_KEY` - Generic API key for any provider
- `VOYAGE_API_KEY` - Voyage AI specific API key
- `OPENAI_API_KEY` - OpenAI specific API key
- `EMBEDDING_SERVER_ENDPOINT` - Custom embedding API endpoint URL (optional, allows using self-hosted models)

## See Also

- [Installation Guide](INSTALL.md) — platform-specific install details
- [Configuration](configuration.md) — flags, presets and config files
- [Integrations](integrations.md) — connect an MCP client
