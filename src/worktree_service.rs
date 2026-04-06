use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::github_client::parse_github_remote;

/// Errors from worktree operations.
#[derive(Clone, Debug)]
pub enum WorktreeError {
    /// git command failed with an error message.
    GitError(String),
    /// I/O error during worktree operations.
    Io(String),
    /// The repo path does not exist or is not a git repo.
    InvalidRepo(PathBuf),
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorktreeError::GitError(msg) => write!(f, "git error: {msg}"),
            WorktreeError::Io(msg) => write!(f, "worktree I/O error: {msg}"),
            WorktreeError::InvalidRepo(path) => {
                write!(f, "invalid git repo: {}", path.display())
            }
        }
    }
}

/// Information about a single worktree.
#[derive(Clone, Debug, PartialEq)]
pub struct WorktreeInfo {
    /// Filesystem path to the worktree.
    pub path: PathBuf,
    /// Branch the worktree is on, or None if detached HEAD.
    pub branch: Option<String>,
    /// True if this is the main worktree (the repo's primary checkout).
    pub is_main: bool,
}

/// Trait for worktree operations. Implementations include
/// GitWorktreeService (shells out to git CLI) and test mocks.
pub trait WorktreeService: Send + Sync {
    /// List all worktrees for a repo.
    fn list_worktrees(&self, repo_path: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError>;

    /// Create a new worktree for a branch at the given target directory.
    /// Called when opening a session for a work item that has a branch but
    /// no worktree, and when importing an unlinked PR.
    fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
        target_dir: &Path,
    ) -> Result<WorktreeInfo, WorktreeError>;

    /// Remove a worktree. Optionally delete the branch as well.
    /// Used by integration tests; called from UI when "delete worktree"
    /// flow is added.
    #[allow(dead_code)]
    fn remove_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        delete_branch: bool,
    ) -> Result<(), WorktreeError>;

    /// Get the default branch name (main, master, or configured) for a repo.
    fn default_branch(&self, repo_path: &Path) -> Result<String, WorktreeError>;

    /// Get the GitHub remote (owner, repo) for a repo, if any.
    fn github_remote(&self, repo_path: &Path) -> Result<Option<(String, String)>, WorktreeError>;

    /// Fetch a branch from origin so the local ref points at the correct
    /// commit. Returns Ok(()) if the fetch succeeds, Err if it fails
    /// (branch does not exist on origin, fork PR, network error, etc.).
    fn fetch_branch(&self, repo_path: &Path, branch: &str) -> Result<(), WorktreeError>;

    /// Create a new local branch from the repo's default branch (or HEAD).
    /// Used as a fallback when fetch_branch fails (e.g., the branch does
    /// not exist on origin yet).
    fn create_branch(&self, repo_path: &Path, branch: &str) -> Result<(), WorktreeError>;
}

/// GitWorktreeService shells out to the git CLI for worktree operations.
pub struct GitWorktreeService;

impl GitWorktreeService {
    /// Run a git command with `-C repo_path` and return stdout on success.
    fn run_git(repo_path: &Path, args: &[&str]) -> Result<String, WorktreeError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(args)
            .output()
            .map_err(|e| WorktreeError::Io(format!("failed to run git: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            // Detect invalid repo from common git error messages.
            if stderr.contains("not a git repository") {
                return Err(WorktreeError::InvalidRepo(repo_path.to_path_buf()));
            }
            return Err(WorktreeError::GitError(stderr));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| WorktreeError::GitError(format!("invalid UTF-8 in git output: {e}")))
    }

    /// Parse porcelain output from `git worktree list --porcelain` into
    /// WorktreeInfo entries.
    ///
    /// The porcelain format produces blocks separated by blank lines. Each
    /// block contains lines like:
    ///   worktree /path/to/worktree
    ///   HEAD <sha>
    ///   branch refs/heads/<name>
    /// or:
    ///   worktree /path/to/worktree
    ///   HEAD <sha>
    ///   detached
    ///
    /// The first block is always the main worktree.
    fn parse_porcelain(output: &str) -> Vec<WorktreeInfo> {
        let mut result = Vec::new();
        let mut current_path: Option<PathBuf> = None;
        let mut current_branch: Option<String> = None;
        let mut is_first = true;

        for line in output.lines() {
            if line.is_empty() {
                // End of a block - emit the entry if we have a path.
                if let Some(path) = current_path.take() {
                    result.push(WorktreeInfo {
                        path,
                        branch: current_branch.take(),
                        is_main: is_first,
                    });
                    is_first = false;
                }
                continue;
            }

            if let Some(path_str) = line.strip_prefix("worktree ") {
                current_path = Some(PathBuf::from(path_str));
            } else if let Some(branch_ref) = line.strip_prefix("branch ") {
                // Strip refs/heads/ prefix to get the short branch name.
                current_branch = Some(
                    branch_ref
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch_ref)
                        .to_string(),
                );
            } else if line == "detached" {
                current_branch = None;
            }
        }

        // Handle the last block if there was no trailing blank line.
        if let Some(path) = current_path.take() {
            result.push(WorktreeInfo {
                path,
                branch: current_branch.take(),
                is_main: is_first,
            });
        }

        result
    }

    /// Find the branch name for a worktree at the given path by looking
    /// through the list of all worktrees.
    /// Called from remove_worktree; used in integration tests.
    #[allow(dead_code)]
    fn find_branch_for_worktree(
        repo_path: &Path,
        worktree_path: &Path,
    ) -> Result<Option<String>, WorktreeError> {
        let output = Self::run_git(repo_path, &["worktree", "list", "--porcelain"])?;
        let worktrees = Self::parse_porcelain(&output);
        // Canonicalize the target path to handle symlinks (e.g. /tmp ->
        // /private/tmp on macOS) so it matches the paths git reports.
        let canonical_target = worktree_path
            .canonicalize()
            .unwrap_or_else(|_| worktree_path.to_path_buf());
        Ok(worktrees
            .into_iter()
            .find(|w| {
                let canonical_w = w.path.canonicalize().unwrap_or_else(|_| w.path.clone());
                canonical_w == canonical_target
            })
            .and_then(|w| w.branch))
    }
}

impl WorktreeService for GitWorktreeService {
    fn list_worktrees(&self, repo_path: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let output = Self::run_git(repo_path, &["worktree", "list", "--porcelain"])?;
        let mut worktrees = Self::parse_porcelain(&output);

        // Filter out the main worktree if it is on the default branch,
        // since it is not a work item worktree.
        let default = self.default_branch(repo_path)?;
        worktrees.retain(|w| {
            if w.is_main {
                // Keep the main worktree only if it is NOT on the default branch.
                w.branch.as_deref() != Some(default.as_str())
            } else {
                true
            }
        });

        Ok(worktrees)
    }

    fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
        target_dir: &Path,
    ) -> Result<WorktreeInfo, WorktreeError> {
        let target_str = target_dir.to_str().ok_or_else(|| {
            WorktreeError::Io(format!(
                "target directory path is not valid UTF-8: {}",
                target_dir.display()
            ))
        })?;

        // Check if the branch already exists.
        let branch_exists = Self::run_git(
            repo_path,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_ok();

        let result = if branch_exists {
            Self::run_git(repo_path, &["worktree", "add", target_str, branch])
        } else {
            Self::run_git(repo_path, &["worktree", "add", target_str, "-b", branch])
        };

        result?;

        Ok(WorktreeInfo {
            path: target_dir.to_path_buf(),
            branch: Some(branch.to_string()),
            is_main: false,
        })
    }

    fn remove_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        delete_branch: bool,
    ) -> Result<(), WorktreeError> {
        // Look up the branch name before removing the worktree, since we
        // need it for the optional branch deletion.
        let branch = if delete_branch {
            Self::find_branch_for_worktree(repo_path, worktree_path)?
        } else {
            None
        };

        let wt_str = worktree_path.to_str().ok_or_else(|| {
            WorktreeError::Io(format!(
                "worktree path is not valid UTF-8: {}",
                worktree_path.display()
            ))
        })?;
        Self::run_git(repo_path, &["worktree", "remove", wt_str])?;

        if let Some(branch_name) = branch {
            Self::run_git(repo_path, &["branch", "-d", &branch_name])?;
        }

        Ok(())
    }

    fn default_branch(&self, repo_path: &Path) -> Result<String, WorktreeError> {
        match Self::run_git(repo_path, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
            Ok(output) => {
                let trimmed = output.trim();
                // Output is like "refs/remotes/origin/main" - strip the prefix.
                let branch = trimmed
                    .strip_prefix("refs/remotes/origin/")
                    .unwrap_or(trimmed);
                Ok(branch.to_string())
            }
            Err(_) => {
                // No origin/HEAD available. Check which of "main" or "master"
                // exists as a local branch. If both exist, prefer "main". If
                // neither exists, fall back to "main" as a convention default.
                let main_exists =
                    Self::run_git(repo_path, &["rev-parse", "--verify", "refs/heads/main"]).is_ok();
                if main_exists {
                    return Ok("main".to_string());
                }
                let master_exists =
                    Self::run_git(repo_path, &["rev-parse", "--verify", "refs/heads/master"])
                        .is_ok();
                if master_exists {
                    return Ok("master".to_string());
                }
                // Neither exists - use "main" as convention default.
                Ok("main".to_string())
            }
        }
    }

    fn github_remote(&self, repo_path: &Path) -> Result<Option<(String, String)>, WorktreeError> {
        match Self::run_git(repo_path, &["remote", "get-url", "origin"]) {
            Ok(url) => Ok(parse_github_remote(url.trim())),
            Err(WorktreeError::GitError(ref msg))
                if msg.to_lowercase().contains("no such remote") =>
            {
                // No origin remote configured - not an error, just no GitHub remote.
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn fetch_branch(&self, repo_path: &Path, branch: &str) -> Result<(), WorktreeError> {
        // Fetch the branch from origin into a local branch of the same name.
        // Uses the refspec <branch>:<branch> so that on success the local
        // ref points at the same commit as origin.
        let refspec = format!("{branch}:{branch}");
        Self::run_git(repo_path, &["fetch", "origin", &refspec])?;
        Ok(())
    }

    fn create_branch(&self, repo_path: &Path, branch: &str) -> Result<(), WorktreeError> {
        // Check if the branch already exists.
        if Self::run_git(
            repo_path,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .is_ok()
        {
            return Ok(()); // Branch already exists locally.
        }

        // Base the new branch on the default branch (main/master) so that
        // feature branches start from the canonical base, not whatever
        // happens to be checked out.
        let base = self
            .default_branch(repo_path)
            .unwrap_or_else(|_| "HEAD".to_string());
        Self::run_git(repo_path, &["branch", branch, &base])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_porcelain tests (pure unit tests, no git CLI)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_porcelain_single_main_worktree() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main\n\
                       \n";
        let result = GitWorktreeService::parse_porcelain(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/repo"));
        assert_eq!(result[0].branch, Some("main".to_string()));
        assert!(result[0].is_main);
    }

    #[test]
    fn parse_porcelain_multiple_worktrees() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main\n\
                       \n\
                       worktree /home/user/repo-wt\n\
                       HEAD def5678\n\
                       branch refs/heads/feature-x\n\
                       \n";
        let result = GitWorktreeService::parse_porcelain(output);
        assert_eq!(result.len(), 2);
        assert!(result[0].is_main);
        assert_eq!(result[0].branch, Some("main".to_string()));
        assert!(!result[1].is_main);
        assert_eq!(result[1].branch, Some("feature-x".to_string()));
    }

    #[test]
    fn parse_porcelain_detached_head() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main\n\
                       \n\
                       worktree /home/user/repo-detached\n\
                       HEAD 9999999\n\
                       detached\n\
                       \n";
        let result = GitWorktreeService::parse_porcelain(output);
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].branch, None);
        assert!(!result[1].is_main);
    }

    #[test]
    fn parse_porcelain_no_trailing_newline() {
        let output = "worktree /home/user/repo\n\
                       HEAD abc1234\n\
                       branch refs/heads/main";
        let result = GitWorktreeService::parse_porcelain(output);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/repo"));
        assert!(result[0].is_main);
    }
}

/// Integration tests that shell out to real git. Gated behind the
/// `integration` feature so they don't run on every `cargo test`.
/// Run with: `cargo test --features integration`
///
/// These tests use environment variables (GIT_AUTHOR_EMAIL, etc.)
/// instead of `git config` to avoid writing to any git config file.
/// This prevents worktree config writes from poisoning the parent
/// repo's .git/config (the root cause of the core.bare corruption).
#[cfg(test)]
#[cfg(feature = "integration")]
mod integration_tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    /// Create a temporary git repo with an initial commit.
    /// Uses env vars for author identity - NEVER calls `git config`.
    fn setup_git_repo(dir: &Path) {
        run_in(dir, &["git", "init"]);
        let file_path = dir.join("README");
        fs::write(&file_path, "init").unwrap();
        run_in(dir, &["git", "add", "README"]);
        // Use -c flags for author identity instead of git config.
        let output = Command::new("git")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "initial commit",
            ])
            .output()
            .unwrap();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("git commit failed in {}:\n{stderr}", dir.display());
        }
    }

    fn run_in(dir: &Path, args: &[&str]) {
        let output = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to run {:?}: {e}", args));
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("command {:?} failed in {}:\n{stderr}", args, dir.display());
        }
    }

    /// Helper for git commits that need author identity without git config.
    fn commit_in(dir: &Path, message: &str) {
        let output = Command::new("git")
            .args([
                "-C",
                dir.to_str().unwrap(),
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                message,
            ])
            .output()
            .unwrap();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("git commit failed in {}:\n{stderr}", dir.display());
        }
    }

    #[test]
    fn list_worktrees_returns_non_default_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        // Create a secondary worktree on a feature branch.
        let wt_dir = tmp.path().join("wt-feature");
        run_in(
            &repo_dir,
            &[
                "git",
                "worktree",
                "add",
                wt_dir.to_str().unwrap(),
                "-b",
                "feature-branch",
            ],
        );

        let svc = GitWorktreeService;
        let worktrees = svc.list_worktrees(&repo_dir).unwrap();

        // The main worktree is on "master" (git init default). With the
        // improved fallback, default_branch detects the local "master"
        // branch, so the main worktree IS filtered out. Only the feature
        // worktree should remain.
        let branches: Vec<Option<&str>> = worktrees.iter().map(|w| w.branch.as_deref()).collect();
        assert!(
            branches.contains(&Some("feature-branch")),
            "feature-branch should be listed, got: {:?}",
            branches,
        );
        assert!(
            !branches.contains(&Some("master")),
            "main worktree on 'master' should be filtered out, got: {:?}",
            branches,
        );
    }

    #[test]
    fn list_worktrees_filters_main_on_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        // Rename the branch to "main" so it matches the fallback default.
        run_in(&repo_dir, &["git", "branch", "-m", "main"]);

        let svc = GitWorktreeService;
        let worktrees = svc.list_worktrees(&repo_dir).unwrap();

        // The main worktree is on "main" which matches the default, so it
        // should be filtered out. No other worktrees exist.
        assert!(
            worktrees.is_empty(),
            "main worktree on default branch should be filtered, got: {:?}",
            worktrees,
        );
    }

    #[test]
    fn create_and_remove_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        let svc = GitWorktreeService;
        let wt_dir = tmp.path().join("new-worktree");

        // Create a worktree with a new branch.
        let info = svc
            .create_worktree(&repo_dir, "test-branch", &wt_dir)
            .unwrap();
        assert_eq!(info.path, wt_dir);
        assert_eq!(info.branch, Some("test-branch".to_string()));
        assert!(!info.is_main);
        assert!(wt_dir.exists(), "worktree directory should exist on disk");

        // Remove the worktree (with branch deletion).
        svc.remove_worktree(&repo_dir, &wt_dir, true).unwrap();
        assert!(
            !wt_dir.exists(),
            "worktree directory should be removed from disk"
        );

        // Verify the branch was deleted.
        let branch_check = Command::new("git")
            .arg("-C")
            .arg(&repo_dir)
            .args(["rev-parse", "--verify", "refs/heads/test-branch"])
            .output()
            .unwrap();
        assert!(
            !branch_check.status.success(),
            "branch should have been deleted"
        );
    }

    #[test]
    fn create_worktree_with_existing_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        // Create a branch first without a worktree.
        run_in(&repo_dir, &["git", "branch", "existing-branch"]);

        let svc = GitWorktreeService;
        let wt_dir = tmp.path().join("existing-wt");

        let info = svc
            .create_worktree(&repo_dir, "existing-branch", &wt_dir)
            .unwrap();
        assert_eq!(info.branch, Some("existing-branch".to_string()));
        assert!(wt_dir.exists());
    }

    #[test]
    fn default_branch_falls_back_to_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);
        // Rename to "main" so the local branch check finds it.
        run_in(&repo_dir, &["git", "branch", "-m", "main"]);

        let svc = GitWorktreeService;
        // No remote configured, so symbolic-ref will fail. Should find
        // local "main" branch and fall back to it.
        let branch = svc.default_branch(&repo_dir).unwrap();
        assert_eq!(branch, "main");
    }

    #[test]
    fn default_branch_falls_back_to_master() {
        // F-2 regression: repos whose trunk is "master" should get "master"
        // as the default branch, not "main".
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);
        // Explicitly rename to "master" to test the fallback, since modern
        // git may create "main" by default depending on configuration.
        run_in(&repo_dir, &["git", "branch", "-m", "master"]);

        let svc = GitWorktreeService;
        let branch = svc.default_branch(&repo_dir).unwrap();
        assert_eq!(
            branch, "master",
            "should detect 'master' when no origin/HEAD and 'main' branch does not exist"
        );
    }

    #[test]
    fn default_branch_reads_from_remote_head() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a bare "remote" repo.
        let remote_dir = tmp.path().join("remote.git");
        fs::create_dir_all(&remote_dir).unwrap();
        run_in(&remote_dir, &["git", "init", "--bare"]);

        // Create a local repo and push to the remote.
        let repo_dir = tmp.path().join("local");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);
        run_in(
            &repo_dir,
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote_dir.to_str().unwrap(),
            ],
        );
        // Rename the default branch to "develop" to test non-standard names.
        run_in(&repo_dir, &["git", "branch", "-m", "develop"]);
        run_in(&repo_dir, &["git", "push", "-u", "origin", "develop"]);
        // Set the remote HEAD.
        run_in(
            &repo_dir,
            &[
                "git",
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/develop",
            ],
        );

        let svc = GitWorktreeService;
        let branch = svc.default_branch(&repo_dir).unwrap();
        assert_eq!(branch, "develop");
    }

    #[test]
    fn github_remote_parses_url() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        // Add a GitHub-style remote.
        run_in(
            &repo_dir,
            &[
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:myorg/myrepo.git",
            ],
        );

        let svc = GitWorktreeService;
        let result = svc.github_remote(&repo_dir).unwrap();
        assert_eq!(result, Some(("myorg".to_string(), "myrepo".to_string())));
    }

    #[test]
    fn github_remote_returns_none_for_non_github() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        run_in(
            &repo_dir,
            &[
                "git",
                "remote",
                "add",
                "origin",
                "git@gitlab.com:myorg/myrepo.git",
            ],
        );

        let svc = GitWorktreeService;
        let result = svc.github_remote(&repo_dir).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn github_remote_returns_none_when_no_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        // No remote configured at all.
        let svc = GitWorktreeService;
        let result = svc.github_remote(&repo_dir).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn invalid_repo_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let not_a_repo = tmp.path().join("not-a-repo");
        fs::create_dir_all(&not_a_repo).unwrap();

        let svc = GitWorktreeService;
        let result = svc.list_worktrees(&not_a_repo);
        assert!(result.is_err());
    }

    /// F-2 regression: github_remote() must propagate git errors that are
    /// NOT "no such remote". Only the specific "no such remote" error
    /// should map to Ok(None); other failures (corruption, permissions)
    /// must surface as Err.
    #[test]
    fn github_remote_propagates_non_no_such_remote_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        // Add a valid remote so "no such remote" is NOT the error.
        run_in(
            &repo_dir,
            &["git", "remote", "add", "origin", "git@github.com:o/r.git"],
        );

        // Corrupt the git config to make `git remote get-url origin` fail
        // with an error that is NOT "no such remote".
        let config_path = repo_dir.join(".git/config");
        fs::write(&config_path, "this is not valid git config").unwrap();

        let svc = GitWorktreeService;
        let result = svc.github_remote(&repo_dir);

        assert!(
            result.is_err(),
            "github_remote should propagate non-'no such remote' git errors, got: {:?}",
            result,
        );
    }

    /// F-1 regression: fetch_branch should fetch a branch from origin so
    /// the local ref points at the correct commit, and fail when the
    /// branch does not exist on origin.
    #[test]
    fn fetch_branch_fetches_from_origin() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a bare "remote" repo.
        let remote_dir = tmp.path().join("remote.git");
        fs::create_dir_all(&remote_dir).unwrap();
        run_in(&remote_dir, &["git", "init", "--bare"]);

        // Create a "source" repo, push a branch to the remote.
        let source_dir = tmp.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        setup_git_repo(&source_dir);
        // Normalize to "main" so the test is portable across git configs.
        run_in(&source_dir, &["git", "branch", "-m", "main"]);
        run_in(
            &source_dir,
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote_dir.to_str().unwrap(),
            ],
        );
        run_in(&source_dir, &["git", "push", "-u", "origin", "main"]);
        // Create a feature branch with a unique commit.
        run_in(&source_dir, &["git", "checkout", "-b", "pr-branch"]);
        let pr_file = source_dir.join("pr-change.txt");
        fs::write(&pr_file, "PR content").unwrap();
        run_in(&source_dir, &["git", "add", "pr-change.txt"]);
        commit_in(&source_dir, "PR commit");
        run_in(&source_dir, &["git", "push", "origin", "pr-branch"]);

        // Get the commit SHA on pr-branch in source.
        let expected_sha = Command::new("git")
            .arg("-C")
            .arg(&source_dir)
            .args(["rev-parse", "pr-branch"])
            .output()
            .unwrap();
        let expected_sha = String::from_utf8(expected_sha.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Create a "local" clone that does NOT have pr-branch locally yet.
        let local_dir = tmp.path().join("local");
        fs::create_dir_all(&local_dir).unwrap();
        setup_git_repo(&local_dir);
        run_in(&local_dir, &["git", "branch", "-m", "main"]);
        run_in(
            &local_dir,
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote_dir.to_str().unwrap(),
            ],
        );

        let svc = GitWorktreeService;

        // Before fetch: pr-branch should not exist locally.
        let before = GitWorktreeService::run_git(
            &local_dir,
            &["rev-parse", "--verify", "refs/heads/pr-branch"],
        );
        assert!(
            before.is_err(),
            "pr-branch should not exist locally before fetch",
        );

        // Fetch the branch.
        svc.fetch_branch(&local_dir, "pr-branch").unwrap();

        // After fetch: pr-branch should exist locally at the correct SHA.
        let actual_sha =
            GitWorktreeService::run_git(&local_dir, &["rev-parse", "refs/heads/pr-branch"])
                .unwrap()
                .trim()
                .to_string();
        assert_eq!(
            actual_sha, expected_sha,
            "local pr-branch should point at the same commit as origin",
        );
    }

    /// F-1 regression: fetch_branch should fail when the branch does not
    /// exist on origin (e.g. fork PR branch).
    #[test]
    fn fetch_branch_fails_for_nonexistent_branch() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a bare "remote" repo.
        let remote_dir = tmp.path().join("remote.git");
        fs::create_dir_all(&remote_dir).unwrap();
        run_in(&remote_dir, &["git", "init", "--bare"]);

        // Create a local repo with origin pointing to the bare remote.
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);
        // Normalize to "main" so the test is portable across git configs.
        run_in(&repo_dir, &["git", "branch", "-m", "main"]);
        run_in(
            &repo_dir,
            &[
                "git",
                "remote",
                "add",
                "origin",
                remote_dir.to_str().unwrap(),
            ],
        );
        run_in(&repo_dir, &["git", "push", "-u", "origin", "main"]);

        let svc = GitWorktreeService;

        // Try to fetch a branch that does not exist on origin.
        let result = svc.fetch_branch(&repo_dir, "nonexistent-branch");
        assert!(
            result.is_err(),
            "fetch_branch should fail for a branch not on origin, got: {:?}",
            result,
        );
    }

    #[test]
    fn create_branch_creates_from_default() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);
        // Normalize to "master" (git init default).
        run_in(&repo_dir, &["git", "branch", "-m", "master"]);

        // Advance HEAD away from master so we can verify the new branch
        // starts from master (default branch), not from HEAD.
        run_in(&repo_dir, &["git", "checkout", "-b", "detour"]);
        let detour_file = repo_dir.join("detour.txt");
        fs::write(&detour_file, "detour content").unwrap();
        run_in(&repo_dir, &["git", "add", "detour.txt"]);
        commit_in(&repo_dir, "detour commit");

        let master_sha =
            GitWorktreeService::run_git(&repo_dir, &["rev-parse", "refs/heads/master"])
                .unwrap()
                .trim()
                .to_string();
        let head_sha = GitWorktreeService::run_git(&repo_dir, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        // HEAD should differ from master.
        assert_ne!(master_sha, head_sha, "HEAD should differ from master");

        let svc = GitWorktreeService;

        // Verify the branch does not exist yet.
        let before = GitWorktreeService::run_git(
            &repo_dir,
            &["rev-parse", "--verify", "refs/heads/my-feature"],
        );
        assert!(
            before.is_err(),
            "my-feature should not exist before create_branch",
        );

        // Create the branch - should be based on master, not HEAD.
        svc.create_branch(&repo_dir, "my-feature").unwrap();

        // Verify it now exists.
        let feature_sha =
            GitWorktreeService::run_git(&repo_dir, &["rev-parse", "refs/heads/my-feature"])
                .unwrap()
                .trim()
                .to_string();
        assert_eq!(
            feature_sha, master_sha,
            "new branch should start from default branch (master), not HEAD",
        );
    }

    #[test]
    fn create_branch_noop_if_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        setup_git_repo(&repo_dir);

        // Create the branch first.
        run_in(&repo_dir, &["git", "branch", "existing-branch"]);

        let svc = GitWorktreeService;
        // Should succeed without error (no-op).
        svc.create_branch(&repo_dir, "existing-branch").unwrap();

        // Branch should still exist.
        let check = GitWorktreeService::run_git(
            &repo_dir,
            &["rev-parse", "--verify", "refs/heads/existing-branch"],
        );
        assert!(check.is_ok(), "branch should still exist");
    }
}
