//! `GitWorktreeService`: the real `WorktreeService` implementation that
//! shells out to the git CLI. The struct, its trait implementation, and
//! the private `run_git` / `find_branch_for_worktree` helpers live here.
//! Pure parsing helpers for git's porcelain output live in `parsers`;
//! integration tests that exercise a real git binary live in the
//! `integration_tests` submodule behind the `integration` feature gate.

use std::path::{Path, PathBuf};

use super::{WorktreeError, WorktreeInfo, WorktreeService, git_command};
use crate::github_client::parse_github_remote;

mod parsers;

#[cfg(all(test, feature = "integration"))]
mod integration_tests;

/// `GitWorktreeService` shells out to the git CLI for worktree operations.
pub struct GitWorktreeService;

impl GitWorktreeService {
    /// Run a git command with `-C repo_path` and return stdout on success.
    ///
    /// Clears inherited git env vars (`GIT_DIR`, `GIT_WORK_TREE`, etc.) so the
    /// command operates on `repo_path` rather than a parent worktree. This
    /// matters when the process runs inside a git hook (e.g. pre-push) where
    /// git sets these variables.
    fn run_git(repo_path: &Path, args: &[&str]) -> Result<String, WorktreeError> {
        let output = git_command()
            .arg("-C")
            .arg(repo_path)
            // Clear inherited git env vars so -C is authoritative.
            // Without this, GIT_DIR from a parent worktree context
            // can override the -C target.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .args(args)
            .output()
            .map_err(|e| WorktreeError::Io(format!("failed to run git: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            // Combine both streams so no git diagnostic is lost. Some git
            // commands put progress text on one stream and the fatal error
            // on the other depending on the git version.
            let combined = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
                (true, _) => stderr,
                (_, true) => stdout,
                (false, false) => format!("{stderr}\n{stdout}"),
            };
            // Detect invalid repo from common git error messages.
            if combined.contains("not a git repository") {
                return Err(WorktreeError::InvalidRepo(repo_path.to_path_buf()));
            }
            return Err(WorktreeError::GitError(combined));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| WorktreeError::GitError(format!("invalid UTF-8 in git output: {e}")))
    }

    /// Find the branch name for a worktree at the given path by looking
    /// through the list of all worktrees.
    /// Called from `remove_worktree`; also used in integration tests.
    fn find_branch_for_worktree(
        repo_path: &Path,
        worktree_path: &Path,
    ) -> Result<Option<String>, WorktreeError> {
        let output = Self::run_git(repo_path, &["worktree", "list", "--porcelain"])?;
        let worktrees = parsers::parse_porcelain(&output);
        // Canonicalize the target path to handle symlinks (e.g. /tmp ->
        // /private/tmp on macOS) so it matches the paths git reports.
        let canonical_target = crate::config::canonicalize_path(worktree_path)
            .unwrap_or_else(|_| worktree_path.to_path_buf());
        Ok(worktrees
            .into_iter()
            .find(|w| {
                let canonical_w =
                    crate::config::canonicalize_path(&w.path).unwrap_or_else(|_| w.path.clone());
                canonical_w == canonical_target
            })
            .and_then(|w| w.branch))
    }
}

impl WorktreeService for GitWorktreeService {
    fn list_worktrees(&self, repo_path: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let output = Self::run_git(repo_path, &["worktree", "list", "--porcelain"])?;
        let mut worktrees = parsers::parse_porcelain(&output);

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

        // Populate cached per-worktree state for each non-main worktree so
        // UI-thread code (`App::branch_has_commits` and the unclean-chip
        // renderer in `format_work_item_entry`) can consult the cache
        // instead of shelling out. Each populated
        // field is documented on `WorktreeInfo` itself; we run three git
        // commands per worktree on the background fetcher thread:
        //
        //   1. `git log <default>..HEAD --oneline`           -> has_commits_ahead
        //   2. `git status --porcelain -uall`                -> dirty, untracked
        //   3. `git rev-list --left-right --count HEAD...@{u}` -> unpushed, behind_remote
        //
        // The main worktree is skipped because the review-gate /
        // retroactive-analysis flows only care about side-branch
        // worktrees; detached-HEAD worktrees are skipped because they
        // have no branch-ish HEAD so the range queries are meaningless.
        // On any individual command failure the corresponding field stays
        // `None` so callers fall back to "unknown / safe default" rather
        // than retrying on the UI thread.
        for wt in &mut worktrees {
            if wt.is_main {
                continue;
            }
            if wt.branch.is_none() {
                continue;
            }

            // 1. has_commits_ahead: is this branch ahead of the default
            //    branch at all? Run from inside the worktree so HEAD
            //    resolves to that worktree's branch tip even when
            //    multiple worktrees are checked out.
            let range = format!("{default}..HEAD");
            let ahead_out = git_command()
                .args(["log", &range, "--oneline"])
                .current_dir(&wt.path)
                .output();
            wt.has_commits_ahead = match ahead_out {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    Some(!stdout.trim().is_empty())
                }
                _ => None,
            };

            // 2. dirty + untracked: `git status --porcelain -uall`. The
            //    `-uall` flag ensures untracked files inside untracked
            //    directories are listed one-per-line (the default
            //    `-unormal` collapses them into a single `??` line for
            //    the directory, which still parses correctly here but
            //    `-uall` is more predictable across git versions).
            let status_out = git_command()
                .args(["status", "--porcelain", "-uall"])
                .current_dir(&wt.path)
                .output();
            match status_out {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    let (dirty, untracked) = parsers::parse_status_porcelain(&stdout);
                    wt.dirty = Some(dirty);
                    wt.untracked = Some(untracked);
                }
                _ => {
                    // Leave both as `None` so readers fall back to "unknown".
                }
            }

            // 3. unpushed + behind_remote: `git rev-list --left-right
            //    --count HEAD...@{u}`. A non-zero exit typically means
            //    the branch has no upstream configured (e.g. freshly
            //    created local branch never pushed). In that case both
            //    fields stay `None` - the cleanliness helper treats
            //    missing upstream data as "nothing to warn about" so
            //    unpublished branches do not accidentally get flagged
            //    as dirty. A branch that IS published but has unpushed
            //    commits will exit 0 and return a non-zero left count.
            let revlist_out = git_command()
                .args(["rev-list", "--left-right", "--count", "HEAD...@{u}"])
                .current_dir(&wt.path)
                .output();
            if let Ok(o) = revlist_out
                && o.status.success()
            {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if let Some((ahead, behind)) = parsers::parse_rev_list_left_right(&stdout) {
                    wt.unpushed = Some(ahead);
                    wt.behind_remote = Some(behind);
                }
            }
        }

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

        // Detect two forms of "branch locked to existing worktree":
        // 1. Target path already exists (stale worktree at the expected
        //    location): "fatal: '<path>' already exists"
        // 2. Branch checked out at a different path: "fatal: '<branch>'
        //    is already used by worktree at '<path>'"
        // Both mean a stale worktree is blocking creation. Case 1 is the
        // common case (create_worktree always targets the canonical path,
        // which is where the stale worktree sits).
        if let Err(WorktreeError::GitError(ref msg)) = result {
            let is_locked = msg.contains("is already used by worktree at");
            let target_exists = msg.contains("already exists");
            if is_locked || target_exists {
                let locked_at = if is_locked {
                    parsers::parse_locked_worktree_path(msg)
                } else {
                    // "already exists" means the target dir itself is
                    // the stale worktree.
                    Some(target_dir.to_path_buf())
                }
                .unwrap_or_else(|| PathBuf::from("(unknown)"));
                return Err(WorktreeError::BranchLockedToWorktree {
                    branch: branch.to_string(),
                    locked_at,
                });
            }
        }
        result?;

        Ok(WorktreeInfo {
            path: target_dir.to_path_buf(),
            branch: Some(branch.to_string()),
            is_main: false,
            // `has_commits_ahead: None` means "not yet resolved" - the
            // background fetcher will compute the real answer on its
            // next cycle. Returning `Some(false)` eagerly would be
            // silently wrong for the `branch_exists` path, where an
            // existing branch being attached to a new worktree may
            // already be ahead of the default branch. `None` is the
            // safe default: the cache-reading helpers
            // (`branch_has_commits`) treat it the same as `Some(false)`
            // and defer the decision to the fetcher.
            has_commits_ahead: None,
            // Cleanliness fields start unresolved for the same reason:
            // the next fetcher cycle will populate them. Readers treat
            // `None` as "unknown / assume clean" so this cannot make a
            // brand-new worktree render as dirty.
            dirty: None,
            untracked: None,
            unpushed: None,
            behind_remote: None,
        })
    }

    fn remove_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        delete_branch: bool,
        force: bool,
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
        if force {
            Self::run_git(repo_path, &["worktree", "remove", "--force", wt_str])?;
        } else {
            Self::run_git(repo_path, &["worktree", "remove", wt_str])?;
        }

        if let Some(branch_name) = branch {
            let flag = if force { "-D" } else { "-d" };
            Self::run_git(repo_path, &["branch", flag, &branch_name])?;
        }

        Ok(())
    }

    fn delete_branch(
        &self,
        repo_path: &Path,
        branch: &str,
        force: bool,
    ) -> Result<(), WorktreeError> {
        let flag = if force { "-D" } else { "-d" };
        Self::run_git(repo_path, &["branch", flag, branch])?;
        Ok(())
    }

    fn default_branch(&self, repo_path: &Path) -> Result<String, WorktreeError> {
        if let Ok(output) = Self::run_git(repo_path, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        {
            let trimmed = output.trim();
            // Output is like "refs/remotes/origin/main" - strip the prefix.
            let branch = trimmed
                .strip_prefix("refs/remotes/origin/")
                .unwrap_or(trimmed);
            Ok(branch.to_string())
        } else {
            // No origin/HEAD available. Check which of "main" or "master"
            // exists as a local branch. If both exist, prefer "main". If
            // neither exists, fall back to "main" as a convention default.
            let main_exists =
                Self::run_git(repo_path, &["rev-parse", "--verify", "refs/heads/main"]).is_ok();
            if main_exists {
                return Ok("main".to_string());
            }
            let master_exists =
                Self::run_git(repo_path, &["rev-parse", "--verify", "refs/heads/master"]).is_ok();
            if master_exists {
                return Ok("master".to_string());
            }
            // Neither exists - use "main" as convention default.
            Ok("main".to_string())
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

        // `git branch` is a pure ref operation: it writes a single file
        // under `.git/refs/heads/<name>` pointing at the commit `base`
        // resolves to. It never reads the working tree, so the main repo's
        // dirty state is irrelevant here. See commit 9b25497 for the
        // historical false-positive check that used to live here.
        Self::run_git(repo_path, &["branch", branch, &base])?;
        Ok(())
    }

    fn prune_worktrees(&self, repo_path: &Path) -> Result<(), WorktreeError> {
        Self::run_git(repo_path, &["worktree", "prune"])?;
        Ok(())
    }
}
