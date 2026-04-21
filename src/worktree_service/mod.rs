//! Worktree service module.
//!
//! Public surface: the `WorktreeService` trait and the concrete
//! `GitWorktreeService` implementation (re-exported from the `git_impl`
//! submodule), plus the shared `WorktreeError` / `WorktreeInfo` value
//! types and the `git_command()` helper used by any caller that needs
//! to shell out to git with the inherited `GIT_DIR` / `GIT_WORK_TREE`
//! environment variables stripped.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

mod git_impl;

pub use git_impl::GitWorktreeService;

/// Errors from worktree operations.
#[derive(Clone, Debug)]
pub enum WorktreeError {
    /// git command failed with an error message.
    GitError(String),
    /// I/O error during worktree operations.
    Io(String),
    /// The repo path does not exist or is not a git repo.
    InvalidRepo(PathBuf),
    /// The branch is already locked to another worktree (e.g. after an
    /// interrupted rebase leaves the worktree mid-rebase with a detached
    /// HEAD). Fields: branch name, path where git says the branch is
    /// locked, full git error message.
    BranchLockedToWorktree { branch: String, locked_at: PathBuf },
}

impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitError(msg) => write!(f, "git error: {msg}"),
            Self::Io(msg) => write!(f, "worktree I/O error: {msg}"),
            Self::InvalidRepo(path) => {
                write!(f, "invalid git repo: {}", path.display())
            }
            Self::BranchLockedToWorktree { branch, locked_at } => {
                write!(
                    f,
                    "branch '{}' is locked to worktree at '{}'",
                    branch,
                    locked_at.display()
                )
            }
        }
    }
}

/// Information about a single worktree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Filesystem path to the worktree.
    pub path: PathBuf,
    /// Branch the worktree is on, or None if detached HEAD.
    pub branch: Option<String>,
    /// True if this is the main worktree (the repo's primary checkout).
    pub is_main: bool,
    /// Cached answer to "does this worktree's branch have commits ahead of
    /// the repo's default branch?" - populated by `list_worktrees` during
    /// the background fetch cycle so UI-thread code can read the result
    /// synchronously without shelling out to `git log`. `None` means the
    /// check was not attempted (detached HEAD, the main worktree, or the
    /// check failed); the UI thread must treat `None` as "unknown - safe
    /// default" rather than retrying the shell-out.
    pub has_commits_ahead: Option<bool>,
    /// Cached answer to "does this worktree have uncommitted tracked-file
    /// changes (modified/staged/renamed/deleted)?" - populated by
    /// `list_worktrees` from `git status --porcelain -uall`. `None` means
    /// the check was not attempted or failed; UI-thread readers treat
    /// `None` as "unknown" and fall back to their safe default. See the
    /// "Unclean worktree indicator + merge guard" flow for how the
    /// live precheck feeds this through `MergeReadiness::classify`.
    pub dirty: Option<bool>,
    /// Cached answer to "does this worktree have any untracked files?" -
    /// populated by `list_worktrees` from the same
    /// `git status --porcelain -uall` call that sets `dirty`. `None` on
    /// failure; treat as "unknown" / safe default.
    pub untracked: Option<bool>,
    /// Number of commits the worktree's branch is ahead of its upstream
    /// (i.e. commits that exist locally but not on `@{u}`). Populated by
    /// `list_worktrees` from
    /// `git rev-list --left-right --count HEAD...@{u}`. `None` means the
    /// branch has no upstream configured, the check failed, or the
    /// worktree was skipped (main / detached HEAD).
    pub unpushed: Option<u32>,
    /// Number of commits the worktree's branch is behind its upstream
    /// (i.e. commits that exist on `@{u}` but not locally). Populated by
    /// the same `git rev-list` call as `unpushed`. `None` on missing
    /// upstream / failure.
    pub behind_remote: Option<u32>,
}

/// Trait for worktree operations. Implementations include
/// `GitWorktreeService` (shells out to git CLI) and test mocks.
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
    /// When `force` is true, uses `--force` to remove dirty worktrees and
    /// `-D` (instead of `-d`) for branch deletion.
    fn remove_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        delete_branch: bool,
        force: bool,
    ) -> Result<(), WorktreeError>;

    /// Delete a local branch. When `force` is true, uses `-D` (force delete)
    /// instead of `-d` (safe delete that refuses unmerged branches).
    fn delete_branch(
        &self,
        repo_path: &Path,
        branch: &str,
        force: bool,
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
    /// Used as a fallback when `fetch_branch` fails (e.g., the branch does
    /// not exist on origin yet).
    fn create_branch(&self, repo_path: &Path, branch: &str) -> Result<(), WorktreeError>;

    /// Prune stale worktree bookkeeping entries. Equivalent to
    /// `git worktree prune`. Used during recovery after a stale
    /// worktree is force-removed.
    fn prune_worktrees(&self, repo_path: &Path) -> Result<(), WorktreeError>;
}

/// Build a `Command::new("git")` with inherited git env vars cleared.
///
/// Git sets `GIT_DIR`, `GIT_WORK_TREE`, etc. when running inside hooks or
/// worktrees. Clearing them ensures each command targets only the
/// directory it's told to via `-C` or `current_dir()`.
pub fn git_command() -> Command {
    let mut cmd = Command::new("git");
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        cmd.env_remove(var);
    }
    cmd
}
