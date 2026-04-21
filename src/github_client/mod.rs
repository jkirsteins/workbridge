use std::fmt;

use serde::{Deserialize, Serialize};

use crate::work_item::{CheckStatus, MergeableState};

#[cfg(test)]
mod mock;
mod real;
#[cfg(test)]
mod stub;

#[cfg(test)]
pub use mock::MockGithubClient;
pub use real::GhCliClient;
#[cfg(test)]
pub use stub::StubGithubClient;

/// Errors from GitHub API operations.
#[derive(Clone, Debug)]
pub enum GithubError {
    /// gh CLI is not installed or not on PATH.
    CliNotFound,
    /// gh CLI returned an auth error (not logged in).
    AuthRequired,
    /// gh CLI or API returned an error with a message.
    ApiError(String),
    /// Failed to parse the JSON response from gh CLI.
    ParseError(String),
}

impl fmt::Display for GithubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CliNotFound => write!(f, "gh CLI not found"),
            Self::AuthRequired => write!(f, "gh auth required"),
            Self::ApiError(msg) => write!(f, "GitHub API error: {msg}"),
            Self::ParseError(msg) => write!(f, "GitHub parse error: {msg}"),
        }
    }
}

/// A raw PR as returned by the GitHub API (via gh CLI).
/// Includes `head_branch` for matching against work item repo associations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GithubPr {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub is_draft: bool,
    pub head_branch: String,
    pub url: String,
    /// Raw review decision string from GitHub (e.g. "APPROVED",
    /// "`CHANGES_REQUESTED`", "`REVIEW_REQUIRED`", or empty).
    pub review_decision: String,
    /// Raw status check rollup (e.g. "SUCCESS", "PENDING", "FAILURE",
    /// or empty).
    pub status_check_rollup: String,
    /// The owner (login) of the head repository. None if the gh CLI
    /// did not return the field. For same-repo PRs this matches the
    /// repo owner; for fork PRs it is the fork owner's login.
    pub head_repo_owner: Option<String>,
    /// The login of the PR author. None if the gh CLI did not return
    /// the field (backwards compatibility with older fetch results).
    pub author: Option<String>,
    /// Raw mergeable state from GitHub (e.g. "MERGEABLE", "CONFLICTING",
    /// "UNKNOWN", or empty). Indicates whether the PR has a merge conflict
    /// against its base branch.
    pub mergeable: String,
    /// Logins of users the PR has explicitly requested for review.
    /// Populated only by `list_review_requested_prs` (the open-PR and
    /// merged-PR paths do not ask `gh` for this field and leave it
    /// empty). Used to classify review-request rows as direct-to-you
    /// vs. team-requested. `#[serde(default)]` lets legacy serialized
    /// fetch results (persisted before this field existed) deserialize
    /// with an empty vec instead of erroring.
    #[serde(default)]
    pub requested_reviewer_logins: Vec<String>,
    /// Slugs of teams the PR has explicitly requested for review.
    /// Populated only by `list_review_requested_prs`. Used to build the
    /// reviewer badge and the "Requested from:" detail panel line.
    #[serde(default)]
    pub requested_team_slugs: Vec<String>,
}

/// Live PR merge-state signals fetched on the merge-precheck
/// background thread via `GithubClient::fetch_live_merge_state`.
/// Packaged as a struct so the classifier in
/// `MergeReadiness::classify` can consume both remote dimensions
/// together, and so the "no open PR" sentinel is a single value
/// (`LivePrState::no_pr()`) rather than three parallel `Option`s.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LivePrState {
    pub mergeable: MergeableState,
    pub check_rollup: CheckStatus,
    /// `false` when `gh pr view` reported no open PR for the branch.
    /// The classifier treats this as "no remote constraints" so a
    /// clean worktree with no PR still resolves to `Clean`; the
    /// downstream merge thread then surfaces the existing `NoPr`
    /// outcome.
    pub has_open_pr: bool,
}

impl LivePrState {
    /// Sentinel for "no open PR exists for this branch". The
    /// classifier skips remote checks when `has_open_pr` is false,
    /// so the `Unknown` defaults for `mergeable` / `check_rollup`
    /// never surface in a merge decision.
    pub const fn no_pr() -> Self {
        Self {
            mergeable: MergeableState::Unknown,
            check_rollup: CheckStatus::None,
            has_open_pr: false,
        }
    }
}

/// A raw issue as returned by the GitHub API (via gh CLI).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GithubIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub labels: Vec<String>,
}

/// Trait for interacting with GitHub. Implementations include `GhCliClient`
/// (shells out to `gh`) and `MockGithubClient` (returns fixture data for
/// tests).
pub trait GithubClient: Send + Sync {
    /// List all open PRs for a given owner/repo.
    fn list_open_prs(&self, owner: &str, repo: &str) -> Result<Vec<GithubPr>, GithubError>;

    /// List open PRs where the authenticated user is a requested reviewer.
    fn list_review_requested_prs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<GithubPr>, GithubError>;

    /// Get a single issue by number.
    fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<GithubIssue, GithubError>;

    /// List merged PRs for a given owner/repo. Used to backfill PR identity
    /// for Done items that were merged before persistence was added.
    fn list_merged_prs(&self, owner: &str, repo: &str) -> Result<Vec<GithubPr>, GithubError> {
        let _ = (owner, repo);
        Ok(vec![])
    }

    /// Resolve the current GitHub user's login (e.g. `alice`). The
    /// production `GhCliClient` caches the first successful result; the
    /// default trait impl simply returns an error so test doubles that
    /// do not need this information are not forced to stub it.
    fn current_user_login(&self) -> Result<String, GithubError> {
        Err(GithubError::ApiError(
            "current_user_login not implemented for this client".into(),
        ))
    }

    /// Re-fetch the live merge-state signals (mergeable flag + CI
    /// rollup) for a single branch's open PR. Called exclusively from
    /// `App::spawn_merge_precheck` on the merge-precheck background
    /// thread, so it may block for as long as `gh` takes to respond.
    ///
    /// Returns `LivePrState::no_pr()` when `gh pr view` reports no
    /// open PR for the branch. The merge precheck treats this as
    /// "no remote constraints" and classifies on the local worktree
    /// state alone; the downstream merge thread then surfaces the
    /// existing `NoPr` outcome.
    ///
    /// The default impl returns an error so test doubles that do
    /// not need this path do not have to stub it. `MockGithubClient`
    /// overrides it via a configurable fixture; `GhCliClient`
    /// overrides it to shell out to `gh pr view`.
    fn fetch_live_merge_state(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<LivePrState, GithubError> {
        let _ = (owner, repo, branch);
        Err(GithubError::ApiError(
            "fetch_live_merge_state not implemented for this client".into(),
        ))
    }
}

/// Extract owner and repo name from a GitHub remote URL.
///
/// Supports both SSH (git@github.com:owner/repo.git) and HTTPS
/// (<https://github.com/owner/repo.git>) formats. Returns None for
/// non-GitHub URLs.
pub fn parse_github_remote(url: &str) -> Option<(String, String)> {
    // SSH format: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
        return None;
    }

    // HTTPS format: https://github.com/owner/repo.git
    let url_trimmed = url.strip_suffix(".git").unwrap_or(url);
    for prefix in &["https://github.com/", "http://github.com/"] {
        if let Some(rest) = url_trimmed.strip_prefix(prefix) {
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                return Some((parts[0].to_string(), parts[1].to_string()));
            }
            return None;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_url() {
        let result = parse_github_remote("git@github.com:owner/repo.git");
        assert_eq!(result, Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn parse_ssh_url_no_git_suffix() {
        let result = parse_github_remote("git@github.com:owner/repo");
        assert_eq!(result, Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn parse_https_url() {
        let result = parse_github_remote("https://github.com/owner/repo.git");
        assert_eq!(result, Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn parse_https_url_no_git_suffix() {
        let result = parse_github_remote("https://github.com/owner/repo");
        assert_eq!(result, Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn parse_http_url() {
        let result = parse_github_remote("http://github.com/owner/repo.git");
        assert_eq!(result, Some(("owner".into(), "repo".into())));
    }

    #[test]
    fn parse_non_github_url_returns_none() {
        assert_eq!(parse_github_remote("git@gitlab.com:owner/repo.git"), None);
        assert_eq!(
            parse_github_remote("https://gitlab.com/owner/repo.git"),
            None
        );
        assert_eq!(
            parse_github_remote("https://bitbucket.org/owner/repo.git"),
            None
        );
    }

    #[test]
    fn parse_malformed_url_returns_none() {
        assert_eq!(parse_github_remote("git@github.com:"), None);
        assert_eq!(parse_github_remote("git@github.com:owner"), None);
        assert_eq!(parse_github_remote("https://github.com/"), None);
        assert_eq!(parse_github_remote("https://github.com/owner"), None);
        assert_eq!(parse_github_remote("not-a-url"), None);
        assert_eq!(parse_github_remote(""), None);
    }
}
