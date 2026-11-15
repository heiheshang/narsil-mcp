//! Repository-relative paths rendered the same on every platform.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

pub trait SlashPath {
    /// `to_string_lossy` with `/` between components on Windows too: relative
    /// paths are index keys and tool output, and must not depend on the OS.
    fn to_slash_lossy(&self) -> Cow<'_, str>;
}

impl SlashPath for Path {
    fn to_slash_lossy(&self) -> Cow<'_, str> {
        let lossy = self.to_string_lossy();
        if cfg!(windows) && lossy.contains('\\') {
            Cow::Owned(lossy.replace('\\', "/"))
        } else {
            lossy
        }
    }
}

/// `canonicalize`, falling back to `path` itself, without the Windows
/// verbatim prefix `\\?\`: git rejects it, and roots the user passes in
/// never carry it, so the two would not compare equal.
pub fn canonical(path: &Path) -> PathBuf {
    strip_verbatim(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        if let Some(plain) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            if !plain.starts_with(r"UNC\") {
                return PathBuf::from(plain);
            }
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn joined_components_use_forward_slashes() {
        let path: PathBuf = ["onec", "Catalogs", "Configuration.xml"].iter().collect();
        assert_eq!(path.to_slash_lossy(), "onec/Catalogs/Configuration.xml");
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_drive_prefix_is_dropped() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\repo")),
            PathBuf::from(r"C:\repo")
        );
        let unc = PathBuf::from(r"\\?\UNC\server\share");
        assert_eq!(strip_verbatim(unc.clone()), unc);
    }

    #[cfg(windows)]
    #[test]
    fn backslashes_become_slashes_on_windows() {
        assert_eq!(Path::new(r"src\main.rs").to_slash_lossy(), "src/main.rs");
    }
}
