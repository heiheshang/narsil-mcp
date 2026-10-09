[← Configuration](configuration.md) · [Back to README](../README.md) · [Available Tools →](tools.md)

# Integrations

### MCP Configuration

Add narsil-mcp to your AI assistant by creating a configuration file. Here are the recommended setups:

---

**Claude Code** (`.mcp.json` in project root - Recommended):

Create `.mcp.json` in your project directory for per-project configuration:
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

Then start Claude Code in your project:
```bash
cd /path/to/project
claude
```

Using `.` for `--repos` automatically indexes the current directory. Claude now has access to 90 code intelligence tools.

> **Tip**: Add `--persist --index-path .claude/cache` for faster startup on subsequent
> runs — the cached symbols let a restart skip tree-sitter parsing for files that have
> not changed. (Skipped when `--call-graph` is on: the call graph needs the parse trees.)

For global configuration, edit `~/.claude/settings.json` instead. See [Claude Code Integration](playbooks/integrations/claude-code.md) for advanced setups.

---

**Cursor** (`.cursor/mcp.json`):
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

---

**VS Code + GitHub Copilot** (`.vscode/mcp.json`):
```json
{
  "servers": {
    "narsil-mcp": {
      "command": "narsil-mcp",
      "args": ["--repos", ".", "--git", "--call-graph"]
    }
  }
}
```

> **Note for Copilot Enterprise**: MCP support requires VS Code 1.102+ and must be enabled by your organization administrator.

---

**Claude Desktop** (`claude_desktop_config.json`):
```json
{
  "mcpServers": {
    "narsil-mcp": {
      "command": "narsil-mcp",
      "args": ["--repos", "/path/to/your/projects", "--git"]
    }
  }
}
```

---

**Zed** (`settings.json` → Context Servers):
```json
{
  "context_servers": {
    "narsil-mcp": {
      "command": "narsil-mcp",
      "args": ["--repos", ".", "--git"]
    }
  }
}
```

> **Note for Zed**: narsil-mcp starts immediately and indexes in the background, preventing initialization timeouts.

---

### Claude Code Plugin

For **Claude Code** users, we provide a plugin with slash commands and a skill for effective tool usage.

**Install via Marketplace (Recommended):**
```shell
# Add the narsil-mcp marketplace
/plugin marketplace add postrv/narsil-mcp

# Install the plugin
/plugin install narsil@narsil-mcp
```

**Or install directly from GitHub:**
```shell
/plugin install github:postrv/narsil-mcp/narsil-plugin
```

**What's included:**

| Component | Description |
|-----------|-------------|
| `/narsil:security-scan` | Run comprehensive security audits |
| `/narsil:explore` | Explore unfamiliar codebases |
| `/narsil:analyze-function` | Deep dive on specific functions |
| `/narsil:find-feature` | Find where features are implemented |
| `/narsil:supply-chain` | Analyze supply chain security |
| **Skill** | Guides Claude on using 90 tools effectively |
| **MCP Config** | Auto-starts narsil-mcp with sensible defaults |

See [narsil-plugin/README.md](../narsil-plugin/README.md) for full documentation.

### Forgemax Integration (Experimental)

For large-scale agentic workflows, narsil-mcp can be used through [Forgemax](https://github.com/postrv/forgemax) — a Code Mode MCP gateway that collapses all 90 tools into just 2 (`search` + `execute`), reducing tool schema overhead from ~12,000 tokens to ~1,000.

```bash
# Install Forgemax
cargo install forgemax

# Run narsil-mcp through Forgemax (uses forge.toml in repo root)
forgemax
```

The included `forge.toml` configures narsil-mcp with sensible defaults:

```toml
[servers.narsil]
command = "narsil-mcp"
args = ["--repos", ".", "--git", "--call-graph", "--persist", "--watch"]
transport = "stdio"

[sandbox]
timeout_secs = 10
max_heap_mb = 64
max_concurrent = 8
```

The LLM writes JavaScript that calls through typed proxy objects inside a sandboxed V8 isolate — credentials, file paths, and internal state never leave the host. This approach is particularly useful when working with multiple MCP servers simultaneously, as it keeps the total tool context small and predictable.

### Ralph Automation Integration

[Ralph](https://github.com/postrv/ralphing-la-vida-locum) is a Claude Code automation suite for autonomous code development. When narsil-mcp is available, Ralph gains enhanced code intelligence capabilities:

| Feature | Without narsil-mcp | With narsil-mcp |
|---------|-------------------|-----------------|
| Security scanning | Basic (clippy) | OWASP/CWE vulnerability detection |
| Code understanding | File-based | Call graphs, symbol references |
| Architecture analysis | Manual | CCG L0/L1/L2 automatic layers |
| Dependency analysis | cargo tree | Import graphs, circular detection |

**Setup:**
```bash
# Install narsil-mcp (Ralph auto-detects it)
cargo install narsil-mcp

# Ralph's quality gates use these tools:
narsil-mcp scan_security --repo <name>
narsil-mcp check_type_errors --repo <name> --path src
narsil-mcp find_injection_vulnerabilities --repo <name>
```

Ralph gracefully degrades when narsil-mcp is unavailable - all core automation features work without it.

> **Documentation:** See [Ralph README](https://github.com/postrv/ralphing-la-vida-locum) for full integration details.

### Playbooks & Tutorials

See **[docs/playbooks](playbooks/)** for practical usage guides:

| Guide | Description |
|-------|-------------|
| [Getting Started](playbooks/getting-started.md) | Quick setup and first tool calls |
| [Understand a Codebase](playbooks/workflows/understand-codebase.md) | Explore unfamiliar projects |
| [Fix a Bug](playbooks/workflows/fix-a-bug.md) | Debug with call graphs and taint analysis |
| [Security Audit](playbooks/workflows/security-audit.md) | Find vulnerabilities with OWASP/CWE scanning |
| [Code Review](playbooks/workflows/code-review.md) | Review changes effectively |

Each playbook shows the exact tool chains Claude uses to answer your questions.

## See Also

- [Configuration](configuration.md) — presets per editor
- [Protocol and HTTP API](api.md) — HTTP transport and sessions
- [Available Tools](tools.md) — tool reference
