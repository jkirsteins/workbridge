//! `impl Config` methods: load, add/remove repos and base dirs, and
//! MCP server CRUD operations. Kept separate so `config/mod.rs` stays
//! focused on the schema types.

use std::fs;
use std::path::PathBuf;

use super::{
    Config, ConfigError, McpServerEntry, RepoEntry, RepoSource, canonicalize_path, collapse_home,
    config_path, expand_tilde, loader, normalize_repo_path,
};

impl Config {
    /// Create a Config for tests with sensible defaults and "in-memory (test)"
    /// as the source. Avoids tests needing to specify every field.
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            source: "in-memory (test)".into(),
            ..Self::default()
        }
    }

    /// Load config from the default path. Returns a default (empty) config
    /// if the file does not exist. Returns an error if the file exists but
    /// cannot be parsed.
    pub fn load() -> Result<Self, ConfigError> {
        let path = config_path()?;
        let source = format!("{}", path.display());
        if !path.exists() {
            return Ok(Self {
                source: format!("{source} (not yet created)"),
                ..Self::default()
            });
        }
        let contents = fs::read_to_string(&path)?;
        let mut cfg: Self = toml::from_str(&contents).map_err(ConfigError::Parse)?;
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
            canonicalize_path(&expanded).map_err(|_| ConfigError::PathNotFound(raw.to_string()))?;

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
            canonicalize_path(&expanded).map_err(|_| ConfigError::PathNotFound(raw.to_string()))?;

        if !canonical.is_dir() {
            return Err(ConfigError::PathNotFound(raw.to_string()));
        }

        let display = collapse_home(&canonical);
        if !self.base_dirs.contains(&display) {
            self.base_dirs.push(display.clone());
        }
        let discovered = loader::discover_git_repos_in(&canonical);
        let count = discovered.len();
        Ok((display, count))
    }

    /// Remove a path from repos, `base_dirs`, and `included_repos`.
    pub fn remove_path(&mut self, raw: &str) -> bool {
        let target = expand_tilde(raw);
        let target_canonical = canonicalize_path(&target).ok();

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
            if let (Some(tc), Ok(sc)) = (&target_canonical, canonicalize_path(&stored_expanded)) {
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

    /// Discover git repos under all `base_dirs` (one level deep).
    pub fn discover_repos(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for base in &self.base_dirs {
            let expanded = expand_tilde(base);
            found.extend(loader::discover_git_repos_in(&expanded));
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

    /// Add an MCP server entry for a repo. Returns an error if an entry with
    /// the same (repo, name) pair already exists.
    pub fn add_mcp_server(&mut self, mut entry: McpServerEntry) -> Result<(), ConfigError> {
        entry.repo = normalize_repo_path(&entry.repo);
        let exists = self
            .mcp_servers
            .iter()
            .any(|s| s.repo == entry.repo && s.name == entry.name);
        if exists {
            return Err(ConfigError::DuplicateMcpServer {
                repo: entry.repo,
                name: entry.name,
            });
        }
        self.mcp_servers.push(entry);
        Ok(())
    }

    /// Remove an MCP server entry by repo + name. Returns true if removed.
    pub fn remove_mcp_server(&mut self, repo: &str, name: &str) -> bool {
        let normalized = normalize_repo_path(repo);
        let before = self.mcp_servers.len();
        self.mcp_servers
            .retain(|s| !(s.repo == normalized && s.name == name));
        self.mcp_servers.len() < before
    }

    /// Return all MCP server entries configured for a given repo path.
    pub fn mcp_servers_for_repo(&self, repo: &str) -> Vec<&McpServerEntry> {
        let normalized = normalize_repo_path(repo);
        self.mcp_servers
            .iter()
            .filter(|s| s.repo == normalized)
            .collect()
    }

    /// Import MCP server entries for a repo using merge-with-overwrite semantics.
    /// Existing entries with the same (repo, name) are replaced; new ones are added.
    /// Returns the number of entries imported.
    pub fn import_mcp_servers(&mut self, repo: &str, entries: Vec<McpServerEntry>) -> usize {
        let normalized = normalize_repo_path(repo);
        let count = entries.len();
        for mut entry in entries {
            entry.repo.clone_from(&normalized);
            if let Some(existing) = self
                .mcp_servers
                .iter_mut()
                .find(|s| s.repo == entry.repo && s.name == entry.name)
            {
                *existing = entry;
            } else {
                self.mcp_servers.push(entry);
            }
        }
        count
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
