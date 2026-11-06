//! Linked git worktrees: discovered with the repository, indexed on first
//! use, addressed by name, branch or path, and kept apart from the main
//! checkout.

use anyhow::Result;
use narsil_mcp::config::schema::WorktreeMode;
use narsil_mcp::engine::{CodeIntelEngine, EngineOptions};
use narsil_mcp::persist::{ChangeType, FileChange};
use narsil_mcp::tool_handlers::ToolRegistry;
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const WT_ONLY: &str = "worktree_only_fn";

struct Fixture {
    root: TempDir,
    primary: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

impl Fixture {
    /// `app/` with one committed file.
    fn new() -> Result<Self> {
        let root = TempDir::new()?;
        let primary = root.path().canonicalize()?.join("app");
        std::fs::create_dir(&primary)?;
        git(&primary, &["init", "-q", "-b", "main"])?;
        std::fs::write(
            primary.join("a.rs"),
            "pub fn shared_fn() -> u32 {\n    1\n}\n",
        )?;
        git(&primary, &["add", "."])?;
        git(&primary, &["commit", "-qm", "init"])?;
        Ok(Self { root, primary })
    }

    fn base(&self) -> PathBuf {
        self.primary.parent().unwrap().to_path_buf()
    }

    /// A worktree at `rel` (relative to the temp root) on a new branch, with
    /// a file only it has.
    fn add_worktree(&self, rel: &str, branch: &str) -> Result<PathBuf> {
        let path = self.base().join(rel);
        git(
            &self.primary,
            &[
                "worktree",
                "add",
                "-q",
                path.to_str().unwrap(),
                "-b",
                branch,
            ],
        )?;
        std::fs::write(
            path.join("only_in_wt.rs"),
            format!("pub fn {WT_ONLY}() -> u32 {{\n    2\n}}\n"),
        )?;
        Ok(path)
    }

    fn options(&self, mode: WorktreeMode) -> EngineOptions {
        EngineOptions {
            git_enabled: true,
            worktree_mode: mode,
            ..Default::default()
        }
    }

    async fn engine(&self, options: EngineOptions) -> Result<(CodeIntelEngine, TempDir)> {
        let index_dir = TempDir::new_in(self.root.path())?;
        let engine = self.engine_at(index_dir.path(), options).await?;
        Ok((engine, index_dir))
    }

    async fn engine_at(&self, index: &Path, options: EngineOptions) -> Result<CodeIntelEngine> {
        let engine =
            CodeIntelEngine::with_options(index.to_path_buf(), vec![self.primary.clone()], options)
                .await?;
        engine.complete_initialization().await?;
        Ok(engine)
    }
}

async fn call(engine: &CodeIntelEngine, tool: &str, args: serde_json::Value) -> Result<String> {
    ToolRegistry::new().dispatch(tool, engine, args).await
}

async fn search(engine: &CodeIntelEngine, repo: &str, query: &str) -> Result<String> {
    call(engine, "search_code", json!({"repo": repo, "query": query})).await
}

#[tokio::test]
async fn lazy_key_indexes_on_first_call() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(listed.contains("**app@wt** — not indexed yet"), "{listed}");
    assert!(listed.contains("branch `feature/x`"), "{listed}");

    let found = search(&engine, "app@wt", WT_ONLY).await?;
    assert!(found.contains("only_in_wt.rs"), "{found}");
    let shared = search(&engine, "app@wt", "shared_fn").await?;
    assert!(shared.contains("a.rs"), "{shared}");

    let symbols = call(
        &engine,
        "find_symbols",
        json!({"repo": "app@wt", "pattern": WT_ONLY}),
    )
    .await?;
    assert!(symbols.contains(WT_ONLY), "{symbols}");

    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(listed.contains("**app@wt** — indexed"), "{listed}");
    Ok(())
}

#[tokio::test]
async fn primary_does_not_see_worktree_code() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    search(&engine, "app@wt", WT_ONLY).await?;
    let primary = search(&engine, "app", WT_ONLY).await?;
    assert!(!primary.contains("only_in_wt.rs"), "{primary}");
    Ok(())
}

#[tokio::test]
async fn abs_path_resolves_to_worktree() -> Result<()> {
    let fx = Fixture::new()?;
    let wt = fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let found = search(&engine, wt.to_str().unwrap(), WT_ONLY).await?;
    assert!(found.contains("only_in_wt.rs"), "{found}");

    let file = call(
        &engine,
        "get_file",
        json!({"repo": wt.to_str().unwrap(), "path": "only_in_wt.rs"}),
    )
    .await?;
    assert!(file.contains(WT_ONLY), "{file}");
    Ok(())
}

#[tokio::test]
async fn branch_alias_resolves() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt1", "feature/login")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let found = search(&engine, "app@feature-login", WT_ONLY).await?;
    assert!(found.contains("only_in_wt.rs"), "{found}");
    Ok(())
}

#[tokio::test]
async fn worktree_added_after_start() -> Result<()> {
    let fx = Fixture::new()?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;
    let wt = fx.add_worktree("late", "feature/late")?;

    let by_path = search(&engine, wt.to_str().unwrap(), WT_ONLY).await?;
    assert!(by_path.contains("only_in_wt.rs"), "{by_path}");

    let wt2 = fx.add_worktree("later", "feature/later")?;
    std::fs::write(wt2.join("later.rs"), "pub fn later_only() {}\n")?;
    let by_name = search(&engine, "app@later", "later_only").await?;
    assert!(by_name.contains("later.rs"), "{by_name}");
    Ok(())
}

#[tokio::test]
async fn omitted_repo_skips_unindexed_worktrees() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let everywhere = call(&engine, "search_code", json!({"query": WT_ONLY})).await?;
    assert!(!everywhere.contains("only_in_wt.rs"), "{everywhere}");
    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(listed.contains("**app@wt** — not indexed yet"), "{listed}");
    Ok(())
}

#[tokio::test]
async fn primary_unaffected_by_nested_worktree() -> Result<()> {
    let fx = Fixture::new()?;
    let nested = fx.add_worktree("app/wt-nested", "feature/nested")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let before = search(&engine, "app", WT_ONLY).await?;
    assert!(!before.contains("only_in_wt.rs"), "{before}");

    let inside = search(&engine, "app@wt-nested", WT_ONLY).await?;
    assert!(inside.contains("only_in_wt.rs"), "{inside}");

    let after = search(&engine, "app", WT_ONLY).await?;
    assert!(!after.contains("only_in_wt.rs"), "{after}");
    let status = call(&engine, "get_index_status", json!({"repo": "app"})).await?;
    assert!(status.contains("- Files: 1\n"), "{status}");

    let structure = call(
        &engine,
        "get_project_structure",
        json!({"repo": nested.to_str().unwrap()}),
    )
    .await?;
    assert!(structure.contains("only_in_wt.rs"), "{structure}");
    Ok(())
}

#[tokio::test]
async fn eager_mode_indexes_all() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Eager)).await?;
    engine.index_startup_worktrees().await;

    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(listed.contains("**app@wt** — indexed"), "{listed}");
    let everywhere = call(&engine, "search_code", json!({"query": WT_ONLY})).await?;
    assert!(everywhere.contains("only_in_wt.rs"), "{everywhere}");
    Ok(())
}

#[tokio::test]
async fn off_mode_unknown_key() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Off)).await?;

    let err = call(&engine, "get_project_structure", json!({"repo": "app@wt"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
    let nothing = search(&engine, "app@wt", WT_ONLY).await?;
    assert!(!nothing.contains("only_in_wt.rs"), "{nothing}");
    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(!listed.contains("Git Worktrees"), "{listed}");
    Ok(())
}

#[tokio::test]
async fn detached_worktree() -> Result<()> {
    let fx = Fixture::new()?;
    let path = fx.base().join("det");
    git(
        &fx.primary,
        &["worktree", "add", "-q", "--detach", path.to_str().unwrap()],
    )?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(listed.contains("**app@det**"), "{listed}");
    assert!(listed.contains("detached @ "), "{listed}");
    let found = search(&engine, "app@det", "shared_fn").await?;
    assert!(found.contains("a.rs"), "{found}");
    Ok(())
}

#[tokio::test]
async fn same_dir_name_gets_suffix() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("one/wt", "feature/one")?;
    fx.add_worktree("two/wt", "feature/two")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(listed.contains("**app@wt**"), "{listed}");
    assert!(listed.contains("**app@wt~2**"), "{listed}");
    Ok(())
}

#[tokio::test]
async fn git_tools_see_worktree_branch() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let modified = call(&engine, "get_modified_files", json!({"repo": "app@wt"})).await?;
    assert!(modified.contains("only_in_wt.rs"), "{modified}");
    let primary = call(&engine, "get_modified_files", json!({"repo": "app"})).await?;
    assert!(!primary.contains("only_in_wt.rs"), "{primary}");
    Ok(())
}

#[tokio::test]
async fn watch_updates_worktree() -> Result<()> {
    let fx = Fixture::new()?;
    let wt = fx.add_worktree("wt", "feature/x")?;
    let mut options = fx.options(WorktreeMode::Lazy);
    options.watch_enabled = true;
    let (engine, _idx) = fx.engine(options).await?;
    search(&engine, "app@wt", WT_ONLY).await?;

    let added = wt.join("added.rs");
    std::fs::write(&added, "pub fn added_later() {}\n")?;
    let count = engine
        .process_file_changes(&[FileChange {
            path: added,
            change_type: ChangeType::Created,
        }])
        .await?;
    assert_eq!(count, 1);

    let found = search(&engine, "app@wt", "added_later").await?;
    assert!(found.contains("added.rs"), "{found}");
    let primary = search(&engine, "app", "added_later").await?;
    assert!(!primary.contains("added.rs"), "{primary}");
    Ok(())
}

#[tokio::test]
async fn removed_worktree_unregistered() -> Result<()> {
    let fx = Fixture::new()?;
    let wt = fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;
    search(&engine, "app@wt", WT_ONLY).await?;

    git(
        &fx.primary,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    )?;
    let err = search(&engine, "app@wt", WT_ONLY).await.unwrap_err();
    assert!(err.to_string().contains("removed"), "{err}");

    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(!listed.contains("app@wt"), "{listed}");
    let everywhere = call(&engine, "search_code", json!({"query": WT_ONLY})).await?;
    assert!(!everywhere.contains("only_in_wt.rs"), "{everywhere}");
    Ok(())
}

#[tokio::test]
async fn worktree_persist_warm_restart() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let index = TempDir::new_in(fx.root.path())?;
    let mut options = fx.options(WorktreeMode::Lazy);
    options.persist_enabled = true;

    {
        let engine = fx.engine_at(index.path(), options.clone()).await?;
        search(&engine, "app@wt", WT_ONLY).await?;
    }
    let databases = std::fs::read_dir(index.path())?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "db"))
        .count();
    assert_eq!(databases, 2, "one SQLite index per checkout");

    let engine = fx.engine_at(index.path(), options).await?;
    let listed = call(&engine, "list_repos", json!({})).await?;
    assert!(listed.contains("**app@wt** — not indexed yet"), "{listed}");
    let found = search(&engine, "app@wt", WT_ONLY).await?;
    assert!(found.contains("only_in_wt.rs"), "{found}");
    let primary = search(&engine, "app", WT_ONLY).await?;
    assert!(!primary.contains("only_in_wt.rs"), "{primary}");
    Ok(())
}

#[tokio::test]
async fn call_graph_built_for_worktree() -> Result<()> {
    let fx = Fixture::new()?;
    let wt = fx.add_worktree("wt", "feature/x")?;
    std::fs::write(
        wt.join("caller.rs"),
        format!("pub fn wt_caller() -> u32 {{\n    {WT_ONLY}()\n}}\n"),
    )?;
    let mut options = fx.options(WorktreeMode::Lazy);
    options.call_graph_enabled = true;
    let (engine, _idx) = fx.engine(options).await?;

    let callers = call(
        &engine,
        "get_callers",
        json!({"repo": "app@wt", "function": WT_ONLY}),
    )
    .await?;
    assert!(callers.contains("wt_caller"), "{callers}");
    Ok(())
}

#[tokio::test]
async fn unknown_worktree_name_is_an_error() -> Result<()> {
    let fx = Fixture::new()?;
    fx.add_worktree("wt", "feature/x")?;
    let (engine, _idx) = fx.engine(fx.options(WorktreeMode::Lazy)).await?;

    let err = call(&engine, "get_modified_files", json!({"repo": "app@nope"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Worktree 'app@nope' not found"), "{err}");
    assert!(err.contains("app@wt"), "{err}");
    Ok(())
}
