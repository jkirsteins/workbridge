use super::{GithubClient, GithubError, GithubIssue, GithubPr, LivePrState};

/// Inert GitHub client that always reports "no open PR" for the
/// merge-precheck path and errors out of every other method. Used as
/// the default for `App::with_config_and_worktree_service` so tests
/// and other non-production construction sites do not have to
/// construct a real `GhCliClient` (which would shell out to `gh`).
/// Production `main.rs` passes a real `GhCliClient` via
/// `App::with_config_worktree_and_github` and never touches this
/// stub.
pub struct StubGithubClient;

impl GithubClient for StubGithubClient {
    fn list_open_prs(&self, _owner: &str, _repo: &str) -> Result<Vec<GithubPr>, GithubError> {
        Ok(Vec::new())
    }

    fn list_review_requested_prs(
        &self,
        _owner: &str,
        _repo: &str,
    ) -> Result<Vec<GithubPr>, GithubError> {
        Ok(Vec::new())
    }

    fn get_issue(
        &self,
        _owner: &str,
        _repo: &str,
        number: u64,
    ) -> Result<GithubIssue, GithubError> {
        Err(GithubError::ApiError(format!(
            "stub github client cannot fetch issue #{number}"
        )))
    }

    /// Matches the contract documented on the trait method: "no open
    /// PR found" is represented by `LivePrState::no_pr()`, not an
    /// error. Returning `no_pr()` here keeps the merge-precheck
    /// classifier in the "no remote constraints" arm when the App
    /// was constructed without a real GitHub client.
    fn fetch_live_merge_state(
        &self,
        _owner: &str,
        _repo: &str,
        _branch: &str,
    ) -> Result<LivePrState, GithubError> {
        Ok(LivePrState::no_pr())
    }
}
