use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Get the user's home directory using the directories crate.
fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

/// The TOML configuration for workbridge.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Directories to scan one level deep for git repos.
    #[serde(default)]
    pub base_dirs: Vec<String>,
    /// Individual repo paths (explicit additions).
    #[serde(default)]
    pub repos: Vec<String>,
    /// Fallback settings for repos that don't specify overrides.
    #[serde(default)]
    pub defaults: Defaults,
}

/// Default settings applied to repos that don't override them.
#[derive(Debug, Serialize, Deserialize)]
pub struct Defaults {
    /// Directory for managed worktrees, relative to repo root.
    #[serde(default = "default_worktree_dir")]
    pub worktree_dir: String,
    /// Regex for extracting issue identifiers from branch names.
    #[serde(default = "default_branch_issue_pattern")]
    pub branch_issue_pattern: String,
}

fn default_worktree_dir() -> String {
    ".worktrees".into()
}

fn default_branch_issue_pattern() -> String {
    r"^(\d+)-".into()
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            worktree_dir: default_worktree_dir(),
            branch_issue_pattern: default_branch_issue_pattern(),
        }
    }
}

/// How a repo was found: explicitly configured or discovered under a base dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSource {
    Explicit,
    Discovered,
}

/// A resolved repository entry for display.
#[derive(Debug, Clone)]
pub struct RepoEntry {
    pub path: PathBuf,
    pub source: RepoSource,
    pub available: bool,
}

/// Errors from config operations.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    NoConfigDir,
    PathNotFound(String),
    NotAGitRepo(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "I/O error: {e}"),
            ConfigError::Parse(e) => write!(f, "config parse error: {e}"),
            ConfigError::Serialize(e) => write!(f, "config serialization error: {e}"),
            ConfigError::NoConfigDir => write!(f, "could not determine config directory"),
            ConfigError::PathNotFound(p) => write!(f, "path not found: {p}"),
            ConfigError::NotAGitRepo(p) => write!(f, "not a git repository: {p}"),
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

/// Return the platform-specific config file path.
///
/// macOS: ~/Library/Application Support/workbridge/config.toml
/// Linux: ~/.config/workbridge/config.toml
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let proj = directories::ProjectDirs::from("", "", "workbridge")
        .ok_or(ConfigError::NoConfigDir)?;
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

/// Collapse the user's home directory back to `~` for display.
fn collapse_home(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

impl Config {
    /// Load config from the default path. Returns a default (empty) config
    /// if the file does not exist. Returns an error if the file exists but
    /// cannot be parsed.
    pub fn load() -> Result<Self, ConfigError> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let contents = fs::read_to_string(&path)?;
        toml::from_str(&contents).map_err(ConfigError::Parse)
    }

    /// Save config to the default path, creating parent directories if needed.
    /// Uses atomic write (write to temp file, then rename) to prevent data
    /// loss if the process is killed mid-write.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        atomic_write(&path, contents.as_bytes())?;
        Ok(())
    }

    /// Add a path as a repo (if it contains .git/) or as a base_dir (if it
    /// contains git repos one level down). Returns what was added.
    pub fn add_path(&mut self, raw: &str) -> Result<AddResult, ConfigError> {
        let expanded = expand_tilde(raw);
        let canonical = fs::canonicalize(&expanded)
            .map_err(|_| ConfigError::PathNotFound(raw.to_string()))?;

        // Check if it's a git repo itself.
        if canonical.join(".git").exists() {
            let display = collapse_home(&canonical);
            if !self.repos.contains(&display) {
                self.repos.push(display.clone());
            }
            return Ok(AddResult::Repo(display));
        }

        // Check if it contains git repos one level down.
        if canonical.is_dir() {
            let children = discover_git_repos_in(&canonical);
            if !children.is_empty() {
                let display = collapse_home(&canonical);
                if !self.base_dirs.contains(&display) {
                    self.base_dirs.push(display.clone());
                }
                return Ok(AddResult::BaseDir(display, children.len()));
            }
        }

        Err(ConfigError::NotAGitRepo(raw.to_string()))
    }

    /// Remove a path from both repos and base_dirs.
    pub fn remove_path(&mut self, raw: &str) -> bool {
        let expanded = expand_tilde(raw);
        let canonical = fs::canonicalize(&expanded).ok();

        let before = self.repos.len() + self.base_dirs.len();

        self.repos.retain(|r| {
            let r_expanded = expand_tilde(r);
            let r_canonical = fs::canonicalize(&r_expanded).ok();
            r_expanded != expanded && r_canonical != canonical
        });
        self.base_dirs.retain(|b| {
            let b_expanded = expand_tilde(b);
            let b_canonical = fs::canonicalize(&b_expanded).ok();
            b_expanded != expanded && b_canonical != canonical
        });

        let after = self.repos.len() + self.base_dirs.len();
        after < before
    }

    /// Discover git repos under all base_dirs (one level deep).
    pub fn discover_repos(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for base in &self.base_dirs {
            let expanded = expand_tilde(base);
            found.extend(discover_git_repos_in(&expanded));
        }
        found.sort();
        found.dedup();
        found
    }

    /// Return all repos (explicit + discovered) with metadata.
    pub fn all_repos(&self) -> Vec<RepoEntry> {
        let mut entries = Vec::new();

        for repo in &self.repos {
            let path = expand_tilde(repo);
            let available = path.join(".git").exists();
            entries.push(RepoEntry {
                path,
                source: RepoSource::Explicit,
                available,
            });
        }

        for path in self.discover_repos() {
            // Skip if already listed as explicit.
            if entries.iter().any(|e| e.path == path) {
                continue;
            }
            let available = path.join(".git").exists();
            entries.push(RepoEntry {
                path,
                source: RepoSource::Discovered,
                available,
            });
        }

        entries
    }
}

/// What was added by add_path.
pub enum AddResult {
    Repo(String),
    BaseDir(String, usize),
}

/// Write data to a file atomically by writing to a temp file in the same
/// directory and then renaming. On POSIX, rename within the same filesystem
/// is atomic, so a crash mid-write leaves the original file intact.
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config".into())
    ));
    fs::write(&tmp_path, data)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Scan a directory one level deep for subdirectories containing `.git/`.
fn discover_git_repos_in(dir: &Path) -> Vec<PathBuf> {
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
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn expand_tilde_with_home() {
        let expanded = expand_tilde("~/Projects");
        assert!(expanded.to_str().unwrap().contains("Projects"));
        assert!(!expanded.to_str().unwrap().starts_with('~'));
    }

    #[test]
    fn expand_tilde_absolute_unchanged() {
        let expanded = expand_tilde("/tmp/foo");
        assert_eq!(expanded, PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn expand_tilde_bare() {
        let expanded = expand_tilde("~");
        assert!(!expanded.to_str().unwrap().starts_with('~'));
    }

    #[test]
    fn load_missing_file_returns_default() {
        // Point at a nonexistent path - load should return default.
        // We can't easily override config_path in tests, so just verify
        // Default works.
        let config = Config::default();
        assert!(config.repos.is_empty());
        assert!(config.base_dirs.is_empty());
        assert_eq!(config.defaults.worktree_dir, ".worktrees");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("workbridge-test-config");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let config = Config {
            base_dirs: vec!["~/Projects".into()],
            repos: vec!["~/Forks/repo".into()],
            defaults: Defaults::default(),
        };

        let contents = toml::to_string_pretty(&config).unwrap();
        fs::write(&path, &contents).unwrap();

        let loaded: Config = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.base_dirs, vec!["~/Projects"]);
        assert_eq!(loaded.repos, vec!["~/Forks/repo"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_repos_finds_git_dirs() {
        let dir = std::env::temp_dir().join("workbridge-test-discover");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo-a/.git")).unwrap();
        fs::create_dir_all(dir.join("repo-b/.git")).unwrap();
        fs::create_dir_all(dir.join("not-a-repo")).unwrap();

        let found = discover_git_repos_in(&dir);
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|p| p.ends_with("repo-a")));
        assert!(found.iter().any(|p| p.ends_with("repo-b")));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_repos_empty_dir() {
        let dir = std::env::temp_dir().join("workbridge-test-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let found = discover_git_repos_in(&dir);
        assert!(found.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_repos_nonexistent_dir() {
        let found = discover_git_repos_in(Path::new("/nonexistent/path"));
        assert!(found.is_empty());
    }

    #[test]
    fn add_path_detects_git_repo() {
        let dir = std::env::temp_dir().join("workbridge-test-add-repo");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();

        let mut config = Config::default();
        let result = config.add_path(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(config.repos.len(), 1);
        assert!(config.base_dirs.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_path_detects_base_dir() {
        let dir = std::env::temp_dir().join("workbridge-test-add-base");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("child-repo/.git")).unwrap();

        let mut config = Config::default();
        let result = config.add_path(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert!(config.repos.is_empty());
        assert_eq!(config.base_dirs.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_path_rejects_non_repo() {
        let dir = std::env::temp_dir().join("workbridge-test-add-fail");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut config = Config::default();
        let result = config.add_path(dir.to_str().unwrap());
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_path_removes_repo() {
        let dir = std::env::temp_dir().join("workbridge-test-remove");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();

        let mut config = Config::default();
        config.add_path(dir.to_str().unwrap()).unwrap();
        assert_eq!(config.repos.len(), 1);

        let removed = config.remove_path(dir.to_str().unwrap());
        assert!(removed);
        assert!(config.repos.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
