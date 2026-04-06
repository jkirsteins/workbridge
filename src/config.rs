use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Abstracts config persistence so tests can use an in-memory store
/// instead of writing to the real config file.
pub trait ConfigProvider {
    /// Load the persisted config. Used by FileConfigProvider at startup
    /// and by InMemoryConfigProvider in tests.
    #[allow(dead_code)]
    fn load(&self) -> Result<Config, ConfigError>;
    fn save(&self, config: &Config) -> Result<(), ConfigError>;
}

/// Production config provider that reads/writes the platform config file.
pub struct FileConfigProvider;

impl ConfigProvider for FileConfigProvider {
    fn load(&self) -> Result<Config, ConfigError> {
        Config::load()
    }

    fn save(&self, config: &Config) -> Result<(), ConfigError> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(config).map_err(ConfigError::Serialize)?;
        atomic_write(&path, contents.as_bytes())?;
        Ok(())
    }
}

/// In-memory config provider for tests. Never touches disk.
/// Constructed only in `#[cfg(test)]` code.
#[allow(dead_code)]
pub struct InMemoryConfigProvider {
    data: Mutex<Option<String>>,
}

impl InMemoryConfigProvider {
    #[allow(dead_code)]
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
        Ok(())
    }
}

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
    /// Repo paths opted-in from discovery. A discovered repo is only active
    /// if it appears here. Explicit repos (in `repos`) are always active.
    #[serde(default)]
    pub included_repos: Vec<String>,
    /// Fallback settings for repos that don't specify overrides.
    #[serde(default)]
    pub defaults: Defaults,
    /// Human-readable description of where this config came from.
    /// Set by the loader - not serialized to the TOML file.
    #[serde(skip)]
    pub source: String,
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
    /// Maximum number of work items that can have active Claude sessions in
    /// the Implementing stage at once. Additional items are queued and
    /// auto-started when a slot becomes available. Default: 1.
    #[serde(default = "default_max_implementing")]
    pub max_implementing: u32,
}

fn default_worktree_dir() -> String {
    ".worktrees".into()
}

fn default_branch_issue_pattern() -> String {
    r"^(\d+)-".into()
}

fn default_max_implementing() -> u32 {
    1
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            worktree_dir: default_worktree_dir(),
            branch_issue_pattern: default_branch_issue_pattern(),
            max_implementing: default_max_implementing(),
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
    /// Whether the .git directory exists on disk right now.
    pub git_dir_present: bool,
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
    let proj =
        directories::ProjectDirs::from("", "", "workbridge").ok_or(ConfigError::NoConfigDir)?;
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

/// Normalize a repo path to a consistent form: expand tilde, canonicalize
/// if possible, then collapse back to ~/... for storage. This ensures that
/// `./repo`, `~/repo`, and `/abs/path/repo` all produce the same string.
fn normalize_repo_path(path: &str) -> String {
    let expanded = expand_tilde(path);
    let canonical = fs::canonicalize(&expanded).unwrap_or(expanded);
    collapse_home(&canonical)
}

impl Config {
    /// Create a Config for tests with sensible defaults and "in-memory (test)"
    /// as the source. Avoids tests needing to specify every field.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Config {
            source: "in-memory (test)".into(),
            ..Config::default()
        }
    }

    /// Load config from the default path. Returns a default (empty) config
    /// if the file does not exist. Returns an error if the file exists but
    /// cannot be parsed.
    pub fn load() -> Result<Self, ConfigError> {
        let path = config_path()?;
        let source = format!("{}", path.display());
        if !path.exists() {
            return Ok(Config {
                source: format!("{source} (not yet created)"),
                ..Config::default()
            });
        }
        let contents = fs::read_to_string(&path)?;
        let mut cfg: Config = toml::from_str(&contents).map_err(ConfigError::Parse)?;
        cfg.source = source;
        // Normalize included_repos so hand-edited paths (relative, non-canonical)
        // match correctly in active_repos() filtering.
        cfg.included_repos = cfg
            .included_repos
            .into_iter()
            .map(|p| normalize_repo_path(&p))
            .collect();
        Ok(cfg)
    }

    /// Add an individual repo path. Validates that it contains `.git/`.
    /// Explicit repos are always active (no need to also include them).
    pub fn add_repo(&mut self, raw: &str) -> Result<String, ConfigError> {
        let expanded = expand_tilde(raw);
        let canonical =
            fs::canonicalize(&expanded).map_err(|_| ConfigError::PathNotFound(raw.to_string()))?;

        if !canonical.join(".git").exists() {
            return Err(ConfigError::NotAGitRepo(raw.to_string()));
        }

        let display = collapse_home(&canonical);
        if !self.repos.contains(&display) {
            self.repos.push(display.clone());
        }
        Ok(display)
    }

    /// Add a base directory for auto-discovery. Validates that it exists
    /// and is a directory. Discovered repos start unmanaged by default -
    /// the user opts in via `include_repo`.
    pub fn add_base_dir(&mut self, raw: &str) -> Result<(String, usize), ConfigError> {
        let expanded = expand_tilde(raw);
        let canonical =
            fs::canonicalize(&expanded).map_err(|_| ConfigError::PathNotFound(raw.to_string()))?;

        if !canonical.is_dir() {
            return Err(ConfigError::PathNotFound(raw.to_string()));
        }

        let display = collapse_home(&canonical);
        if !self.base_dirs.contains(&display) {
            self.base_dirs.push(display.clone());
        }
        let discovered = discover_git_repos_in(&canonical);
        let count = discovered.len();
        Ok((display, count))
    }

    /// Remove a path from repos, base_dirs, and included_repos.
    pub fn remove_path(&mut self, raw: &str) -> bool {
        let target = expand_tilde(raw);
        let target_canonical = fs::canonicalize(&target).ok();

        let before = self.repos.len() + self.base_dirs.len() + self.included_repos.len();

        let matches_target = |stored: &str| -> bool {
            let stored_expanded = expand_tilde(stored);
            // Always compare expanded paths (string equality).
            if stored_expanded == target {
                return true;
            }
            // Only compare canonical paths when BOTH sides succeed.
            // If either fails to canonicalize (e.g., unmounted drive),
            // we cannot conclude they match - this prevents removing
            // unrelated entries that also happen to be inaccessible.
            if let (Some(tc), Ok(sc)) = (&target_canonical, fs::canonicalize(&stored_expanded)) {
                return sc == *tc;
            }
            false
        };

        self.repos.retain(|r| !matches_target(r));
        self.base_dirs.retain(|b| !matches_target(b));
        self.included_repos.retain(|i| !matches_target(i));

        let after = self.repos.len() + self.base_dirs.len() + self.included_repos.len();
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

    /// Add a repo path to the inclusion list (opt-in from discovery).
    /// Normalizes the path so different representations of the same path
    /// (relative, absolute, ~/) all produce the same stored entry.
    pub fn include_repo(&mut self, path: &str) {
        let normalized = normalize_repo_path(path);
        if !self.included_repos.contains(&normalized) {
            self.included_repos.push(normalized);
        }
    }

    /// Remove a repo path from the inclusion list. Matches by normalized
    /// path so `./repo`, `~/repo`, and `/abs/repo` all resolve the same entry.
    pub fn uninclude_repo(&mut self, path: &str) {
        let target = normalize_repo_path(path);
        self.included_repos
            .retain(|p| normalize_repo_path(p) != target);
    }

    /// Return all repos (explicit + discovered) with metadata.
    pub fn all_repos(&self) -> Vec<RepoEntry> {
        let mut entries = Vec::new();

        for repo in &self.repos {
            let path = expand_tilde(repo);
            let git_dir_present = path.join(".git").exists();
            entries.push(RepoEntry {
                path,
                source: RepoSource::Explicit,
                git_dir_present,
            });
        }

        for path in self.discover_repos() {
            // Skip if already listed as explicit.
            if entries.iter().any(|e| e.path == path) {
                continue;
            }
            let git_dir_present = path.join(".git").exists();
            entries.push(RepoEntry {
                path,
                source: RepoSource::Discovered,
                git_dir_present,
            });
        }

        entries
    }

    /// Return active repos: explicit repos (always active) plus discovered
    /// repos that appear in `included_repos`. This is the authoritative
    /// "what repos are active" query for both CLI and TUI.
    /// Both sides are normalized before comparison so hand-edited,
    /// relative, or non-canonical config paths still match correctly.
    pub fn active_repos(&self) -> Vec<RepoEntry> {
        // included_repos are normalized both on insert (include_repo) and
        // on load (Config::load), so we compare directly.
        self.all_repos()
            .into_iter()
            .filter(|entry| {
                // Explicit repos are always active.
                if entry.source == RepoSource::Explicit {
                    return true;
                }
                // Discovered repos are active only if opted-in.
                let entry_normalized = normalize_repo_path(&entry.path.display().to_string());
                self.included_repos.contains(&entry_normalized)
            })
            .collect()
    }
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
            ..Config::for_test()
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
    fn add_repo_validates_git_dir() {
        let dir = std::env::temp_dir().join("workbridge-test-add-repo");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();

        let mut config = Config::default();
        let result = config.add_repo(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(config.repos.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_repo_rejects_non_repo() {
        let dir = std::env::temp_dir().join("workbridge-test-add-fail");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut config = Config::default();
        let result = config.add_repo(dir.to_str().unwrap());
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_base_dir_accepts_directory() {
        let dir = std::env::temp_dir().join("workbridge-test-add-base");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("child-repo/.git")).unwrap();

        let mut config = Config::default();
        let result = config.add_base_dir(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert!(config.repos.is_empty());
        assert_eq!(config.base_dirs.len(), 1);
        let (_, count) = result.unwrap();
        assert_eq!(count, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_path_removes_repo() {
        let dir = std::env::temp_dir().join("workbridge-test-remove");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();

        let mut config = Config::default();
        config.add_repo(dir.to_str().unwrap()).unwrap();
        assert_eq!(config.repos.len(), 1);

        let removed = config.remove_path(dir.to_str().unwrap());
        assert!(removed);
        assert!(config.repos.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
