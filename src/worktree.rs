//! Discovery of linked git worktrees: `git worktree list --porcelain`.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, warn};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub prunable: bool,
}

pub fn parse_porcelain(output: &str) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in output.lines() {
        if line.is_empty() {
            worktrees.extend(current.take());
            continue;
        }
        let (field, value) = line.split_once(' ').unwrap_or((line, ""));
        if field == "worktree" {
            worktrees.extend(current.take());
            current = Some(WorktreeInfo {
                path: PathBuf::from(value),
                ..WorktreeInfo::default()
            });
            continue;
        }
        let Some(info) = current.as_mut() else {
            continue;
        };
        match field {
            "HEAD" => info.head = value.to_string(),
            "branch" => {
                info.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                )
            }
            "detached" => info.detached = true,
            "bare" => info.bare = true,
            "locked" => info.locked = true,
            "prunable" => info.prunable = true,
            _ => {}
        }
    }
    worktrees.extend(current);
    worktrees
}

fn git_output(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed in {:?}: {}",
            args.join(" "),
            dir,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Linked worktrees of the repository at `primary`, without `primary`
/// itself and without bare, prunable or vanished ones. An error means git
/// could not tell, not that there are none.
pub fn list_worktrees(primary: &Path) -> Result<Vec<WorktreeInfo>> {
    let output = git_output(primary, &["worktree", "list", "--porcelain"])?;
    let primary = canonical(primary);
    let worktrees: Vec<WorktreeInfo> = parse_porcelain(&output)
        .into_iter()
        .filter_map(|mut info| {
            let reason = if info.bare {
                Some("bare")
            } else if info.prunable {
                Some("prunable")
            } else if !info.path.is_dir() {
                Some("missing")
            } else {
                None
            };
            if let Some(reason) = reason {
                debug!(path = ?info.path, reason, "worktree skipped");
                return None;
            }
            info.path = canonical(&info.path);
            (info.path != primary).then_some(info)
        })
        .collect();
    debug!(primary = ?primary, count = worktrees.len(), "worktrees discovered");
    Ok(worktrees)
}

/// The shared `.git` directory of the repository `path` belongs to; the same
/// for a primary checkout and all its worktrees.
pub fn common_dir(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() { path } else { path.parent()? };
    match git_output(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ) {
        Ok(out) if !out.is_empty() => Some(canonical(Path::new(&out))),
        Ok(_) => None,
        Err(e) => {
            debug!(path = ?path, error = %e, "not inside a git repository");
            None
        }
    }
}

/// Root of the checkout (primary or worktree) containing `path`.
pub fn checkout_root(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() { path } else { path.parent()? };
    git_output(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .filter(|out| !out.is_empty())
        .map(|out| canonical(Path::new(&out)))
}

/// Whether git can be run at all; discovery is skipped without it.
pub fn git_available() -> bool {
    match Command::new("git").arg("--version").output() {
        Ok(output) => output.status.success(),
        Err(e) => {
            warn!(error = %e, "git not found; worktree discovery disabled");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORCELAIN: &str = "\
worktree /src/app
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /src/app/.claude/worktrees/fix-x
HEAD 2222222222222222222222222222222222222222
branch refs/heads/fix/x

worktree /tmp/detached
HEAD 3333333333333333333333333333333333333333
detached

worktree /tmp/locked
HEAD 4444444444444444444444444444444444444444
branch refs/heads/locked-branch
locked reason here

worktree /tmp/gone
HEAD 5555555555555555555555555555555555555555
branch refs/heads/gone
prunable gitdir file points to non-existent location
";

    #[test]
    fn parses_every_block() {
        let worktrees = parse_porcelain(PORCELAIN);
        assert_eq!(worktrees.len(), 5);
        assert_eq!(worktrees[0].path, PathBuf::from("/src/app"));
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert_eq!(worktrees[1].branch.as_deref(), Some("fix/x"));
        assert_eq!(
            worktrees[1].head,
            "2222222222222222222222222222222222222222"
        );
        assert!(worktrees[2].detached);
        assert_eq!(worktrees[2].branch, None);
        assert!(worktrees[3].locked);
        assert!(worktrees[4].prunable);
    }

    #[test]
    fn parses_bare_primary() {
        let worktrees = parse_porcelain(
            "worktree /srv/repo.git\nbare\n\nworktree /srv/wt\nHEAD abc\nbranch refs/heads/dev\n",
        );
        assert_eq!(worktrees.len(), 2);
        assert!(worktrees[0].bare);
        assert_eq!(worktrees[1].branch.as_deref(), Some("dev"));
    }

    #[test]
    fn list_worktrees_without_git_repo_is_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_worktrees(dir.path()).is_err());
        assert!(common_dir(dir.path()).is_none());
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn lists_real_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("app");
        std::fs::create_dir(&primary).unwrap();
        git(&primary, &["init", "-q"]);
        std::fs::write(primary.join("a.rs"), "fn a() {}\n").unwrap();
        git(&primary, &["add", "."]);
        git(&primary, &["commit", "-q", "-m", "init"]);
        let wt = dir.path().join("wt");
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature/x",
            ],
        );

        let worktrees = list_worktrees(&primary).unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].path, wt.canonicalize().unwrap());
        assert_eq!(worktrees[0].branch.as_deref(), Some("feature/x"));
        assert_eq!(common_dir(&wt), common_dir(&primary));
        assert!(common_dir(&primary).is_some());
        assert_eq!(
            checkout_root(&wt.join("a.rs")),
            Some(wt.canonicalize().unwrap())
        );

        std::fs::remove_dir_all(&wt).unwrap();
        assert!(list_worktrees(&primary).unwrap().is_empty());
    }
}
