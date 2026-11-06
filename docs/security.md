[← Protocol and HTTP API](api.md) · [Back to README](../README.md) · [Architecture →](architecture.md)

# Security Rules

## Rule sets

narsil-mcp includes built-in security rules in `rules/`:

**Core Rulesets:**
- **`owasp-top10.yaml`** - OWASP Top 10 2021 vulnerability patterns
- **`cwe-top25.yaml`** - CWE Top 25 Most Dangerous Weaknesses
- **`crypto.yaml`** - Cryptographic issues (weak algorithms, hardcoded keys)
- **`secrets.yaml`** - Secret detection (API keys, passwords, tokens)

**Language-Specific Rules:**
- **`rust.yaml`** - Rust security patterns (unsafe transmute, FFI boundaries, command injection, TOCTOU)
- **`elixir.yaml`** - Elixir/BEAM patterns (atom exhaustion, binary_to_term, Code.eval, Ecto SQL injection)
- **`go.yaml`** - Go security patterns (SQL injection, TLS, command injection)
- **`java.yaml`** - Java vulnerabilities (XXE, deserialization, LDAP injection)
- **`csharp.yaml`** - C# security issues (deserialization, XSS, path traversal)
- **`kotlin.yaml`** - Kotlin/Android patterns (WebView, intents, secrets)
- **`bash.yaml`** - Shell script vulnerabilities (command injection, eval)

**Infrastructure & Configuration:**
- **`iac.yaml`** - Infrastructure as Code (Terraform, CloudFormation, Kubernetes)
- **`config.yaml`** - Configuration file security (hardcoded credentials, insecure settings)

Custom rules can be loaded with `scan_security --ruleset /path/to/rules.yaml`.

## See Also

- [Available Tools](tools.md) — security tools
- [Architecture](architecture.md) — taint analysis design
