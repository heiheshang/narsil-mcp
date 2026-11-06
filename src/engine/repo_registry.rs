//! Repository roots the engine knows about, and the key each one is indexed
//! under. Every per-repository map in the engine is keyed by that key.

use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoKind {
    Primary,
    Worktree {
        primary_key: String,
        branch: Option<String>,
        head: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoState {
    Registered,
    Indexing,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct RepoEntry {
    pub key: String,
    pub path: PathBuf,
    pub kind: RepoKind,
    pub state: RepoState,
}

impl RepoEntry {
    pub fn is_worktree(&self) -> bool {
        matches!(self.kind, RepoKind::Worktree { .. })
    }

    pub fn branch(&self) -> Option<&str> {
        match &self.kind {
            RepoKind::Worktree { branch, .. } => branch.as_deref(),
            RepoKind::Primary => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct RepoRegistry {
    entries: RwLock<Vec<RepoEntry>>,
}

/// `/` and `\` would make the key look like a path to the repo resolvers.
pub fn sanitize_key_part(part: &str) -> String {
    part.chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect()
}

fn basename(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

impl RepoRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_primary(&self, path: &Path) -> String {
        self.register(path, sanitize_key_part(&basename(path)), RepoKind::Primary)
    }

    pub fn register_worktree(
        &self,
        primary_key: &str,
        path: &Path,
        branch: Option<String>,
        head: String,
    ) -> String {
        let wanted = format!("{primary_key}@{}", sanitize_key_part(&basename(path)));
        self.register(
            path,
            wanted,
            RepoKind::Worktree {
                primary_key: primary_key.to_string(),
                branch,
                head,
            },
        )
    }

    /// Registering a path twice returns the key it already has.
    fn register(&self, path: &Path, wanted: String, kind: RepoKind) -> String {
        let mut entries = self.entries.write();
        if let Some(existing) = entries.iter_mut().find(|e| e.path == path) {
            if let (RepoKind::Worktree { .. }, RepoKind::Worktree { .. }) = (&existing.kind, &kind)
            {
                existing.kind = kind;
            }
            return existing.key.clone();
        }
        let mut key = wanted.clone();
        let mut suffix = 2;
        while entries.iter().any(|e| e.key == key) {
            key = format!("{wanted}~{suffix}");
            suffix += 1;
        }
        if key != wanted {
            warn!(key = %key, wanted = %wanted, path = ?path, "repo key collision, suffixed");
        }
        debug!(key = %key, path = ?path, kind = ?kind, "repo registered");
        entries.push(RepoEntry {
            key: key.clone(),
            path: path.to_path_buf(),
            kind,
            state: RepoState::Registered,
        });
        key
    }

    pub fn snapshot(&self) -> Vec<RepoEntry> {
        self.entries.read().clone()
    }

    pub fn primaries(&self) -> Vec<RepoEntry> {
        self.entries
            .read()
            .iter()
            .filter(|e| !e.is_worktree())
            .cloned()
            .collect()
    }

    pub fn entry(&self, key: &str) -> Option<RepoEntry> {
        self.entries.read().iter().find(|e| e.key == key).cloned()
    }

    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Repositories whose documents are, or are being put, in the shared indexes.
    pub fn indexed_count(&self) -> usize {
        self.entries
            .read()
            .iter()
            .filter(|e| {
                !e.is_worktree() || matches!(e.state, RepoState::Indexing | RepoState::Ready)
            })
            .count()
    }

    pub fn set_state(&self, key: &str, state: RepoState) {
        if let Some(entry) = self.entries.write().iter_mut().find(|e| e.key == key) {
            debug!(key = %key, from = ?entry.state, to = ?state, "repo state");
            entry.state = state;
        }
    }

    pub fn unregister(&self, key: &str) -> Option<RepoEntry> {
        let mut entries = self.entries.write();
        let index = entries.iter().position(|e| e.key == key)?;
        Some(entries.remove(index))
    }

    /// Key of the root `path` was registered with, compared as given.
    pub fn key_for_root(&self, path: &Path) -> Option<String> {
        self.entries
            .read()
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.key.clone())
    }

    /// The innermost registered repository containing `path`: a worktree
    /// nested in its primary's tree belongs to the worktree.
    pub fn owner_of(&self, path: &Path) -> Option<RepoEntry> {
        self.entries
            .read()
            .iter()
            .filter(|e| super::path_is_within_repo(path, &e.path))
            .max_by_key(|e| e.path.components().count())
            .cloned()
    }

    /// The key itself, or `<primary>@<branch>` with the branch sanitized.
    pub fn resolve_alias(&self, name: &str) -> Option<String> {
        let entries = self.entries.read();
        if entries.iter().any(|e| e.key == name) {
            return Some(name.to_string());
        }
        let (primary, wanted) = name.split_once('@')?;
        entries
            .iter()
            .find(|e| match &e.kind {
                RepoKind::Worktree {
                    primary_key,
                    branch: Some(branch),
                    ..
                } => primary_key == primary && sanitize_key_part(branch) == wanted,
                _ => false,
            })
            .map(|e| e.key.clone())
    }

    /// Roots of other registered repositories lying inside `path`.
    pub fn nested_roots(&self, path: &Path) -> Vec<PathBuf> {
        self.entries
            .read()
            .iter()
            .filter(|e| e.path != path && e.path.starts_with(path))
            .map(|e| e.path.clone())
            .collect()
    }

    /// Whether `file` lies in another registered repository nested inside
    /// `root`, and so is not `root`'s.
    pub fn in_nested_repo(&self, root: &Path, file: &Path) -> bool {
        self.entries
            .read()
            .iter()
            .any(|e| e.path != root && e.path.starts_with(root) && file.starts_with(&e.path))
    }

    pub fn worktrees_of(&self, primary: &str) -> Vec<RepoEntry> {
        self.entries
            .read()
            .iter()
            .filter(|e| matches!(&e.kind, RepoKind::Worktree { primary_key, .. } if primary_key == primary))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_key_is_basename() {
        let registry = RepoRegistry::new();
        assert_eq!(registry.register_primary(Path::new("/src/app")), "app");
    }

    #[test]
    fn same_basename_gets_suffix() {
        let registry = RepoRegistry::new();
        assert_eq!(registry.register_primary(Path::new("/a/app")), "app");
        assert_eq!(registry.register_primary(Path::new("/b/app")), "app~2");
        assert_eq!(registry.register_primary(Path::new("/c/app")), "app~3");
    }

    #[test]
    fn registering_same_path_twice_keeps_key() {
        let registry = RepoRegistry::new();
        registry.register_primary(Path::new("/src/app"));
        let first = registry.register_worktree("app", Path::new("/wt/x"), None, "abc".into());
        let again = registry.register_worktree(
            "app",
            Path::new("/wt/x"),
            Some("feature/x".into()),
            "def".into(),
        );
        assert_eq!(first, "app@x");
        assert_eq!(again, "app@x");
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.entry("app@x").unwrap().branch(), Some("feature/x"));
    }

    #[test]
    fn worktree_key_and_branch_alias() {
        let registry = RepoRegistry::new();
        registry.register_primary(Path::new("/src/app"));
        let key = registry.register_worktree(
            "app",
            Path::new("/src/app/.claude/worktrees/fix-x"),
            Some("fix/x".into()),
            "abc".into(),
        );
        assert_eq!(key, "app@fix-x");
        assert_eq!(
            registry.resolve_alias("app@fix-x").as_deref(),
            Some("app@fix-x")
        );
        assert_eq!(registry.resolve_alias("app@nope"), None);
        assert_eq!(registry.resolve_alias("other@fix-x"), None);
    }

    #[test]
    fn branch_alias_differs_from_dir_name() {
        let registry = RepoRegistry::new();
        registry.register_primary(Path::new("/src/app"));
        registry.register_worktree(
            "app",
            Path::new("/tmp/wt1"),
            Some("feature/login".into()),
            "abc".into(),
        );
        assert_eq!(
            registry.resolve_alias("app@feature-login").as_deref(),
            Some("app@wt1")
        );
    }

    #[test]
    fn owner_is_innermost_root() {
        let registry = RepoRegistry::new();
        registry.register_primary(Path::new("/src/app"));
        registry.register_worktree("app", Path::new("/src/app/.worktrees/x"), None, "a".into());
        let owner = registry
            .owner_of(Path::new("/src/app/.worktrees/x/lib.rs"))
            .unwrap();
        assert_eq!(owner.key, "app@x");
        assert_eq!(
            registry.owner_of(Path::new("/src/app/lib.rs")).unwrap().key,
            "app"
        );
        assert!(registry.owner_of(Path::new("/elsewhere/lib.rs")).is_none());
        assert!(registry.in_nested_repo(
            Path::new("/src/app"),
            Path::new("/src/app/.worktrees/x/lib.rs")
        ));
        assert!(!registry.in_nested_repo(
            Path::new("/src/app/.worktrees/x"),
            Path::new("/src/app/.worktrees/x/lib.rs")
        ));
        assert!(!registry.in_nested_repo(Path::new("/src/app"), Path::new("/src/app/lib.rs")));
        assert_eq!(
            registry.nested_roots(Path::new("/src/app")),
            vec![PathBuf::from("/src/app/.worktrees/x")]
        );
    }

    #[test]
    fn indexed_count_ignores_unindexed_worktrees() {
        let registry = RepoRegistry::new();
        registry.register_primary(Path::new("/src/app"));
        registry.register_worktree("app", Path::new("/wt/x"), None, "a".into());
        assert_eq!(registry.indexed_count(), 1);
        registry.set_state("app@x", RepoState::Indexing);
        assert_eq!(registry.indexed_count(), 2);
        registry.unregister("app@x");
        assert_eq!(registry.indexed_count(), 1);
        assert_eq!(registry.len(), 1);
    }
}
