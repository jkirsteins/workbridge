//! Path helpers, atomic-write, and filesystem repo discovery for the
//! config module. Kept separate from the schema types so `config/mod.rs`
//! stays focused on the serialization surface.

use std::fs;
use std::path::{Path, PathBuf};

use super::ConfigError;

/// Get the user's home directory via the side-effects gate. Returns
/// `None` under `cfg(test)` so tests cannot reach the real `$HOME`.
#[cfg_attr(
    test,
    expect(
        clippy::missing_const_for_fn,
        reason = "delegates to a side-effects gate that is `const` only under cfg(test); under cfg(not(test)) the body calls a non-const user-dirs helper and cannot be const"
    )
)]
pub(super) fn home_dir() -> Option<PathBuf> {
    crate::side_effects::paths::home_dir()
}

/// Return the platform-specific config file path.
///
/// macOS: ~/Library/Application Support/workbridge/config.toml
/// Linux: ~/.config/workbridge/config.toml
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let proj = crate::side_effects::paths::project_dirs().ok_or(ConfigError::NoConfigDir)?;
    Ok(proj.config_dir().join("config.toml"))
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    if path == "~"
        && let Some(home) = home_dir()
    {
        return home;
    }
    PathBuf::from(path)
}

/// Canonicalize a path, resolving symlinks and `.`/`..` components.
///
/// On macOS, `/tmp` is a symlink to `/private/tmp`. The `/private` prefix
/// is an implementation detail that leaks into displayed paths and breaks
/// snapshot tests across platforms. This function strips the `/private`
/// prefix when the shortened path still exists, keeping paths consistent
/// between macOS and Linux.
pub fn canonicalize_path(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;

    #[cfg(target_os = "macos")]
    {
        if let Ok(rest) = canonical.strip_prefix("/private") {
            let shortened = Path::new("/").join(rest);
            if shortened.exists() {
                return Ok(shortened);
            }
        }
    }

    Ok(canonical)
}

/// Collapse the user's home directory back to `~` for display and storage.
pub fn collapse_home(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Normalize a repo path to a consistent form: expand tilde, canonicalize
/// if possible, then collapse back to ~/... for storage. This ensures that
/// `./repo`, `~/repo`, and `/abs/path/repo` all produce the same string.
pub fn normalize_repo_path(path: &str) -> String {
    let expanded = expand_tilde(path);
    let canonical = canonicalize_path(&expanded).unwrap_or(expanded);
    collapse_home(&canonical)
}

/// Write data to a file atomically by writing to a temp file in the same
/// directory and then renaming. On POSIX, rename within the same filesystem
/// is atomic, so a crash mid-write leaves the original file intact.
pub(super) fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .map_or_else(|| "config".into(), |n| n.to_string_lossy().into_owned())
    ));
    fs::write(&tmp_path, data)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Scan a directory one level deep for subdirectories containing `.git/`.
pub(super) fn discover_git_repos_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut repos = Vec::new();
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() && child.join(".git").exists() {
            repos.push(child);
        }
    }
    repos.sort();
    repos
}

#[cfg(test)]
mod path_tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{discover_git_repos_in, expand_tilde};

    #[test]
    fn expand_tilde_with_home() {
        // Under cfg(test), side_effects::paths::home_dir() returns None
        // (by design - tests must not reach the real $HOME). So
        // expand_tilde leaves "~/..." unchanged. This test asserts that
        // contract: when home_dir() returns None the input path is
        // returned verbatim.
        let expanded = expand_tilde("~/Projects");
        assert_eq!(expanded, PathBuf::from("~/Projects"));
    }

    #[test]
    fn expand_tilde_absolute_unchanged() {
        let expanded = expand_tilde("/tmp/foo");
        assert_eq!(expanded, PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn expand_tilde_bare() {
        // Same contract as expand_tilde_with_home: home_dir() returns
        // None under cfg(test), so "~" passes through unchanged.
        let expanded = expand_tilde("~");
        assert_eq!(expanded, PathBuf::from("~"));
    }

    #[test]
    fn discover_repos_finds_git_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        fs::create_dir_all(dir.join("repo-a/.git")).unwrap();
        fs::create_dir_all(dir.join("repo-b/.git")).unwrap();
        fs::create_dir_all(dir.join("not-a-repo")).unwrap();

        let found = discover_git_repos_in(&dir);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|p| p.ends_with("repo-a")));
        assert!(found.iter().any(|p| p.ends_with("repo-b")));
    }

    #[test]
    fn discover_repos_empty_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

        let found = discover_git_repos_in(&dir);
        assert!(found.is_empty());
    }

    #[test]
    fn discover_repos_nonexistent_dir() {
        let found = discover_git_repos_in(Path::new("/nonexistent/path"));
        assert!(found.is_empty());
    }
}

/// Test-only helpers. Gated behind `#[cfg(test)]` so the production
/// build never sees these items, which keeps `dead_code` clean without
/// needing source-level `#[allow]` attributes. Tests import
/// `crate::config::InMemoryConfigProvider` via the `pub use` re-export
/// in `super`, so existing call sites do not need updating.
#[cfg(test)]
pub mod test_support {
    use std::sync::Mutex;

    use super::super::{Config, ConfigError, ConfigProvider};

    /// In-memory config provider for tests. Never touches disk.
    pub struct InMemoryConfigProvider {
        data: Mutex<Option<String>>,
    }

    impl InMemoryConfigProvider {
        pub fn new() -> Self {
            Self {
                data: Mutex::new(None),
            }
        }
    }

    impl ConfigProvider for InMemoryConfigProvider {
        fn load(&self) -> Result<Config, ConfigError> {
            let guard = self.data.lock().unwrap();
            match &*guard {
                Some(contents) => {
                    let cfg: Config = toml::from_str(contents).map_err(ConfigError::Parse)?;
                    Ok(cfg)
                }
                None => Ok(Config::default()),
            }
        }

        fn save(&self, config: &Config) -> Result<(), ConfigError> {
            let contents = toml::to_string_pretty(config).map_err(ConfigError::Serialize)?;
            let mut guard = self.data.lock().unwrap();
            *guard = Some(contents);
            drop(guard);
            Ok(())
        }
    }
}
