//! Repository management utilities
//!
//! Handles repository discovery, validation, and configuration.

// Allow dead code for planned config features
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Configuration for a repository
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Repository name
    pub name: String,

    /// Path to repository root
    pub path: PathBuf,

    /// Patterns to exclude from indexing
    pub exclude_patterns: Vec<String>,

    /// Patterns to include (if empty, include all)
    pub include_patterns: Vec<String>,

    /// Maximum file size to index (bytes)
    pub max_file_size: u64,

    /// Whether to follow symlinks
    pub follow_symlinks: bool,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            path: PathBuf::new(),
            exclude_patterns: vec![
                "**/node_modules/**".to_string(),
                "**/target/**".to_string(),
                "**/.git/**".to_string(),
                "**/vendor/**".to_string(),
                "**/__pycache__/**".to_string(),
                "**/dist/**".to_string(),
                "**/build/**".to_string(),
                "**/*.min.js".to_string(),
                "**/*.min.css".to_string(),
                "**/package-lock.json".to_string(),
                "**/yarn.lock".to_string(),
                "**/Cargo.lock".to_string(),
            ],
            include_patterns: vec![],
            max_file_size: 1024 * 1024, // 1MB
            follow_symlinks: false,
        }
    }
}

/// Discover repositories in a directory
pub fn discover_repos(base_path: &Path, max_depth: usize) -> Result<Vec<PathBuf>> {
    let mut repos = Vec::new();
    let mut seen = HashSet::new();

    discover_standard_repos_recursive(base_path, 0, max_depth, &mut repos, &mut seen)?;
    discover_onec_repos(base_path, max_depth, &mut repos, &mut seen);

    Ok(repos)
}

fn discover_standard_repos_recursive(
    path: &Path,
    depth: usize,
    max_depth: usize,
    repos: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    if depth > max_depth {
        return Ok(());
    }

    // Keep the legacy repo discovery behavior for VCS/project roots.
    if is_standard_repository(path) {
        push_repo(path, repos, seen);
        return Ok(()); // Don't recurse into repos
    }

    // Recurse into subdirectories
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let entry_path = entry.path();

            if entry_path.is_dir() {
                let name = entry_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");

                // Skip hidden directories
                if !name.starts_with('.') {
                    discover_standard_repos_recursive(
                        &entry_path,
                        depth + 1,
                        max_depth,
                        repos,
                        seen,
                    )?;
                }
            }
        }
    }

    Ok(())
}

/// Check if a directory is a repository root
pub fn is_repository(path: &Path) -> bool {
    is_standard_repository(path) || OneCRepositoryDetector::new().detect_root(path).is_some()
}

fn is_standard_repository(path: &Path) -> bool {
    // Check for common VCS directories
    if path.join(".git").exists() {
        return true;
    }

    // Check for common project files
    let project_markers = [
        "Cargo.toml",     // Rust
        "package.json",   // Node.js
        "pyproject.toml", // Python
        "setup.py",       // Python
        "go.mod",         // Go
        "pom.xml",        // Java/Maven
        "build.gradle",   // Java/Gradle
        "CMakeLists.txt", // C/C++
        "Makefile",       // Generic
        ".project",       // Eclipse
        "*.sln",          // .NET
    ];

    for marker in &project_markers {
        if marker.contains('*') {
            // Glob pattern
            if let Ok(entries) = glob::glob(&path.join(marker).to_string_lossy()) {
                if entries.filter_map(|e| e.ok()).count() > 0 {
                    return true;
                }
            }
        } else if path.join(marker).exists() {
            return true;
        }
    }

    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneCDetectionReason {
    ConfigurationXml,
    ConfigDumpInfoXml,
    MetadataLayout,
    MetadataObjectWithExt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneCRepositoryMatch {
    pub root: PathBuf,
    pub reason: OneCDetectionReason,
}

/// Detects 1C configuration dump roots in whole repositories and nested subtrees.
#[derive(Debug, Clone)]
pub struct OneCRepositoryDetector;

impl Default for OneCRepositoryDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl OneCRepositoryDetector {
    pub fn new() -> Self {
        Self
    }

    pub fn detect_root(&self, path: &Path) -> Option<OneCRepositoryMatch> {
        if !path.is_dir() {
            return None;
        }

        if path.join("Configuration.xml").is_file() {
            return Some(OneCRepositoryMatch {
                root: path.to_path_buf(),
                reason: OneCDetectionReason::ConfigurationXml,
            });
        }

        if path.join("ConfigDumpInfo.xml").is_file() {
            return Some(OneCRepositoryMatch {
                root: path.to_path_buf(),
                reason: OneCDetectionReason::ConfigDumpInfoXml,
            });
        }

        if has_known_metadata_layout(path) {
            return Some(OneCRepositoryMatch {
                root: path.to_path_buf(),
                reason: OneCDetectionReason::MetadataLayout,
            });
        }

        if has_metadata_object_with_ext(path) {
            return Some(OneCRepositoryMatch {
                root: path.to_path_buf(),
                reason: OneCDetectionReason::MetadataObjectWithExt,
            });
        }

        None
    }
}

fn discover_onec_repos(
    base_path: &Path,
    max_depth: usize,
    repos: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) {
    let detector = OneCRepositoryDetector::new();

    for entry in WalkDir::new(base_path)
        .max_depth(max_depth.saturating_add(1))
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_scan_dir(entry.path(), base_path))
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }

        if let Some(matched) = detector.detect_root(entry.path()) {
            push_repo(&matched.root, repos, seen);
        }
    }
}

fn should_scan_dir(path: &Path, base_path: &Path) -> bool {
    if path == base_path {
        return true;
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| !name.starts_with('.'))
        .unwrap_or(true)
}

fn push_repo(path: &Path, repos: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    let path = path.to_path_buf();
    if seen.insert(path.clone()) {
        repos.push(path);
    }
}

fn has_known_metadata_layout(path: &Path) -> bool {
    const KNOWN_METADATA_DIRS: &[&str] = &[
        "Catalogs",
        "Documents",
        "CommonModules",
        "Reports",
        "DataProcessors",
        "InformationRegisters",
        "AccumulationRegisters",
        "AccountingRegisters",
        "CalculationRegisters",
        "ExchangePlans",
        "ChartsOfCharacteristicTypes",
        "ChartsOfAccounts",
        "BusinessProcesses",
        "Tasks",
        "Constants",
        "Enums",
        "Subsystems",
        "Roles",
        "CommonForms",
        "CommonCommands",
        "ScheduledJobs",
        "HTTPServices",
        "WebServices",
        "WSReferences",
        "XDTOPackages",
    ];

    KNOWN_METADATA_DIRS
        .iter()
        .filter(|dir| path.join(dir).is_dir())
        .take(2)
        .count()
        >= 2
}

fn has_metadata_object_with_ext(path: &Path) -> bool {
    let ext_dir = path.join("Ext");
    if !ext_dir.is_dir() {
        return false;
    }

    let object_name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) if !name.is_empty() => name,
        _ => return false,
    };

    let metadata_xml = path.join(format!("{object_name}.xml"));
    if !metadata_xml.is_file() {
        return false;
    }

    ext_dir.join("ObjectModule.bsl").is_file()
        || ext_dir.join("ManagerModule.bsl").is_file()
        || ext_dir.join("RecordSetModule.bsl").is_file()
        || ext_dir.join("ValueManagerModule.bsl").is_file()
        || ext_dir.join("Module.bsl").is_file()
}

/// Get the repository name from a path
pub fn repo_name_from_path(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

/// Validate a repository path
pub fn validate_repo_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("Path does not exist: {:?}", path));
    }

    if !path.is_dir() {
        return Err(anyhow!("Path is not a directory: {:?}", path));
    }

    // Check if readable
    std::fs::read_dir(path).map_err(|e| anyhow!("Cannot read directory {:?}: {}", path, e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_is_repository_git() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(is_repository(dir.path()));
    }

    #[test]
    fn test_is_repository_cargo() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert!(is_repository(dir.path()));
    }

    #[test]
    fn test_is_not_repository() {
        let dir = tempdir().unwrap();
        assert!(!is_repository(dir.path()));
    }

    #[test]
    fn test_is_repository_onec_by_configuration_xml() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Configuration.xml"), "<MetaDataObject/>").unwrap();

        assert!(is_repository(dir.path()));
    }

    #[test]
    fn test_detect_onec_root_by_metadata_layout() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("Catalogs")).unwrap();
        fs::create_dir(dir.path().join("Documents")).unwrap();

        let detected = OneCRepositoryDetector::new().detect_root(dir.path());

        assert!(detected.is_some());
        assert_eq!(
            detected.unwrap().reason,
            OneCDetectionReason::MetadataLayout
        );
    }

    #[test]
    fn test_detect_onec_root_by_ext_object_pattern() {
        let dir = tempdir().unwrap();
        let object_dir = dir.path().join("Catalogs").join("Products");
        fs::create_dir_all(object_dir.join("Ext")).unwrap();
        fs::write(object_dir.join("Products.xml"), "<MetaDataObject/>").unwrap();
        fs::write(
            object_dir.join("Ext").join("ManagerModule.bsl"),
            "Procedure Test() EndProcedure",
        )
        .unwrap();

        let detected = OneCRepositoryDetector::new().detect_root(&object_dir);

        assert!(detected.is_some());
        assert_eq!(
            detected.unwrap().reason,
            OneCDetectionReason::MetadataObjectWithExt
        );
    }

    #[test]
    fn test_discover_repos_includes_nested_onec_dump_in_git_repo() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();

        let onec_root = dir.path().join("vendor").join("erp_dump");
        fs::create_dir_all(&onec_root).unwrap();
        fs::write(onec_root.join("Configuration.xml"), "<MetaDataObject/>").unwrap();

        let repos = discover_repos(dir.path(), 3).unwrap();

        assert!(repos.contains(&dir.path().to_path_buf()));
        assert!(repos.contains(&onec_root));
    }

    #[test]
    fn test_discover_repos_does_not_match_ext_without_metadata() {
        let dir = tempdir().unwrap();
        let candidate = dir.path().join("Catalogs").join("Products");
        fs::create_dir_all(candidate.join("Ext")).unwrap();
        fs::write(
            candidate.join("Ext").join("ManagerModule.bsl"),
            "Procedure Test() EndProcedure",
        )
        .unwrap();

        assert!(OneCRepositoryDetector::new()
            .detect_root(&candidate)
            .is_none());
        assert!(!is_repository(candidate.as_path()));
    }
}
