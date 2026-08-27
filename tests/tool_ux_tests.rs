//! Tool-contract UX tests: argument aliases, explicit line ranges,
//! result caps and loud failures instead of silently-empty successes.

use anyhow::Result;
use narsil_mcp::index::{CodeIntelEngine, EngineOptions};
use narsil_mcp::tool_handlers::ToolRegistry;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

struct TestRepo {
    dir: TempDir,
}

impl TestRepo {
    fn new() -> Result<Self> {
        Ok(Self {
            dir: TempDir::new()?,
        })
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn name(&self) -> String {
        self.dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    fn add_file(&self, name: &str, content: &str) -> Result<()> {
        let path = self.dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }
}

async fn engine_with(
    repo: &TestRepo,
    options: EngineOptions,
) -> Result<(CodeIntelEngine, TempDir)> {
    let index_dir = TempDir::new()?;
    let engine = CodeIntelEngine::with_options(
        index_dir.path().to_path_buf(),
        vec![repo.path().to_path_buf()],
        options,
    )
    .await?;
    engine.complete_initialization().await?;
    Ok((engine, index_dir))
}

async fn engine_for(repo: &TestRepo) -> Result<(CodeIntelEngine, TempDir)> {
    engine_with(
        repo,
        EngineOptions {
            git_enabled: false,
            call_graph_enabled: false,
            persist_enabled: false,
            watch_enabled: false,
            ..Default::default()
        },
    )
    .await
}

#[tokio::test]
async fn get_excerpt_accepts_start_end_line_range() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("notes.md", "# Title\nalpha\nbeta\ngamma\ndelta\nepsilon\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "get_excerpt",
            &engine,
            json!({"repo": repo.name(), "path": "notes.md", "start_line": 3, "end_line": 5}),
        )
        .await?;

    assert!(!out.contains("0 excerpt"), "range gave no excerpts:\n{out}");
    for expected in ["beta", "gamma", "delta"] {
        assert!(out.contains(expected), "missing '{expected}':\n{out}");
    }
    // Explicit range means exactly those lines: no context padding.
    assert!(!out.contains("# Title"), "leaked line before range:\n{out}");
    assert!(!out.contains("epsilon"), "leaked line after range:\n{out}");
    Ok(())
}

#[tokio::test]
async fn get_excerpt_range_larger_than_default_max_is_not_clamped() -> Result<()> {
    let repo = TestRepo::new()?;
    let body: String = (1..=80).map(|i| format!("row number {i}\n")).collect();
    repo.add_file("big.md", &body)?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "get_excerpt",
            &engine,
            json!({"repo": repo.name(), "path": "big.md", "start_line": 1, "end_line": 70}),
        )
        .await?;

    assert!(
        out.contains("row number 1\n") || out.contains("row number 1"),
        "{out}"
    );
    assert!(out.contains("row number 70"), "range was clamped:\n{out}");
    Ok(())
}

#[tokio::test]
async fn get_excerpt_without_lines_errors_instead_of_empty_success() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("notes.md", "alpha\nbeta\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let err = registry
        .dispatch(
            "get_excerpt",
            &engine,
            json!({"repo": repo.name(), "path": "notes.md"}),
        )
        .await
        .expect_err("missing lines must be an error, not an empty result");
    assert!(err.to_string().contains("lines"), "unhelpful error: {err}");
    Ok(())
}

#[tokio::test]
async fn get_excerpt_out_of_range_anchor_does_not_panic() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("small.md", "alpha\nbeta\ngamma\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "get_excerpt",
            &engine,
            json!({"repo": repo.name(), "path": "small.md", "lines": [5000]}),
        )
        .await?;
    // Clamped to the last real line — something must come back.
    assert!(out.contains("gamma"), "clamped anchor gave nothing:\n{out}");
    Ok(())
}

#[tokio::test]
async fn search_code_path_alias_with_bare_filename() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("docs/target.md", "the marker SO-9999 lives here\n")?;
    repo.add_file("docs/other.md", "the marker SO-9999 also here\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "search_code",
            &engine,
            json!({"repo": repo.name(), "query": "SO-9999", "path": "target.md"}),
        )
        .await?;

    assert!(
        out.contains("target.md"),
        "path filter found nothing:\n{out}"
    );
    assert!(
        !out.contains("other.md"),
        "path filter leaked other files:\n{out}"
    );
    Ok(())
}

#[tokio::test]
async fn search_code_invalid_glob_is_an_error() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("a.md", "needle\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let err = registry
        .dispatch(
            "search_code",
            &engine,
            json!({"repo": repo.name(), "query": "needle", "file_pattern": "[oops"}),
        )
        .await
        .expect_err("invalid glob must not be silently dropped");
    assert!(err.to_string().contains("glob"), "unhelpful error: {err}");
    Ok(())
}

#[tokio::test]
async fn find_symbols_query_alias_ranks_exact_match_first() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file(
        "src/lib.rs",
        r#"
pub struct Target { pub a: u32 }
pub struct TargetHelper { pub b: u32 }
pub struct TargetHelperFactory { pub c: u32 }
pub fn make_target() {}
"#,
    )?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    // `query` alias + limit 1: only the exact match may survive.
    let out = registry
        .dispatch(
            "find_symbols",
            &engine,
            json!({"repo": repo.name(), "query": "Target", "limit": 1}),
        )
        .await?;

    assert!(out.contains("**Target**"), "exact match not first:\n{out}");
    assert!(
        !out.contains("TargetHelper"),
        "limit/ranking not applied:\n{out}"
    );
    assert!(
        out.contains("showing first 1"),
        "truncation must be visible:\n{out}"
    );
    Ok(())
}

#[tokio::test]
async fn find_symbols_rejects_empty_pattern_and_bad_type() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("src/lib.rs", "pub fn f() {}\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    registry
        .dispatch(
            "find_symbols",
            &engine,
            json!({"repo": repo.name(), "pattern": "  "}),
        )
        .await
        .expect_err("empty pattern must not return the whole index");

    registry
        .dispatch(
            "find_symbols",
            &engine,
            json!({"repo": repo.name(), "symbol_type": "klass"}),
        )
        .await
        .expect_err("unknown symbol_type must not silently widen the filter");
    Ok(())
}

/// Guard against reintroducing double-encoded UTF-8 literals (mojibake like
/// "â”‚" instead of "│") — the corruption that broke get_file's line gutter.
#[test]
fn no_mojibake_literals_in_source() {
    fn scan(dir: &Path, bad: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan(&path, bad);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (i, line) in content.lines().enumerate() {
                    let chars: Vec<char> = line.chars().collect();
                    for w in chars.windows(2) {
                        let lead = ('\u{00c0}'..='\u{00ff}').contains(&w[0]);
                        let cont = ('\u{0080}'..='\u{00bf}').contains(&w[1])
                            || ('\u{2000}'..='\u{20ff}').contains(&w[1]);
                        if lead && cont {
                            bad.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                        }
                    }
                }
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut bad = Vec::new();
    scan(&src, &mut bad);
    assert!(
        bad.is_empty(),
        "double-encoded UTF-8 literals found (mojibake):\n{}",
        bad.join("\n")
    );
}

#[tokio::test]
async fn dispatch_rejects_unknown_argument() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("a.md", "needle\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let err = registry
        .dispatch(
            "search_code",
            &engine,
            json!({"repo": repo.name(), "query": "needle", "serach_pattern": "typo"}),
        )
        .await
        .expect_err("unknown argument key must be rejected, not silently dropped");
    let msg = err.to_string();
    assert!(msg.contains("serach_pattern"), "{msg}");
    assert!(
        msg.contains("Accepted parameters"),
        "error must list accepted params: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn dispatch_rejects_missing_required_argument() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("a.md", "alpha\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let err = registry
        .dispatch(
            "get_excerpt",
            &engine,
            json!({"repo": repo.name(), "start_line": 1}),
        )
        .await
        .expect_err("missing required 'path' must be rejected");
    assert!(err.to_string().contains("path"), "{err}");
    Ok(())
}

#[tokio::test]
async fn dispatch_alias_satisfies_required_argument() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("a.md", "alpha\nbeta\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    // 'file' is a declared alias for the required 'path'
    let out = registry
        .dispatch(
            "get_excerpt",
            &engine,
            json!({"repo": repo.name(), "file": "a.md", "start_line": 1, "end_line": 2}),
        )
        .await?;
    assert!(out.contains("alpha"), "{out}");
    Ok(())
}

#[test]
fn validate_modes_behave_as_documented() {
    use narsil_mcp::tool_handlers::{validate_tool_args, ArgValidationMode};

    let bad = json!({"query": "x", "bogus_key": 1});
    assert!(validate_tool_args("search_code", &bad, ArgValidationMode::Strict).is_err());
    assert!(validate_tool_args("search_code", &bad, ArgValidationMode::Warn).is_ok());
    assert!(validate_tool_args("search_code", &bad, ArgValidationMode::Off).is_ok());

    // Underscore-prefixed keys are reserved and never rejected
    let meta_key = json!({"query": "x", "_meta": {"traceparent": "t"}});
    assert!(validate_tool_args("search_code", &meta_key, ArgValidationMode::Strict).is_ok());

    // Tools without metadata are not validated
    let anything = json!({"whatever": true});
    assert!(validate_tool_args("no_such_tool", &anything, ArgValidationMode::Strict).is_ok());

    // Null arguments are fine when nothing is required
    assert!(validate_tool_args(
        "list_repos",
        &serde_json::Value::Null,
        ArgValidationMode::Strict
    )
    .is_ok());
}

/// Validation used to check names and `required` only, leaving the schema's
/// own `type` and `enum` unenforced. Handlers read arguments with
/// `as_u64`/`as_str`, which yield None on a mismatch and fall through to the
/// default — so `max_results="50"` silently returned 10 results and reported
/// success.
#[test]
fn validate_enforces_declared_types_and_enums() {
    use narsil_mcp::tool_handlers::{validate_tool_args, ArgValidationMode};

    let stringly_number = json!({"query": "x", "max_results": "50"});
    let err = validate_tool_args("search_code", &stringly_number, ArgValidationMode::Strict)
        .expect_err("a string where an integer is declared must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("max_results"), "{msg}");
    assert!(msg.contains("an integer"), "{msg}");

    // The correct call still passes, and a whole-valued float counts as an
    // integer — several clients send 50.0 for a schema that says 50.
    for value in [json!(50), json!(50.0)] {
        assert!(validate_tool_args(
            "search_code",
            &json!({"query": "x", "max_results": value}),
            ArgValidationMode::Strict
        )
        .is_ok());
    }

    // An out-of-range enum names the accepted values rather than falling back.
    let bad_enum = json!({"format": "yaml"});
    let err = validate_tool_args("get_metrics", &bad_enum, ArgValidationMode::Strict)
        .expect_err("a value outside the declared enum must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("markdown") && msg.contains("json"), "{msg}");

    assert!(validate_tool_args(
        "get_metrics",
        &json!({"format": "json"}),
        ArgValidationMode::Strict
    )
    .is_ok());

    // Warn mode still only logs, as for every other problem class.
    assert!(validate_tool_args("get_metrics", &bad_enum, ArgValidationMode::Warn).is_ok());
}

#[tokio::test]
async fn vendored_paths_excluded_from_index_by_default() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("src/lib.rs", "pub fn app_needle_fn() {}\n")?;
    repo.add_file("vendor/dep/lib.rs", "pub fn vendored_needle_fn() {}\n")?;
    repo.add_file("package-lock.json", "{\"needle_fn\": true}\n")?;
    let (engine, _idx) = engine_for(&repo).await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "search_code",
            &engine,
            json!({"repo": repo.name(), "query": "needle_fn"}),
        )
        .await?;
    assert!(out.contains("src/lib.rs"), "app code not indexed:\n{out}");
    assert!(
        !out.contains("vendor/"),
        "vendored file leaked into search:\n{out}"
    );
    assert!(
        !out.contains("package-lock.json"),
        "lockfile leaked into search:\n{out}"
    );

    let symbols = registry
        .dispatch(
            "find_symbols",
            &engine,
            json!({"repo": repo.name(), "pattern": "needle_fn"}),
        )
        .await?;
    assert!(symbols.contains("app_needle_fn"), "{symbols}");
    assert!(
        !symbols.contains("vendored_needle_fn"),
        "vendored symbol leaked:\n{symbols}"
    );
    Ok(())
}

#[tokio::test]
async fn exclude_negation_reincludes_vendored_paths() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("vendor/dep/lib.rs", "pub fn vendored_needle_fn() {}\n")?;
    let (engine, _idx) = engine_with(
        &repo,
        EngineOptions {
            index_exclude: vec!["!**/vendor/**".to_string()],
            ..Default::default()
        },
    )
    .await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "search_code",
            &engine,
            json!({"repo": repo.name(), "query": "vendored_needle_fn"}),
        )
        .await?;
    assert!(
        out.contains("vendor/dep/lib.rs"),
        "'!' pattern must re-include vendored paths:\n{out}"
    );
    Ok(())
}

#[tokio::test]
async fn custom_exclude_glob_is_applied() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("docs/generated.md", "gen_needle here\n")?;
    repo.add_file("README.md", "gen_needle here too\n")?;
    let (engine, _idx) = engine_with(
        &repo,
        EngineOptions {
            index_exclude: vec!["docs/**".to_string()],
            ..Default::default()
        },
    )
    .await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "search_code",
            &engine,
            json!({"repo": repo.name(), "query": "gen_needle"}),
        )
        .await?;
    assert!(out.contains("README.md"), "{out}");
    assert!(!out.contains("docs/"), "custom exclude ignored:\n{out}");
    Ok(())
}

#[tokio::test]
async fn oversized_files_skipped_from_index() -> Result<()> {
    let repo = TestRepo::new()?;
    repo.add_file("small.md", "size_needle small\n")?;
    repo.add_file(
        "huge.md",
        &format!("size_needle huge\n{}", "x".repeat(4096)),
    )?;
    let (engine, _idx) = engine_with(
        &repo,
        EngineOptions {
            index_max_file_size: Some(1024),
            ..Default::default()
        },
    )
    .await?;
    let registry = ToolRegistry::new();

    let out = registry
        .dispatch(
            "search_code",
            &engine,
            json!({"repo": repo.name(), "query": "size_needle"}),
        )
        .await?;
    assert!(out.contains("small.md"), "{out}");
    assert!(!out.contains("huge.md"), "oversized file indexed:\n{out}");
    Ok(())
}
