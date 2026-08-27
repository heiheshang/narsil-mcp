//! Persistent index storage and watch mode for incremental updates
//!
//! Saves index to disk and watches for file changes to update incrementally.

use anyhow::Result;
#[cfg(feature = "native")]
use notify::{Config, Event, EventKind, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::store::{FileRow, SqliteStore};
use crate::symbols::Symbol;

/// File metadata for change detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub path: PathBuf,
    pub content_hash: String,
    pub modified_time: u64,
    pub size: u64,
    pub symbols: Vec<Symbol>,
}

/// Persisted index structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedIndex {
    pub version: u32,
    pub created_at: u64,
    pub updated_at: u64,
    pub repo_root: PathBuf,
    pub files: HashMap<PathBuf, FileMetadata>,
}

impl PersistedIndex {
    const CURRENT_VERSION: u32 = 2;

    pub fn new(repo_root: PathBuf) -> Self {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            version: Self::CURRENT_VERSION,
            created_at: now,
            updated_at: now,
            repo_root,
            files: HashMap::new(),
        }
    }

    /// Check if a file needs re-indexing
    pub fn needs_reindex(&self, path: &Path) -> Result<bool> {
        let metadata = std::fs::metadata(path)?;
        let modified = metadata
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        let size = metadata.len();

        if let Some(cached) = self.files.get(path) {
            // Quick check: size and mtime
            if cached.size == size && cached.modified_time == modified {
                return Ok(false);
            }

            // Slower check: content hash
            let hash = hash_file(path)?;
            Ok(hash != cached.content_hash)
        } else {
            Ok(true)
        }
    }

    /// Update file in index
    pub fn update_file(&mut self, path: PathBuf, symbols: Vec<Symbol>) -> Result<()> {
        let metadata = std::fs::metadata(&path)?;
        let hash = hash_file(&path)?;

        self.files.insert(
            path.clone(),
            FileMetadata {
                path,
                content_hash: hash,
                modified_time: metadata
                    .modified()?
                    .duration_since(SystemTime::UNIX_EPOCH)?
                    .as_secs(),
                size: metadata.len(),
                symbols,
            },
        );

        self.updated_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Ok(())
    }

    /// Remove file from index
    pub fn remove_file(&mut self, path: &Path) {
        self.files.remove(path);
        self.updated_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }

    /// Get all symbols across all files
    pub fn all_symbols(&self) -> Vec<&Symbol> {
        self.files.values().flat_map(|f| f.symbols.iter()).collect()
    }

    /// Get symbols for a specific file
    pub fn file_symbols(&self, path: &Path) -> Option<&[Symbol]> {
        self.files.get(path).map(|f| f.symbols.as_slice())
    }
}

/// Compute SHA256 hash of file content
fn hash_file(path: &Path) -> Result<String> {
    let content = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&content);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Index storage manager
pub struct IndexStore {
    index_dir: PathBuf,
}

impl IndexStore {
    pub fn new(index_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&index_dir)?;
        Ok(Self { index_dir })
    }

    /// SQLite database path for a repository.
    pub fn db_path(&self, repo_root: &Path) -> PathBuf {
        let hash = {
            let mut hasher = Sha256::new();
            hasher.update(repo_root.to_string_lossy().as_bytes());
            format!("{:x}", hasher.finalize())
        };
        self.index_dir.join(format!("{}.db", &hash[..16]))
    }

    /// The `repo` key used for rows belonging to `repo_root`.
    fn repo_key(repo_root: &Path) -> String {
        repo_root.to_string_lossy().to_string()
    }

    /// Load or create index for a repository
    pub fn load_or_create(&self, repo_root: &Path) -> Result<PersistedIndex> {
        let db_path = self.db_path(repo_root);

        if db_path.exists() {
            match self.load_from_db(repo_root) {
                Ok(index) => {
                    info!("Loaded existing index from {:?}", db_path);
                    return Ok(index);
                }
                Err(e) => {
                    warn!("Failed to load index, creating new: {}", e);
                }
            }
        }

        info!("Creating new index for {:?}", repo_root);
        Ok(PersistedIndex::new(repo_root.to_path_buf()))
    }

    /// Rebuild an in-memory `PersistedIndex` from the SQLite store.
    ///
    /// File metadata rows are read first (keyed by absolute path), then symbols
    /// are streamed and attached to their file by `repo_root.join(file_path)`.
    fn load_from_db(&self, repo_root: &Path) -> Result<PersistedIndex> {
        let store = SqliteStore::open(&self.db_path(repo_root))?;
        let repo = Self::repo_key(repo_root);

        let created_at = store
            .get_meta("created_at")?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let updated_at = store
            .get_meta("updated_at")?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        let mut files: HashMap<PathBuf, FileMetadata> = HashMap::new();
        store.for_each_file(&repo, |f| {
            let abs = PathBuf::from(f.path);
            files.insert(
                abs.clone(),
                FileMetadata {
                    path: abs,
                    content_hash: f.content_hash,
                    modified_time: f.modified_time,
                    size: f.size,
                    symbols: Vec::new(),
                },
            );
        })?;

        let mut dropped = 0usize;
        store.for_each_symbol(&repo, |sym| {
            let abs = repo_root.join(&sym.file_path);
            match files.get_mut(&abs) {
                Some(meta) => meta.symbols.push(sym),
                None => dropped += 1,
            }
        })?;
        if dropped > 0 {
            warn!(
                "Dropped {} symbol(s) with no matching file row for repo {}",
                dropped, repo
            );
        }

        Ok(PersistedIndex {
            version: PersistedIndex::CURRENT_VERSION,
            created_at,
            updated_at,
            repo_root: repo_root.to_path_buf(),
            files,
        })
    }

    /// Save index for a repository
    pub fn save(&self, index: &PersistedIndex) -> Result<()> {
        let db_path = self.db_path(&index.repo_root);
        let store = SqliteStore::open(&db_path)?;
        let repo = Self::repo_key(&index.repo_root);

        let mut files = Vec::with_capacity(index.files.len());
        let mut symbols = Vec::new();
        for meta in index.files.values() {
            files.push(FileRow {
                path: meta.path.to_string_lossy().to_string(),
                content_hash: meta.content_hash.clone(),
                modified_time: meta.modified_time,
                size: meta.size,
            });
            symbols.extend(meta.symbols.iter().cloned());
        }

        store.replace_repo(&repo, &files, &symbols)?;
        store.set_meta("repo_root", &repo)?;
        store.set_meta("created_at", &index.created_at.to_string())?;
        store.set_meta("updated_at", &index.updated_at.to_string())?;
        info!("Saved index to {:?}", db_path);
        Ok(())
    }

    /// List all cached repositories
    pub fn list_cached(&self) -> Result<Vec<PathBuf>> {
        let mut repos = Vec::new();

        for entry in std::fs::read_dir(&self.index_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().map(|e| e == "db").unwrap_or(false) {
                if let Ok(store) = SqliteStore::open(&path) {
                    if let Ok(Some(root)) = store.get_meta("repo_root") {
                        repos.push(PathBuf::from(root));
                    }
                }
            }
        }

        Ok(repos)
    }

    /// Path to call-graph JSON file for a repository
    fn call_graph_path(&self, repo_root: &Path) -> PathBuf {
        let hash = {
            let mut hasher = Sha256::new();
            hasher.update(repo_root.to_string_lossy().as_bytes());
            format!("{:x}", hasher.finalize())
        };
        self.index_dir
            .join(format!("{}.callgraph.json", &hash[..16]))
    }

    /// Save call-graph to disk
    pub fn save_call_graph(&self, repo_root: &Path, call_graph_json: &str) -> Result<()> {
        let path = self.call_graph_path(repo_root);
        std::fs::write(&path, call_graph_json)?;
        debug!("Saved call-graph to {:?}", path);
        Ok(())
    }

    /// Load call-graph from disk
    pub fn load_call_graph(&self, repo_root: &Path) -> Result<Option<String>> {
        let path = self.call_graph_path(repo_root);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            debug!("Loaded call-graph from {:?}", path);
            Ok(Some(content))
        } else {
            Ok(None)
        }
    }
}

/// Poll interval used only when the OS event-driven watcher is unavailable.
///
/// Polling re-stats every watched path on each tick, so on large corpora (tens
/// of thousands of files) a sub-second interval pins a CPU core permanently.
/// Keep it lazy: the fallback is for correctness, not latency.
#[cfg(feature = "native")]
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Build a file watcher, preferring the OS event-driven backend (inotify on
/// Linux, FSEvents on macOS, ReadDirectoryChangesW on Windows).
///
/// Falls back to `PollWatcher` when the OS backend cannot be created (e.g. the
/// inotify instance limit is exhausted), or when `NARSIL_WATCH_POLL` is set,
/// which is the escape hatch for network filesystems that never deliver events.
#[cfg(feature = "native")]
fn build_watcher<F, H>(handler_factory: F) -> Result<Box<dyn Watcher + Send>>
where
    F: Fn() -> H,
    H: notify::EventHandler,
{
    let force_poll = std::env::var_os("NARSIL_WATCH_POLL").is_some_and(|v| v != "0");

    if !force_poll {
        match RecommendedWatcher::new(handler_factory(), Config::default()) {
            Ok(watcher) => return Ok(Box::new(watcher)),
            Err(e) => warn!(
                "OS file watcher unavailable ({e}), falling back to polling every {:?}",
                FALLBACK_POLL_INTERVAL
            ),
        }
    }

    let watcher = PollWatcher::new(
        handler_factory(),
        Config::default().with_poll_interval(FALLBACK_POLL_INTERVAL),
    )?;
    Ok(Box::new(watcher))
}

/// File watcher for incremental updates (legacy, sync API)
#[cfg(feature = "native")]
pub struct FileWatcher {
    watcher: Box<dyn Watcher + Send>,
    rx: std::sync::mpsc::Receiver<Result<Event, notify::Error>>,
    watched_paths: Vec<PathBuf>,
}

#[cfg(feature = "native")]
impl FileWatcher {
    pub fn new() -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();

        let watcher = build_watcher(move || {
            let tx = tx.clone();
            move |res| {
                let _ = tx.send(res);
            }
        })?;

        Ok(Self {
            watcher,
            rx,
            watched_paths: Vec::new(),
        })
    }

    /// Start watching a directory
    pub fn watch(&mut self, path: &Path) -> Result<()> {
        self.watcher.watch(path, RecursiveMode::Recursive)?;
        self.watched_paths.push(path.to_path_buf());
        info!("Watching for changes: {:?}", path);
        Ok(())
    }

    /// Stop watching a directory
    pub fn unwatch(&mut self, path: &Path) -> Result<()> {
        self.watcher.unwatch(path)?;
        self.watched_paths.retain(|p| p != path);
        Ok(())
    }

    /// Poll for file changes (non-blocking)
    pub fn poll_changes(&self) -> Vec<FileChange> {
        let mut changes = Vec::new();

        while let Ok(result) = self.rx.try_recv() {
            if let Ok(event) = result {
                for path in event.paths {
                    let change_type = match event.kind {
                        EventKind::Create(_) => ChangeType::Created,
                        EventKind::Modify(_) => ChangeType::Modified,
                        EventKind::Remove(_) => ChangeType::Deleted,
                        _ => continue,
                    };

                    changes.extend(source_changes_for_path(&path, change_type));
                }
            }
        }

        // Deduplicate
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes.dedup_by(|a, b| a.path == b.path);

        changes
    }

    /// Block until changes occur
    pub fn wait_for_changes(&self, timeout: Duration) -> Vec<FileChange> {
        let mut changes = Vec::new();

        if let Ok(Ok(event)) = self.rx.recv_timeout(timeout) {
            for path in event.paths {
                let change_type = match event.kind {
                    EventKind::Create(_) => ChangeType::Created,
                    EventKind::Modify(_) => ChangeType::Modified,
                    EventKind::Remove(_) => ChangeType::Deleted,
                    _ => continue,
                };

                changes.extend(source_changes_for_path(&path, change_type));
            }
        }

        // Drain any additional events
        changes.extend(self.poll_changes());

        changes
    }
}

/// Async file watcher for event-driven incremental updates
#[cfg(feature = "native")]
pub struct AsyncFileWatcher {
    _watcher: Box<dyn Watcher + Send>,
    watched_paths: Vec<PathBuf>,
}

#[cfg(feature = "native")]
impl AsyncFileWatcher {
    /// Create a new async file watcher and return a channel receiver for events
    pub fn new() -> Result<(Self, mpsc::Receiver<Vec<FileChange>>)> {
        let (tx, rx) = mpsc::channel(100);

        // Create a channel for the notify watcher
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();

        let watcher = build_watcher(move || {
            let notify_tx = notify_tx.clone();
            move |res| {
                let _ = notify_tx.send(res);
            }
        })?;

        // Spawn a task to process notify events and send batched changes
        tokio::spawn(async move {
            let mut debounce_buffer: HashMap<PathBuf, FileChange> = HashMap::new();
            let debounce_duration = Duration::from_millis(300);
            let mut debounce_timer = tokio::time::interval(debounce_duration);
            debounce_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Receive events from notify
                    Some(result) = notify_rx.recv() => {
                        if let Ok(event) = result {
                            for path in event.paths {
                                let change_type = match event.kind {
                                    EventKind::Create(_) => ChangeType::Created,
                                    EventKind::Modify(_) => ChangeType::Modified,
                                    EventKind::Remove(_) => ChangeType::Deleted,
                                    _ => continue,
                                };

                                for change in source_changes_for_path(&path, change_type.clone()) {
                                    // Add to debounce buffer (overwrites previous events for same file)
                                    debounce_buffer.insert(change.path.clone(), change);
                                }
                            }
                        }
                    }
                    // Debounce timer tick - flush buffered changes
                    _ = debounce_timer.tick() => {
                        if !debounce_buffer.is_empty() {
                            let changes: Vec<FileChange> = debounce_buffer.drain().map(|(_, v)| v).collect();
                            if tx.send(changes).await.is_err() {
                                // Receiver dropped, exit task
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                _watcher: watcher,
                watched_paths: Vec::new(),
            },
            rx,
        ))
    }

    /// Watch a directory for changes
    pub fn watch(&mut self, path: &Path) -> Result<()> {
        self._watcher.watch(path, RecursiveMode::Recursive)?;
        self.watched_paths.push(path.to_path_buf());
        info!("Async watching for changes: {:?}", path);
        Ok(())
    }

    /// Stop watching a directory
    pub fn unwatch(&mut self, path: &Path) -> Result<()> {
        self._watcher.unwatch(path)?;
        self.watched_paths.retain(|p| p != path);
        Ok(())
    }

    /// Get the list of watched paths
    pub fn watched_paths(&self) -> &[PathBuf] {
        &self.watched_paths
    }
}

/// A detected file change
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub change_type: ChangeType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeType {
    Created,
    Modified,
    Deleted,
}

/// Check if a path is a source file we care about
fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(crate::parser::is_supported_extension)
}

/// Convert a notify path into source-file changes.
///
/// Some platforms, especially macOS FSEvents and network/container mounts,
/// can report a directory as modified instead of the exact file. When that
/// happens, scan the reported directory for source files so watch mode does
/// not silently miss the change.
fn source_changes_for_path(path: &Path, change_type: ChangeType) -> Vec<FileChange> {
    if is_source_file(path) {
        return vec![FileChange {
            path: path.to_path_buf(),
            change_type,
        }];
    }

    if change_type == ChangeType::Deleted || !path.is_dir() {
        return Vec::new();
    }

    let mut changes = Vec::new();
    collect_source_files(path, change_type, &mut changes);
    changes
}

fn collect_source_files(path: &Path, change_type: ChangeType, changes: &mut Vec<FileChange>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        if is_source_file(&entry_path) {
            changes.push(FileChange {
                path: entry_path,
                change_type: change_type.clone(),
            });
        } else if entry_path.is_dir() {
            collect_source_files(&entry_path, change_type.clone(), changes);
        }
    }
}

/// Incremental indexer that combines persistence and watching
#[cfg(feature = "native")]
pub struct IncrementalIndexer {
    store: IndexStore,
    index: Arc<RwLock<PersistedIndex>>,
    watcher: Option<FileWatcher>,
}

#[cfg(feature = "native")]
impl IncrementalIndexer {
    pub fn new(index_dir: PathBuf, repo_root: &Path) -> Result<Self> {
        let store = IndexStore::new(index_dir)?;
        let index = store.load_or_create(repo_root)?;

        Ok(Self {
            store,
            index: Arc::new(RwLock::new(index)),
            watcher: None,
        })
    }

    /// Enable watch mode
    pub fn enable_watch(&mut self, repo_root: &Path) -> Result<()> {
        let mut watcher = FileWatcher::new()?;
        watcher.watch(repo_root)?;
        self.watcher = Some(watcher);
        Ok(())
    }

    /// Check for and process file changes
    pub fn process_changes<F>(&self, mut reindex_fn: F) -> Result<usize>
    where
        F: FnMut(&Path) -> Result<Vec<Symbol>>,
    {
        let changes = match &self.watcher {
            Some(w) => w.poll_changes(),
            None => return Ok(0),
        };

        if changes.is_empty() {
            return Ok(0);
        }

        let mut index = self.index.write();
        let mut count = 0;

        for change in changes {
            match change.change_type {
                ChangeType::Created | ChangeType::Modified => {
                    debug!("Re-indexing: {:?}", change.path);
                    match reindex_fn(&change.path) {
                        Ok(symbols) => {
                            index.update_file(change.path, symbols)?;
                            count += 1;
                        }
                        Err(e) => {
                            warn!("Failed to index {:?}: {}", change.path, e);
                        }
                    }
                }
                ChangeType::Deleted => {
                    debug!("Removing from index: {:?}", change.path);
                    index.remove_file(&change.path);
                    count += 1;
                }
            }
        }

        if count > 0 {
            self.store.save(&index)?;
        }

        Ok(count)
    }

    /// Get a read reference to the index
    pub fn index(&self) -> Arc<RwLock<PersistedIndex>> {
        Arc::clone(&self.index)
    }

    /// Force save the current index
    pub fn save(&self) -> Result<()> {
        let index = self.index.read();
        self.store.save(&index)
    }

    /// Get files that need re-indexing
    pub fn files_needing_reindex(&self) -> Result<Vec<PathBuf>> {
        let index = self.index.read();
        let mut needs_reindex = Vec::new();

        for path in index.files.keys() {
            if !path.exists() || index.needs_reindex(path)? {
                needs_reindex.push(path.clone());
            }
        }

        Ok(needs_reindex)
    }
}

/// Run the file watcher in background using an async event-driven loop.
///
/// The function exits cleanly when:
/// * The shutdown channel's only `Sender` is dropped (`recv()` returns
///   `Err(Closed)`), or
/// * A `()` value is sent on the shutdown channel.
///
/// **Bug history (issue #26):** the spawn site in `main.rs` used to drop the
/// shutdown sender immediately after creating it, so the receiver here saw
/// `Closed` on the first poll and the watcher exited milliseconds after
/// startup — silently disabling `--watch`. Use `spawn_watch_mode` (below)
/// from new call sites; it returns the sender so the caller cannot forget to
/// keep it alive.
pub async fn run_watch_mode(
    engine: Arc<crate::index::CodeIntelEngine>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    info!("Starting async watch mode background task");

    let (_watcher, mut rx) = match engine.create_async_file_watcher() {
        Some((w, r)) => (w, r),
        None => {
            warn!("Failed to create async file watcher, watch mode disabled");
            return;
        }
    };

    loop {
        tokio::select! {
            // Receive batched file change events
            Some(changes) = rx.recv() => {
                if !changes.is_empty() {
                    info!("Detected {} file change(s)", changes.len());
                    match engine.process_file_changes(&changes).await {
                        Ok(count) => {
                            if count > 0 {
                                info!("Re-indexed {} file(s)", count);
                            }
                        }
                        Err(e) => {
                            warn!("Error processing file changes: {}", e);
                        }
                    }
                }
            }
            // Handle shutdown signal (or all senders dropped)
            _ = shutdown.recv() => {
                info!("Watch mode shutting down");
                break;
            }
        }
    }
}

/// Spawn the watch-mode background task and return the shutdown `Sender`.
///
/// **Callers must hold the returned `Sender` for as long as the watcher
/// should keep running.** Dropping it makes the watcher loop exit on its
/// next poll (this is the cause of issue #26 — the original wiring dropped
/// the sender immediately).
///
/// The spawned task is detached; the returned `Sender` is the only handle
/// needed to keep the watcher alive.
#[must_use = "the returned Sender must be held until the watcher should stop; \
              dropping it immediately exits the watcher (issue #26)"]
pub fn spawn_watch_mode(
    engine: Arc<crate::index::CodeIntelEngine>,
) -> tokio::sync::broadcast::Sender<()> {
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
    tokio::spawn(async move {
        run_watch_mode(engine, shutdown_rx).await;
    });
    shutdown_tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_hash_consistency() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hello world").unwrap();

        let hash1 = hash_file(&file).unwrap();
        let hash2 = hash_file(&file).unwrap();
        assert_eq!(hash1, hash2);

        std::fs::write(&file, "hello world!").unwrap();
        let hash3 = hash_file(&file).unwrap();
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_is_source_file() {
        assert!(is_source_file(Path::new("foo.rs")));
        assert!(is_source_file(Path::new("bar.py")));
        assert!(is_source_file(Path::new("src/index.ts")));
        assert!(!is_source_file(Path::new("README.md")));
        assert!(!is_source_file(Path::new("data.json")));

        // Every language the parser supports must reach watch mode, not just
        // the 20 extensions this function used to hard-code.
        assert!(is_source_file(Path::new("Модуль.bsl")));
        assert!(is_source_file(Path::new("app.rb")));
        assert!(is_source_file(Path::new("Main.kt")));
        assert!(is_source_file(Path::new("deploy.sh")));
    }

    #[test]
    fn test_index_store() {
        let dir = tempdir().unwrap();
        let store = IndexStore::new(dir.path().to_path_buf()).unwrap();

        let repo = tempdir().unwrap();
        let index = PersistedIndex::new(repo.path().to_path_buf());

        store.save(&index).unwrap();

        let loaded = store.load_or_create(repo.path()).unwrap();
        assert_eq!(loaded.version, PersistedIndex::CURRENT_VERSION);
    }

    #[test]
    fn test_index_store_roundtrip_symbols() {
        use crate::symbols::{Symbol, SymbolKind};

        let dir = tempdir().unwrap();
        let store = IndexStore::new(dir.path().to_path_buf()).unwrap();

        let repo = tempdir().unwrap();
        let file = repo.path().join("a.rs");
        std::fs::write(&file, "fn foo() {}").unwrap();

        let mut index = PersistedIndex::new(repo.path().to_path_buf());
        let symbols = vec![Symbol {
            name: "foo".into(),
            kind: SymbolKind::Function,
            file_path: "a.rs".into(),
            start_line: 1,
            end_line: 1,
            signature: Some("fn foo()".into()),
            qualified_name: Some("foo".into()),
            doc_comment: None,
        }];
        index.files.insert(
            file.clone(),
            FileMetadata {
                path: file,
                content_hash: "abc".into(),
                modified_time: 123,
                size: 11,
                symbols,
            },
        );

        store.save(&index).unwrap();

        let loaded = store.load_or_create(repo.path()).unwrap();
        assert_eq!(loaded.files.len(), 1);
        let meta = loaded.files.values().next().unwrap();
        assert_eq!(meta.symbols.len(), 1);
        assert_eq!(meta.symbols[0].name, "foo");
        assert_eq!(meta.symbols[0].file_path, "a.rs");
        assert_eq!(meta.content_hash, "abc");
        assert_eq!(meta.modified_time, 123);
        assert_eq!(loaded.repo_root, repo.path().to_path_buf());

        // Timestamps must survive the round-trip (they feed `last_indexed`).
        assert_eq!(loaded.created_at, index.created_at);
        assert_eq!(loaded.updated_at, index.updated_at);

        // list_cached must discover the repo via the meta table.
        let cached = store.list_cached().unwrap();
        assert_eq!(cached, vec![repo.path().to_path_buf()]);
    }
}
