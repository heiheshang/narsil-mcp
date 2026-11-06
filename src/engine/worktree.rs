//! Linked git worktrees of the indexed repositories: discovery, indexing on
//! first use, resolution of the `repo` argument, and removal.

use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tracing::{debug, debug_span, info, warn, Instrument};

use super::{CodeIntelEngine, RepoEntry, RepoState};
use crate::config::schema::WorktreeMode;
use crate::persist::WatchCommand;

impl CodeIntelEngine {
    /// Gives background indexing a handle that outlives the tool call that
    /// started it. Without it a call waits for the indexing to finish.
    pub fn set_self_ref(self: &Arc<Self>) {
        let _ = self.self_ref.set(Arc::downgrade(self));
    }

    pub(crate) fn take_watch_commands(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<WatchCommand>> {
        self.watch_rx.lock().take()
    }

    pub(super) fn send_watch(&self, command: WatchCommand) {
        if !self.options.watch_enabled {
            return;
        }
        debug!(?command, "watch command");
        let _ = self.watch_tx.send(command);
    }

    pub fn worktree_mode(&self) -> WorktreeMode {
        self.options.worktree_mode
    }

    /// The worktree the server process was started in, if any.
    pub fn cwd_worktree(&self) -> Option<&str> {
        self.cwd_worktree.get().map(String::as_str)
    }

    /// Registers the worktrees of every primary repository, without indexing.
    pub(super) fn discover_all_worktrees(&self) {
        if self.options.worktree_mode == WorktreeMode::Off {
            info!("Worktree discovery disabled (--worktrees off)");
            return;
        }
        if !crate::worktree::git_available() {
            return;
        }
        let primaries = self.registry.primaries();
        for primary in &primaries {
            self.refresh_worktrees(primary);
        }
        let worktrees = self.registry.len() - primaries.len();
        info!(
            mode = %self.options.worktree_mode,
            primaries = primaries.len(),
            worktrees,
            "worktree discovery done"
        );

        let cwd = std::env::current_dir()
            .ok()
            .and_then(|cwd| cwd.canonicalize().ok());
        if let Some(owner) = cwd.and_then(|cwd| self.registry.owner_of(&cwd)) {
            if owner.is_worktree() {
                info!(key = %owner.key, "server cwd is worktree");
                let _ = self.cwd_worktree.set(owner.key);
            }
        }
    }

    /// Brings the registered worktrees of `primary` in line with
    /// `git worktree list`. Returns the keys registered by this call.
    pub(super) fn refresh_worktrees(&self, primary: &RepoEntry) -> Vec<String> {
        let found = match crate::worktree::list_worktrees(&primary.path) {
            Ok(found) => found,
            Err(e) => {
                debug!(primary = %primary.key, error = %e, "worktree list unavailable");
                return Vec::new();
            }
        };
        for known in self.registry.worktrees_of(&primary.key) {
            let still_there = found.iter().any(|info| info.path == known.path);
            if !still_there && known.state != RepoState::Indexing {
                self.unregister_repo(&known.key, "gone from git worktree list");
            }
        }
        let mut added = Vec::new();
        for info in found {
            let is_new = self.registry.key_for_root(&info.path).is_none();
            let key = self.registry.register_worktree(
                &primary.key,
                &info.path,
                &info.name,
                info.branch,
                info.head,
            );
            if is_new {
                info!(key = %key, path = ?info.path, "worktree registered");
                self.purge_from_enclosing(&info.path);
                added.push(key);
            }
        }
        added
    }

    /// A worktree created inside an indexed tree may already have been picked
    /// up by the watcher as that tree's files; they are the worktree's now.
    fn purge_from_enclosing(&self, root: &Path) {
        for owner in self.indexed_roots() {
            if owner.path != root && root.starts_with(&owner.path) {
                self.purge_subtree(&owner, root);
            }
        }
    }

    fn purge_subtree(&self, owner: &RepoEntry, root: &Path) {
        let Ok(rel_dir) = root.strip_prefix(&owner.path) else {
            return;
        };
        let files: Vec<PathBuf> = self
            .file_cache
            .iter()
            .filter(|e| e.key().starts_with(root))
            .map(|e| e.key().clone())
            .collect();
        let symbols = self
            .symbols
            .get_mut(&owner.key)
            .map(|mut symbols| {
                let before = symbols.len();
                symbols.retain(|sym| !Path::new(&sym.file_path).starts_with(rel_dir));
                before - symbols.len()
            })
            .unwrap_or(0);
        if files.is_empty() && symbols == 0 {
            return;
        }
        for file in &files {
            let rel = file
                .strip_prefix(&owner.path)
                .unwrap_or(file)
                .to_string_lossy()
                .to_string();
            self.search_index.tantivy().delete_file(&owner.key, &rel);
            if let Some(tri) = self.substring_index.as_ref() {
                tri.delete_file(&owner.key, &rel);
            }
            if let Some(cg) = self.call_graphs.get(&owner.key) {
                let _ = cg.remove_functions_for_file(&rel);
            }
            self.file_cache.remove(file);
            self.chunk_cache.remove(file);
        }
        self.embedding_engine.remove_under(&owner.key, rel_dir);
        if let Some(neural) = self.neural_engine.as_ref() {
            neural.remove_under(&owner.key, rel_dir);
        }
        self.commit_search_indexes();
        self.query_cache.invalidate_for_repo(&owner.key);
        info!(
            repo = %owner.key,
            worktree = ?root,
            files = files.len(),
            symbols,
            "worktree files removed from enclosing repository"
        );
    }

    fn commit_search_indexes(&self) {
        if let Err(e) = self.search_index.tantivy().commit() {
            warn!("Failed to commit search index: {}", e);
        }
        if let Some(tri) = self.substring_index.as_ref() {
            if let Err(e) = tri.commit() {
                warn!("Failed to commit trigram index: {}", e);
            }
        }
    }

    /// Search documents persisted by an earlier run for repositories that are
    /// not indexed in this one — removed worktrees, or worktrees nobody has
    /// asked for yet — would otherwise answer searches without `repo`.
    pub(super) fn drop_unindexed_search_docs(&self) {
        let keep: std::collections::HashSet<String> =
            self.indexed_roots().into_iter().map(|e| e.key).collect();
        let mut tags: std::collections::BTreeSet<String> =
            self.search_index.tantivy().repos().into_iter().collect();
        if let Some(tri) = self.substring_index.as_ref() {
            tags.extend(tri.repos());
        }
        let stale: Vec<String> = tags.into_iter().filter(|t| !keep.contains(t)).collect();
        if stale.is_empty() {
            return;
        }
        for tag in &stale {
            if let Err(e) = self.search_index.tantivy().delete_repo(tag) {
                warn!(repo = %tag, error = %e, "failed to drop stale search documents");
            }
            if let Some(tri) = self.substring_index.as_ref() {
                tri.delete_repo(tag);
            }
        }
        self.commit_search_indexes();
        info!(repos = ?stale, "dropped search documents of repositories not indexed in this run");
    }

    pub(super) fn index_job(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.index_jobs
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// `git worktree add` inside a watched tree writes a `.git` file at the
    /// new root; registering it before its files are read keeps them out of
    /// the enclosing repository.
    pub(super) fn register_new_worktrees(&self, changes: &[crate::persist::FileChange]) {
        if self.options.worktree_mode == WorktreeMode::Off {
            return;
        }
        for change in changes {
            if change.path.file_name().is_none_or(|n| n != ".git") || !change.path.is_file() {
                continue;
            }
            let Some(root) = change.path.parent() else {
                continue;
            };
            if self.registry.key_for_root(root).is_some() {
                continue;
            }
            let primary = self
                .registry
                .owner_of(root)
                .and_then(|owner| match &owner.kind {
                    super::RepoKind::Primary => Some(owner),
                    super::RepoKind::Worktree { primary_key, .. } => {
                        self.registry.entry(primary_key)
                    }
                });
            if let Some(primary) = primary {
                debug!(root = ?root, primary = %primary.key, "new checkout seen by watcher");
                self.refresh_worktrees(&primary);
            }
        }
    }

    /// Worktrees to index without waiting for a tool call: the server's own
    /// directory always, every one with `--worktrees eager`.
    pub async fn index_startup_worktrees(&self) {
        let keys: Vec<String> = match self.options.worktree_mode {
            WorktreeMode::Off => return,
            WorktreeMode::Eager => self
                .registry
                .snapshot()
                .into_iter()
                .filter(RepoEntry::is_worktree)
                .map(|e| e.key)
                .collect(),
            WorktreeMode::Lazy => self
                .cwd_worktree()
                .map(str::to_string)
                .into_iter()
                .collect(),
        };
        for key in keys {
            if let Err(e) = self.ensure_repo_indexed(&key).await {
                warn!(key = %key, error = %e, "worktree indexing failed");
            }
        }
    }

    /// Indexes a registered worktree unless it is indexed already. Concurrent
    /// calls for one key share the work.
    pub async fn ensure_repo_indexed(&self, key: &str) -> Result<()> {
        let job = self.index_job(key);
        let _guard = job.lock().await;

        let entry = self
            .registry
            .entry(key)
            .ok_or_else(|| anyhow!("Repository '{key}' is no longer registered"))?;
        if !entry.is_worktree() || entry.state == RepoState::Ready {
            return Ok(());
        }
        if !entry.path.is_dir() {
            self.unregister_repo(key, "directory is gone");
            bail!(
                "Worktree '{key}' no longer exists at {}",
                entry.path.display()
            );
        }

        // Primaries first: their walk decides TF-IDF snapshot reuse from the
        // number of indexed repositories.
        while !self.primaries_walked() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        self.registry.set_state(key, RepoState::Indexing);
        let started = Instant::now();
        let span = debug_span!("ensure_repo_indexed", key = %key);
        match self.index_worktree(&entry).instrument(span).await {
            Ok((files, warm)) => {
                info!(
                    key = %key,
                    files,
                    warm,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "worktree indexed"
                );
                Ok(())
            }
            Err(e) => {
                warn!(key = %key, error = %e, "worktree indexing failed");
                self.registry
                    .set_state(key, RepoState::Failed(e.to_string()));
                Err(e)
            }
        }
    }

    /// Set once the primaries are walked and saved, by startup or `--reindex`.
    fn primaries_walked(&self) -> bool {
        self.is_fully_initialized() || self.primaries_reindexed.load(Ordering::Acquire)
    }

    async fn index_worktree(&self, entry: &RepoEntry) -> Result<(usize, bool)> {
        let key = entry.key.as_str();
        if self.options.call_graph_enabled {
            self.call_graphs.entry(key.to_string()).or_default();
        }
        let persisted = self
            .index_store
            .as_ref()
            .and_then(|store| store.load_or_create(&entry.path).ok())
            .filter(|persisted| !persisted.files.is_empty());
        let warm = persisted.is_some();
        let outcome = match persisted {
            Some(persisted) => self.index_repo_warm(&entry.path, persisted).await?,
            None => self.index_repo_inner(&entry.path, None).await?,
        };

        self.commit_search_indexes();
        if self.registry.entry(key).is_none() {
            // Removed while it was being walked: what the walk wrote is orphaned.
            self.drop_repo_data(key, &entry.path);
            bail!("Worktree '{key}' was removed while it was being indexed");
        }
        if self.options.git_enabled {
            self.attach_git(key, &entry.path);
        }
        if self.options.call_graph_enabled {
            self.load_startup_call_graph(key, &entry.path);
        }
        self.registry.set_state(key, RepoState::Ready);
        if self.options.persist_enabled {
            if let Err(e) = self.save_index_only(key).await {
                warn!(key = %key, error = %e, "failed to save worktree index");
            }
        }
        self.send_watch(WatchCommand::Watch(entry.path.clone()));
        Ok((outcome.indexed_files, warm))
    }

    /// Like [`ensure_repo_indexed`](Self::ensure_repo_indexed), but gives up
    /// waiting after `--worktree-index-wait-secs` and lets the indexing finish
    /// in the background.
    pub async fn ensure_repo_indexed_or_pending(&self, key: &str) -> Result<()> {
        let Some(engine) = self.self_ref.get().and_then(Weak::upgrade) else {
            return self.ensure_repo_indexed(key).await;
        };
        let wait = self.options.worktree_wait;
        let owned = key.to_string();
        let task = tokio::spawn(async move { engine.ensure_repo_indexed(&owned).await });
        match tokio::time::timeout(wait, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => Err(anyhow!("Indexing worktree '{key}' failed: {e}")),
            Err(_) => {
                info!(key = %key, wait_s = wait.as_secs(), "worktree index wait timed out, continuing in background");
                Err(anyhow!(
                    "Worktree '{key}' is being indexed; retry the request shortly. \
                     Progress: get_index_status."
                ))
            }
        }
    }

    /// Turns a worktree name, branch alias or path in `args["repo"]` into the
    /// worktree's key, indexing it first if needed. Anything else is left for
    /// the tool to resolve as before.
    pub async fn resolve_repo_arg(&self, args: &mut serde_json::Value) -> Result<()> {
        if self.options.worktree_mode == WorktreeMode::Off {
            return Ok(());
        }
        let Some(raw) = args.get("repo").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        if raw.is_empty() {
            return Ok(());
        }
        let raw = raw.to_string();
        let Some((entry, how)) = self.resolve_worktree(&raw) else {
            return self.unknown_worktree_error(&raw);
        };
        if !entry.is_worktree() {
            return Ok(());
        }
        if !entry.path.is_dir() {
            self.unregister_repo(&entry.key, "directory is gone");
            bail!(
                "Worktree '{}' was removed from {}",
                entry.key,
                entry.path.display()
            );
        }
        if entry.state != RepoState::Ready {
            self.ensure_repo_indexed_or_pending(&entry.key).await?;
        }
        // A path below the worktree root keeps pointing at that directory.
        let rewrite = how != Resolution::PathInside;
        if rewrite && raw != entry.key {
            args["repo"] = serde_json::Value::String(entry.key.clone());
        }
        debug!(input = %raw, resolved = %entry.key, how = ?how, "repo arg resolved");
        Ok(())
    }

    /// `<repo>@<name>` of a known repository that names none of its
    /// worktrees; anything else is left for the tool to report.
    fn unknown_worktree_error(&self, raw: &str) -> Result<()> {
        let Some((primary, _)) = raw.split_once('@') else {
            return Ok(());
        };
        if !self
            .registry
            .entry(primary)
            .is_some_and(|e| !e.is_worktree())
        {
            return Ok(());
        }
        let known: Vec<String> = self
            .registry
            .worktrees_of(primary)
            .into_iter()
            .map(|e| e.key)
            .collect();
        bail!(
            "Worktree '{raw}' not found. Worktrees of '{primary}': {}. \
             Use list_repos to see them.",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        )
    }

    fn resolve_worktree(&self, raw: &str) -> Option<(RepoEntry, Resolution)> {
        if let Some(key) = self.registry.resolve_alias(raw) {
            return self.registry.entry(&key).map(|e| (e, Resolution::Key));
        }
        let looks_like_path = raw.contains('/') || raw.contains('\\');
        if looks_like_path && Path::new(raw).is_absolute() {
            return self.resolve_worktree_path(Path::new(raw));
        }
        let (primary, _) = raw.split_once('@')?;
        let primary = self.registry.entry(primary).filter(|e| !e.is_worktree())?;
        self.refresh_worktrees(&primary);
        let key = self.registry.resolve_alias(raw)?;
        self.registry
            .entry(&key)
            .map(|e| (e, Resolution::Rediscovered))
    }

    fn resolve_worktree_path(&self, path: &Path) -> Option<(RepoEntry, Resolution)> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let how = |owner: &RepoEntry| {
            if owner.path == path {
                Resolution::Key
            } else {
                Resolution::PathInside
            }
        };
        // Known roots need no git: only a checkout nested below the owner
        // that is not registered yet sends us to `git worktree list`.
        if let Some(owner) = self.registry.owner_of(&path) {
            if !unregistered_checkout_between(&owner.path, &path) {
                let how = how(&owner);
                return Some((owner, how));
            }
        }
        let root = crate::worktree::checkout_root(&path)?;
        let common = crate::worktree::common_dir(&root)?;
        let primary = self
            .registry
            .primaries()
            .into_iter()
            .find(|primary| self.common_dir_of(primary).as_deref() == Some(common.as_path()))?;
        self.refresh_worktrees(&primary);
        let owner = self
            .registry
            .owner_of(&path)
            .filter(RepoEntry::is_worktree)?;
        let how = how(&owner);
        Some((owner, how))
    }

    fn common_dir_of(&self, primary: &RepoEntry) -> Option<PathBuf> {
        if let Some(known) = self.common_dirs.get(&primary.key) {
            return known.clone();
        }
        let dir = crate::worktree::common_dir(&primary.path);
        self.common_dirs.insert(primary.key.clone(), dir.clone());
        dir
    }

    /// The registered worktrees and how to address them; empty without any.
    /// With `only`, just that worktree.
    pub(super) fn worktrees_markdown(&self, only: Option<&str>) -> String {
        let worktrees: Vec<RepoEntry> = self
            .registry
            .snapshot()
            .into_iter()
            .filter(RepoEntry::is_worktree)
            .filter(|e| only.is_none_or(|key| key == e.key))
            .collect();
        if worktrees.is_empty() {
            return String::new();
        }
        let mut out = format!(
            "\n## Git Worktrees (--worktrees {})\n\n\
             Address a worktree by its name below, by `<repo>@<branch>` (with `/` in the \
             branch written as `-`), or by its absolute path. It is indexed on first use; \
             `<repo>` alone always means the main checkout.\n\n",
            self.options.worktree_mode
        );
        let cwd = self.cwd_worktree();
        for entry in worktrees {
            let state = match &entry.state {
                RepoState::Registered => "not indexed yet".to_string(),
                RepoState::Indexing => "indexing".to_string(),
                RepoState::Ready => "indexed".to_string(),
                RepoState::Failed(e) => format!("failed: {e}"),
            };
            let head = match &entry.kind {
                super::RepoKind::Worktree {
                    branch: Some(branch),
                    ..
                } => format!("branch `{branch}`"),
                super::RepoKind::Worktree { head, .. } => {
                    format!("detached @ {}", head.get(..7).unwrap_or(head))
                }
                super::RepoKind::Primary => String::new(),
            };
            let here = if cwd == Some(entry.key.as_str()) {
                " — server working directory"
            } else {
                ""
            };
            out.push_str(&format!(
                "- **{}** — {state}; {head}; {}{here}\n",
                entry.key,
                entry.path.display()
            ));
        }
        out
    }

    /// Drops a worktree from every index and stops watching it.
    pub(super) fn unregister_repo(&self, key: &str, reason: &str) {
        let Some(entry) = self.registry.entry(key).filter(RepoEntry::is_worktree) else {
            return;
        };
        self.registry.unregister(key);
        self.drop_repo_data(key, &entry.path);
        info!(key = %key, path = ?entry.path, reason, "worktree unregistered");
    }

    fn drop_repo_data(&self, key: &str, root: &Path) {
        if let Err(e) = self.search_index.tantivy().delete_repo(key) {
            warn!(key = %key, error = %e, "failed to drop search documents");
        }
        if let Some(tri) = self.substring_index.as_ref() {
            tri.delete_repo(key);
        }
        self.commit_search_indexes();
        self.repos.remove(key);
        self.symbols.remove(key);
        self.normalized_docs.remove(key);
        self.call_graphs.remove(key);
        self.git_repos.remove(key);
        self.embedding_engine.remove_repo(key);
        if let Some(neural) = self.neural_engine.as_ref() {
            neural.remove_repo(key);
        }
        self.file_cache.retain(|path, _| !path.starts_with(root));
        self.chunk_cache.remove_under(root);
        self.query_cache.invalidate_for_repo(key);
        self.send_watch(WatchCommand::Unwatch(root.to_path_buf()));
    }
}

/// Whether a directory strictly between `owner` and `path` (or `path`
/// itself) is the root of another checkout, i.e. holds a `.git`.
fn unregistered_checkout_between(owner: &Path, path: &Path) -> bool {
    path.ancestors()
        .take_while(|dir| *dir != owner && dir.starts_with(owner))
        .any(|dir| dir.join(".git").exists())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    Key,
    PathInside,
    Rediscovered,
}
