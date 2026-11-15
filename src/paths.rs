//! Repository-relative paths rendered the same on every platform.

use std::borrow::Cow;
use std::path::Path;

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
    fn backslashes_become_slashes_on_windows() {
        assert_eq!(Path::new(r"src\main.rs").to_slash_lossy(), "src/main.rs");
    }
}
