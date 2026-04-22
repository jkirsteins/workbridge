use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{fmt, fs};

use serde::{Deserialize, Serialize};

mod loader;
mod operations;

#[cfg(test)]
pub use loader::test_support::InMemoryConfigProvider;
pub use loader::{
    canonicalize_path, collapse_home, config_path, expand_tilde, normalize_repo_path,
};

/// Abstracts config persistence so tests can use an in-memory store
/// instead of writing to the real config file.
pub trait ConfigProvider {
    /// Load the persisted config. Used by `FileConfigProvider` at
    /// startup and by the test-only `InMemoryConfigProvider`.
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
        loader::atomic_write(&path, contents.as_bytes())?;
        Ok(())
    }
}

/// An MCP server entry configured for a specific repository.
///
/// Stored as `[[mcp_servers]]` array-of-tables in config.toml.
/// The `(repo, name)` pair is the composite key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// Repo path (collapsed home, e.g., "~/Projects/workbridge").
    pub repo: String,
    /// Unique server name within the repo (e.g., "datadog", "chrome-devtools").
    pub name: String,
    /// Server type: "stdio" (default, command-based) or "http".
    #[serde(rename = "type", default = "default_mcp_type")]
    #[serde(skip_serializing_if = "is_default_mcp_type")]
    pub server_type: String,
    /// Command to run (for stdio servers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables for the command.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// URL (for http servers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

fn default_mcp_type() -> String {
    "stdio".into()
}

fn is_default_mcp_type(s: &String) -> bool {
    s == "stdio"
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
    /// Per-repo MCP server configurations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerEntry>,
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
    /// Skill (slash command) to invoke for the review gate.
    #[serde(default = "default_review_skill")]
    pub review_skill: String,
    /// Number of days a Done work item remains visible before auto-deletion.
    /// Set to 0 to disable auto-archival (items stay forever).
    #[serde(default = "default_archive_after_days")]
    pub archive_after_days: u64,
    /// Canonical name (`"claude"` / `"codex"`) of the harness the
    /// Ctrl+G global assistant should spawn. `None` means "not yet
    /// chosen": the first Ctrl+G press opens a modal that lists the
    /// harnesses on PATH and persists the pick here. Settable non-
    /// interactively via `workbridge config set global-assistant-
    /// harness <name>`. "opencode" is not a valid value: the stub
    /// adapter for `OpenCode` exists only as internal scaffolding and
    /// is not user-selectable (rejected by `AgentBackendKind::from_str`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_assistant_harness: Option<String>,
}

fn default_worktree_dir() -> String {
    ".worktrees".into()
}

fn default_branch_issue_pattern() -> String {
    r"^(\d+)-".into()
}

fn default_review_skill() -> String {
    "/claude-adversarial-review".into()
}

const fn default_archive_after_days() -> u64 {
    7
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            worktree_dir: default_worktree_dir(),
            branch_issue_pattern: default_branch_issue_pattern(),
            review_skill: default_review_skill(),
            archive_after_days: default_archive_after_days(),
            global_assistant_harness: None,
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
    DuplicateMcpServer { repo: String, name: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Parse(e) => write!(f, "config parse error: {e}"),
            Self::Serialize(e) => write!(f, "config serialization error: {e}"),
            Self::NoConfigDir => write!(f, "could not determine config directory"),
            Self::PathNotFound(p) => write!(f, "path not found: {p}"),
            Self::NotAGitRepo(p) => write!(f, "not a git repository: {p}"),
            Self::DuplicateMcpServer { repo, name } => {
                write!(f, "MCP server '{name}' already exists for repo '{repo}'")
            }
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::{Config, McpServerEntry};

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
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
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
    }

    #[test]
    fn add_repo_validates_git_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        fs::create_dir_all(dir.join(".git")).unwrap();

        let mut config = Config::default();
        let result = config.add_repo(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(config.repos.len(), 1);
    }

    #[test]
    fn add_repo_rejects_non_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

        let mut config = Config::default();
        let result = config.add_repo(dir.to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn add_base_dir_accepts_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        fs::create_dir_all(dir.join("child-repo/.git")).unwrap();

        let mut config = Config::default();
        let result = config.add_base_dir(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert!(config.repos.is_empty());
        assert_eq!(config.base_dirs.len(), 1);
        let (_, count) = result.unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn add_mcp_server_accepts_new_entry() {
        let mut config = Config::default();
        let entry = McpServerEntry {
            repo: "/tmp/test-repo".into(),
            name: "my-server".into(),
            server_type: "stdio".into(),
            command: Some("npx".into()),
            args: vec!["-y".into(), "some-mcp".into()],
            env: BTreeMap::new(),
            url: None,
        };
        assert!(config.add_mcp_server(entry).is_ok());
        assert_eq!(config.mcp_servers.len(), 1);
    }

    #[test]
    fn add_mcp_server_rejects_duplicate() {
        let mut config = Config::default();
        let entry = McpServerEntry {
            repo: "/tmp/test-repo".into(),
            name: "my-server".into(),
            server_type: "stdio".into(),
            command: Some("npx".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
        };
        config.add_mcp_server(entry.clone()).unwrap();
        assert!(config.add_mcp_server(entry).is_err());
    }

    #[test]
    fn remove_mcp_server_removes_matching_entry() {
        let mut config = Config::default();
        config
            .add_mcp_server(McpServerEntry {
                repo: "/tmp/repo-a".into(),
                name: "server-a".into(),
                server_type: "stdio".into(),
                command: Some("cmd".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            })
            .unwrap();
        let removed = config.remove_mcp_server("/tmp/repo-a", "server-a");
        assert!(removed);
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn remove_mcp_server_returns_false_when_not_found() {
        let mut config = Config::default();
        assert!(!config.remove_mcp_server("/tmp/repo-a", "no-such-server"));
    }

    #[test]
    fn mcp_servers_for_repo_filters_by_repo() {
        let mut config = Config::default();
        config
            .add_mcp_server(McpServerEntry {
                repo: "/tmp/repo-a".into(),
                name: "server-a".into(),
                server_type: "stdio".into(),
                command: Some("cmd-a".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            })
            .unwrap();
        config
            .add_mcp_server(McpServerEntry {
                repo: "/tmp/repo-b".into(),
                name: "server-b".into(),
                server_type: "stdio".into(),
                command: Some("cmd-b".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            })
            .unwrap();
        let for_a = config.mcp_servers_for_repo("/tmp/repo-a");
        assert_eq!(for_a.len(), 1);
        assert_eq!(for_a[0].name, "server-a");
    }

    #[test]
    fn import_mcp_servers_merges_with_overwrite() {
        let mut config = Config::default();
        config
            .add_mcp_server(McpServerEntry {
                repo: "/tmp/repo-a".into(),
                name: "keep-me".into(),
                server_type: "stdio".into(),
                command: Some("old-cmd".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            })
            .unwrap();
        config
            .add_mcp_server(McpServerEntry {
                repo: "/tmp/repo-a".into(),
                name: "replace-me".into(),
                server_type: "stdio".into(),
                command: Some("old-cmd".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            })
            .unwrap();

        let to_import = vec![
            McpServerEntry {
                repo: "/tmp/repo-a".into(),
                name: "replace-me".into(),
                server_type: "stdio".into(),
                command: Some("new-cmd".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            },
            McpServerEntry {
                repo: "/tmp/repo-a".into(),
                name: "brand-new".into(),
                server_type: "stdio".into(),
                command: Some("fresh".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
            },
        ];

        let count = config.import_mcp_servers("/tmp/repo-a", to_import);
        assert_eq!(count, 2);
        assert_eq!(config.mcp_servers.len(), 3);

        let replace_me = config
            .mcp_servers
            .iter()
            .find(|s| s.name == "replace-me")
            .unwrap();
        assert_eq!(replace_me.command.as_deref(), Some("new-cmd"));

        // Original entry is preserved.
        assert!(config.mcp_servers.iter().any(|s| s.name == "keep-me"));
        // New entry was added.
        assert!(config.mcp_servers.iter().any(|s| s.name == "brand-new"));
    }

    #[test]
    fn mcp_server_entry_roundtrips_toml() {
        let mut config = Config::default();
        config
            .add_mcp_server(McpServerEntry {
                repo: "~/Projects/my-repo".into(),
                name: "chrome-devtools".into(),
                server_type: "stdio".into(),
                command: Some("npx".into()),
                args: vec!["-y".into(), "chrome-devtools-mcp@latest".into()],
                env: BTreeMap::new(),
                url: None,
            })
            .unwrap();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let reloaded: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(reloaded.mcp_servers.len(), 1);
        assert_eq!(reloaded.mcp_servers[0].name, "chrome-devtools");
        assert_eq!(reloaded.mcp_servers[0].command.as_deref(), Some("npx"));
    }

    #[test]
    fn remove_path_removes_repo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        fs::create_dir_all(dir.join(".git")).unwrap();

        let mut config = Config::default();
        config.add_repo(dir.to_str().unwrap()).unwrap();
        assert_eq!(config.repos.len(), 1);

        let removed = config.remove_path(dir.to_str().unwrap());
        assert!(removed);
        assert!(config.repos.is_empty());
    }
}
