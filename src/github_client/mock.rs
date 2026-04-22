use super::{GithubClient, GithubError, GithubIssue, GithubPr, LivePrState};

/// Mock GitHub client for tests. Returns configurable fixture data.
pub struct MockGithubClient {
    pub prs: Vec<GithubPr>,
    pub review_requested_prs: Vec<GithubPr>,
    pub issues: Vec<GithubIssue>,
    /// If set, all calls return this error instead of fixture data.
    pub error: Option<GithubError>,
    /// Fixture result for `fetch_live_merge_state`. When `None`, the
    /// trait default (returns an error) applies - the merge precheck
    /// tests that do not exercise the live merge-state path therefore
    /// do not need to set this field. Set to `Some(Ok(...))` to drive
    /// the conflict / ci-failing / clean-PR code paths; set to
    /// `Some(Err(...))` to drive the "remote fetch failed" branch.
    pub live_pr_state: Option<Result<LivePrState, GithubError>>,
}

impl MockGithubClient {
    pub fn new() -> Self {
        Self {
            prs: Vec::new(),
            review_requested_prs: Vec::new(),
            issues: Vec::new(),
            error: None,
            live_pr_state: None,
        }
    }
}

impl GithubClient for MockGithubClient {
    fn list_open_prs(&self, _owner: &str, _repo: &str) -> Result<Vec<GithubPr>, GithubError> {
        if let Some(ref err) = self.error {
            return Err(err.clone());
        }
        Ok(self.prs.clone())
    }

    fn list_review_requested_prs(
        &self,
        _owner: &str,
        _repo: &str,
    ) -> Result<Vec<GithubPr>, GithubError> {
        if let Some(ref err) = self.error {
            return Err(err.clone());
        }
        Ok(self.review_requested_prs.clone())
    }

    fn get_issue(
        &self,
        _owner: &str,
        _repo: &str,
        number: u64,
    ) -> Result<GithubIssue, GithubError> {
        if let Some(ref err) = self.error {
            return Err(err.clone());
        }
        self.issues
            .iter()
            .find(|i| i.number == number)
            .cloned()
            .ok_or_else(|| GithubError::ApiError(format!("issue #{number} not found")))
    }

    fn list_merged_prs(&self, _owner: &str, _repo: &str) -> Result<Vec<GithubPr>, GithubError> {
        if let Some(ref err) = self.error {
            return Err(err.clone());
        }
        Ok(self
            .prs
            .iter()
            .filter(|p| p.state == "MERGED")
            .cloned()
            .collect())
    }

    /// Mock override. Returns the `live_pr_state` fixture when set
    /// (so tests can drive the conflict / CI-failing / clean / error
    /// branches of `spawn_merge_precheck`), otherwise falls back to
    /// `LivePrState::no_pr()` so tests that do not care about the
    /// live merge-state path still get a non-blocking default.
    fn fetch_live_merge_state(
        &self,
        _owner: &str,
        _repo: &str,
        _branch: &str,
    ) -> Result<LivePrState, GithubError> {
        if let Some(ref fixture) = self.live_pr_state {
            return fixture.clone();
        }
        Ok(LivePrState::no_pr())
    }

    /// Mock override. Returns the shared fixture error when `error`
    /// is set (so tests can exercise the "lookup failed" branch of
    /// the fetcher), otherwise returns a stable mock login so every
    /// test that does not care about identity still gets a usable
    /// value and the fetcher does not emit a spurious `FetcherError`.
    fn current_user_login(&self) -> Result<String, GithubError> {
        if let Some(ref err) = self.error {
            return Err(err.clone());
        }
        Ok("mock-user".into())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{GithubClient, GithubError, GithubIssue, GithubPr};
    use super::MockGithubClient;

    #[test]
    fn mock_client_returns_fixture_prs() {
        let client = MockGithubClient {
            prs: vec![GithubPr {
                number: 42,
                title: "Fix bug".into(),
                state: "OPEN".into(),
                is_draft: false,
                head_branch: "42-fix-bug".into(),
                url: "https://github.com/o/r/pull/42".into(),
                review_decision: "APPROVED".into(),
                status_check_rollup: "SUCCESS".into(),
                head_repo_owner: None,
                author: None,
                mergeable: "MERGEABLE".into(),
                requested_reviewer_logins: Vec::new(),
                requested_team_slugs: Vec::new(),
            }],
            issues: Vec::new(),
            review_requested_prs: Vec::new(),

            error: None,
            live_pr_state: None,
        };

        let prs = client.list_open_prs("o", "r").unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 42);
    }

    #[test]
    fn mock_client_returns_error() {
        let client = MockGithubClient {
            prs: Vec::new(),
            issues: Vec::new(),
            review_requested_prs: Vec::new(),

            error: Some(GithubError::AuthRequired),
            live_pr_state: None,
        };

        let result = client.list_open_prs("o", "r");
        assert!(result.is_err());
    }

    #[test]
    fn mock_client_get_issue_found() {
        let client = MockGithubClient {
            prs: Vec::new(),
            issues: vec![GithubIssue {
                number: 7,
                title: "Add feature".into(),
                state: "open".into(),
                labels: vec!["enhancement".into()],
            }],
            review_requested_prs: Vec::new(),

            error: None,
            live_pr_state: None,
        };

        let issue = client.get_issue("o", "r", 7).unwrap();
        assert_eq!(issue.title, "Add feature");
    }

    #[test]
    fn mock_client_get_issue_not_found() {
        let client = MockGithubClient::new();
        let result = client.get_issue("o", "r", 999);
        assert!(result.is_err());
    }
}
