//! The location of the cache directory.
//!
//! The prod server uses `~/.cache/recap`. The test server (`recap serve
//! --test`) uses `~/.cache/recap-test`, which holds a copy of the prod cache.
//! The server sets the root once at start with [`set_cache_root`], before any
//! loader runs. Every module reads the cache root through [`cache_root`], so
//! the test server never writes to the prod cache.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static CACHE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The prod cache root: `$HOME/.cache/recap`.
pub fn prod_cache_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/recap")
}

/// The test cache root: `$HOME/.cache/recap-test`.
pub fn test_cache_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".cache/recap-test")
}

/// Set the cache root. Call it once, before the first call to
/// [`cache_root`]. It returns an error if the root is already set to a
/// different path.
pub fn set_cache_root(root: PathBuf) -> anyhow::Result<()> {
    let current = CACHE_ROOT.get_or_init(|| root.clone());
    if *current != root {
        anyhow::bail!(
            "the cache root is already {}, not {}",
            current.display(),
            root.display()
        );
    }
    Ok(())
}

/// The cache root of this process. It is the prod root if the process did
/// not call [`set_cache_root`].
pub fn cache_root() -> &'static PathBuf {
    CACHE_ROOT.get_or_init(prod_cache_root)
}

/// The folders in the home directory for which macOS asks the user before a
/// process reads a file. A server that launchd started gets no answer until
/// the user sees the prompt, and `open` waits for that answer. macOS asks
/// again after each new build of the binary.
const PROTECTED_HOME_FOLDERS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Library/Mobile Documents",
    "Library/CloudStorage",
    "Library/Containers",
    "Library/Group Containers",
];

/// Whether macOS can ask the user before a process reads `path`: a file in
/// a [`PROTECTED_HOME_FOLDERS`] folder, or a file on another volume. The
/// loaders do not read these files.
pub fn in_protected_folder(path: &Path) -> bool {
    if path.starts_with("/Volumes") {
        return true;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    PROTECTED_HOME_FOLDERS
        .iter()
        .any(|folder| path.starts_with(home.join(folder)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_in_one_of_the_protected_folders() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        assert!(in_protected_folder(&home.join("Downloads/a.txt")));
        assert!(in_protected_folder(&home.join("Library/Mobile Documents/x/b.sh")));
        assert!(in_protected_folder(Path::new("/Volumes/usb/c.rs")));
        assert!(!in_protected_folder(&home.join("git/recap/src/main.rs")));
        assert!(!in_protected_folder(&home.join("DownloadsOld/a.txt")));
    }
}
