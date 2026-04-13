//! Pull-request operations via the `gh` CLI, abstracted behind a trait so
//! the background delete-cleanup thread can be exercised in tests without
//! actually shelling out to GitHub.
//!
//! Currently only exposes `close_pr` because that is the only operation
//! the delete flow needs. Other `gh` invocations in the codebase still
//! call `std::process::Command::new("gh")` directly; they can be migrated
//! to this trait when their own code paths grow test coverage.

use std::sync::Arc;

/// Close a GitHub pull request. Implementations must be `Send + Sync`
/// because the delete-cleanup path calls `close_pr` from a background
/// thread with the implementation captured by `Arc`.
pub trait PullRequestCloser: Send + Sync {
    /// Close the PR with the given number in `<owner>/<repo>`. Returns
    /// `Err(message)` on failure. The message is plumbed through to the
    /// user as a warning; callers should not parse it.
    fn close_pr(&self, owner: &str, repo: &str, pr_number: u64) -> Result<(), String>;
}

/// Production implementation that shells out to `gh pr close`.
pub struct GhPullRequestCloser;

impl PullRequestCloser for GhPullRequestCloser {
    fn close_pr(&self, owner: &str, repo: &str, pr_number: u64) -> Result<(), String> {
        let owner_repo = format!("{owner}/{repo}");
        match std::process::Command::new("gh")
            .args(["pr", "close", &pr_number.to_string(), "--repo", &owner_repo])
            .output()
        {
            Ok(output) if !output.status.success() => {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
            }
            Err(e) => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// Construct the default production closer wrapped in `Arc` for sharing
/// across threads.
pub fn default_pr_closer() -> Arc<dyn PullRequestCloser> {
    Arc::new(GhPullRequestCloser)
}
