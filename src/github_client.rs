use std::fmt;
use std::process::Command;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
            GithubError::CliNotFound => write!(f, "gh CLI not found"),
            GithubError::AuthRequired => write!(f, "gh auth required"),
            GithubError::ApiError(msg) => write!(f, "GitHub API error: {msg}"),
            GithubError::ParseError(msg) => write!(f, "GitHub parse error: {msg}"),
        }
    }
}

/// A raw PR as returned by the GitHub API (via gh CLI).
/// Includes head_branch for matching against work item repo associations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GithubPr {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub is_draft: bool,
    pub head_branch: String,
    pub url: String,
    /// Raw review decision string from GitHub (e.g. "APPROVED",
    /// "CHANGES_REQUESTED", "REVIEW_REQUIRED", or empty).
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

/// A raw issue as returned by the GitHub API (via gh CLI).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GithubIssue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub labels: Vec<String>,
}

/// Trait for interacting with GitHub. Implementations include GhCliClient
/// (shells out to `gh`) and MockGithubClient (returns fixture data for
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
}

/// Mock GitHub client for tests. Returns configurable fixture data.
#[cfg(test)]
pub struct MockGithubClient {
    pub prs: Vec<GithubPr>,
    pub review_requested_prs: Vec<GithubPr>,
    pub issues: Vec<GithubIssue>,
    /// If set, all calls return this error instead of fixture data.
    pub error: Option<GithubError>,
}

#[cfg(test)]
impl MockGithubClient {
    pub fn new() -> Self {
        Self {
            prs: Vec::new(),
            review_requested_prs: Vec::new(),
            issues: Vec::new(),
            error: None,
        }
    }
}

#[cfg(test)]
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

    /// Mock override. Returns the shared fixture error when `error`
    /// is set (so tests can exercise the "lookup failed" branch of
    /// the fetcher), otherwise returns a stable mock login so every
    /// test that does not care about identity still gets a usable
    /// value and the fetcher does not emit a spurious FetcherError.
    fn current_user_login(&self) -> Result<String, GithubError> {
        if let Some(ref err) = self.error {
            return Err(err.clone());
        }
        Ok("mock-user".into())
    }
}

/// GhCliClient shells out to the `gh` CLI to interact with the GitHub API.
///
/// Holds a single cached value - the authenticated user's login - resolved
/// lazily on first call to `current_user_login()` via `gh api user`. Every
/// other piece of state lives in `gh` itself (authentication, hosts, etc.).
pub struct GhCliClient {
    /// Cached `login` field from `gh api user`. Populated on the first
    /// successful `current_user_login()` call and reused thereafter so
    /// repeated fetch cycles do not re-shell for a value that does not
    /// change during a session. If the first call fails (no network,
    /// auth expired), the cache stays empty and the next call retries.
    current_user_login_cache: OnceLock<String>,
}

impl Default for GhCliClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GhCliClient {
    pub fn new() -> Self {
        Self {
            current_user_login_cache: OnceLock::new(),
        }
    }

    /// Run a `gh` command and return its stdout on success.
    ///
    /// Returns GithubError::CliNotFound if the gh binary is not found,
    /// GithubError::AuthRequired if the error output mentions authentication,
    /// and GithubError::ApiError for other non-zero exits.
    fn run_gh(&self, args: &[&str]) -> Result<String, GithubError> {
        let output = Command::new("gh").args(args).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GithubError::CliNotFound
            } else {
                GithubError::ApiError(format!("failed to run gh: {e}"))
            }
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if stderr.contains("auth") || stderr.contains("login") {
                return Err(GithubError::AuthRequired);
            }
            return Err(GithubError::ApiError(stderr));
        }

        String::from_utf8(output.stdout)
            .map_err(|e| GithubError::ParseError(format!("invalid UTF-8 in gh output: {e}")))
    }
}

impl GithubClient for GhCliClient {
    fn list_open_prs(&self, owner: &str, repo: &str) -> Result<Vec<GithubPr>, GithubError> {
        let repo_arg = format!("{owner}/{repo}");
        let json_fields = "number,title,headRefName,state,isDraft,reviewDecision,statusCheckRollup,url,headRepositoryOwner,author,mergeable";
        let stdout = self.run_gh(&[
            "pr",
            "list",
            "--repo",
            &repo_arg,
            "--state",
            "open",
            "--author",
            "@me",
            "--json",
            json_fields,
            "--limit",
            "500",
        ])?;

        let items: Vec<Value> = serde_json::from_str(&stdout)
            .map_err(|e| GithubError::ParseError(format!("failed to parse PR list JSON: {e}")))?;

        items.iter().map(parse_pr_from_value).collect()
    }

    fn list_review_requested_prs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<GithubPr>, GithubError> {
        let repo_arg = format!("{owner}/{repo}");
        // `reviewRequests` expands to the list of pending reviewer
        // identities (users and teams) attached to each PR; the parser
        // splits the mixed array into `requested_reviewer_logins` and
        // `requested_team_slugs` on the returned GithubPr. `mergeable`
        // is also requested so the review-request row can show the
        // same conflict indicator as the user's own PRs.
        let json_fields = "number,title,headRefName,state,isDraft,reviewDecision,statusCheckRollup,url,headRepositoryOwner,author,mergeable,reviewRequests";
        let stdout = self.run_gh(&[
            "pr",
            "list",
            "--repo",
            &repo_arg,
            "--state",
            "open",
            "--search",
            "review-requested:@me",
            "--json",
            json_fields,
            "--limit",
            "500",
        ])?;

        let items: Vec<Value> = serde_json::from_str(&stdout)
            .map_err(|e| GithubError::ParseError(format!("failed to parse PR list JSON: {e}")))?;

        items.iter().map(parse_pr_from_value).collect()
    }

    fn current_user_login(&self) -> Result<String, GithubError> {
        if let Some(cached) = self.current_user_login_cache.get() {
            return Ok(cached.clone());
        }
        let stdout = self.run_gh(&["api", "user", "--jq", ".login"])?;
        let login = stdout.trim().to_string();
        if login.is_empty() {
            return Err(GithubError::ParseError(
                "gh api user returned an empty login".into(),
            ));
        }
        // If two threads race to initialize, only one `set` wins; the
        // loser silently ignores its attempt and both return the same
        // (winning) value on the next read. Either way the caller gets
        // the correct string this call.
        let _ = self.current_user_login_cache.set(login.clone());
        Ok(login)
    }

    fn get_issue(&self, owner: &str, repo: &str, number: u64) -> Result<GithubIssue, GithubError> {
        let repo_arg = format!("{owner}/{repo}");
        let number_str = number.to_string();
        let stdout = self.run_gh(&[
            "issue",
            "view",
            &number_str,
            "--repo",
            &repo_arg,
            "--json",
            "number,title,state,labels",
        ])?;

        let value: Value = serde_json::from_str(&stdout)
            .map_err(|e| GithubError::ParseError(format!("failed to parse issue JSON: {e}")))?;

        parse_issue_from_value(&value)
    }

    fn list_merged_prs(&self, owner: &str, repo: &str) -> Result<Vec<GithubPr>, GithubError> {
        let repo_arg = format!("{owner}/{repo}");
        let json_fields = "number,title,headRefName,state,isDraft,reviewDecision,statusCheckRollup,url,headRepositoryOwner";
        let stdout = self.run_gh(&[
            "pr",
            "list",
            "--repo",
            &repo_arg,
            "--state",
            "merged",
            "--author",
            "@me",
            "--json",
            json_fields,
            "--limit",
            "500",
        ])?;

        let items: Vec<Value> = serde_json::from_str(&stdout)
            .map_err(|e| GithubError::ParseError(format!("failed to parse PR list JSON: {e}")))?;

        items.iter().map(parse_pr_from_value).collect()
    }
}

// ---------------------------------------------------------------------------
// JSON parsing helpers
// ---------------------------------------------------------------------------

/// Parse a single PR JSON object (from gh pr list --json) into a GithubPr.
fn parse_pr_from_value(v: &Value) -> Result<GithubPr, GithubError> {
    let number = v
        .get("number")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| GithubError::ParseError("PR missing 'number' field".into()))?;

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    let head_branch = v
        .get("headRefName")
        .and_then(|h| h.as_str())
        .unwrap_or("")
        .to_string();

    let state = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("OPEN")
        .to_string();

    let is_draft = v.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false);

    let url = v
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();

    let review_decision = parse_review_decision_raw(v);
    let status_check_rollup = parse_check_status_raw(v);

    // headRepositoryOwner is an object with a "login" field, e.g.
    // {"login": "contributor"}. It may be null or absent.
    let head_repo_owner = v
        .get("headRepositoryOwner")
        .and_then(|o| o.get("login"))
        .and_then(|l| l.as_str())
        .map(|s| s.to_string());

    // author is an object with a "login" field, e.g. {"login": "user"}.
    let author = v
        .get("author")
        .and_then(|o| o.get("login"))
        .and_then(|l| l.as_str())
        .map(|s| s.to_string());

    let mergeable = v
        .get("mergeable")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    // reviewRequests is a mixed array of user and team objects. gh
    // returns user entries with a "login" field and team entries with
    // a "slug" field (plus "__typename" on newer gh versions). We
    // classify defensively - presence of a login makes it a user,
    // presence of a slug without a login makes it a team - so the
    // parser tolerates both current and future gh JSON shapes. When
    // the field is absent (open-PR fetch path) both vecs end up empty.
    let (requested_reviewer_logins, requested_team_slugs) = parse_review_requests(v);

    Ok(GithubPr {
        number,
        title,
        state,
        is_draft,
        head_branch,
        url,
        review_decision,
        status_check_rollup,
        head_repo_owner,
        author,
        mergeable,
        requested_reviewer_logins,
        requested_team_slugs,
    })
}

/// Split the `reviewRequests` JSON array from `gh pr list --json` into
/// user-login and team-slug vecs. See the comment in
/// `parse_pr_from_value` for the classification rules.
fn parse_review_requests(v: &Value) -> (Vec<String>, Vec<String>) {
    let mut logins = Vec::new();
    let mut slugs = Vec::new();
    let Some(arr) = v.get("reviewRequests").and_then(|r| r.as_array()) else {
        return (logins, slugs);
    };
    for entry in arr {
        if let Some(login) = entry.get("login").and_then(|l| l.as_str()) {
            logins.push(login.to_string());
        } else if let Some(slug) = entry.get("slug").and_then(|s| s.as_str()) {
            slugs.push(slug.to_string());
        } else if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
            // Some gh versions expose team identity under "name"
            // instead of "slug". Fall through to capture the team
            // name so the badge still renders something meaningful.
            slugs.push(name.to_string());
        }
    }
    (logins, slugs)
}

/// Parse a single issue JSON object (from gh issue view --json) into a GithubIssue.
fn parse_issue_from_value(v: &Value) -> Result<GithubIssue, GithubError> {
    let number = v
        .get("number")
        .and_then(|n| n.as_u64())
        .ok_or_else(|| GithubError::ParseError("issue missing 'number' field".into()))?;

    let title = v
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    let state = v
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("OPEN")
        .to_string();

    let labels = v
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|label| {
                    // gh returns labels as objects with a "name" field
                    label
                        .get("name")
                        .and_then(|n| n.as_str())
                        .or_else(|| label.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(GithubIssue {
        number,
        title,
        state,
        labels,
    })
}

/// Summarize the statusCheckRollup array from gh into a single raw string.
///
/// The gh CLI returns statusCheckRollup as an array of objects, each with a
/// "status" or "conclusion" field. This function reduces that array to a
/// single summary string:
/// - If the array is empty or missing: ""
/// - If any check has conclusion "FAILURE": "FAILURE"
/// - If any check has status "PENDING" or "IN_PROGRESS" (and none failed): "PENDING"
/// - If all checks have conclusion "SUCCESS": "SUCCESS"
/// - Otherwise: "UNKNOWN"
fn parse_check_status_raw(v: &Value) -> String {
    let checks = match v.get("statusCheckRollup").and_then(|s| s.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return String::new(),
    };

    let mut has_pending = false;
    let mut has_failure = false;
    let mut has_success = false;

    for check in checks {
        // gh returns each check with either "conclusion" (completed checks)
        // or "status" (in-progress checks). conclusion can be null for
        // in-progress checks.
        let conclusion = check
            .get("conclusion")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let status = check.get("status").and_then(|s| s.as_str()).unwrap_or("");

        match conclusion {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => has_success = true,
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            | "STALE" => has_failure = true,
            _ => {
                // No conclusion yet - check the status field
                match status {
                    "COMPLETED" => has_success = true,
                    "IN_PROGRESS" | "QUEUED" | "PENDING" | "WAITING" | "REQUESTED" => {
                        has_pending = true
                    }
                    _ => has_pending = true,
                }
            }
        }
    }

    if has_failure {
        "FAILURE".to_string()
    } else if has_pending {
        "PENDING".to_string()
    } else if has_success {
        "SUCCESS".to_string()
    } else {
        "UNKNOWN".to_string()
    }
}

/// Extract the reviewDecision string from gh JSON output.
///
/// gh returns reviewDecision as a string ("APPROVED", "CHANGES_REQUESTED",
/// "REVIEW_REQUIRED") or an empty string / null if no review has happened.
fn parse_review_decision_raw(v: &Value) -> String {
    v.get("reviewDecision")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string()
}

/// Extract owner and repo name from a GitHub remote URL.
///
/// Supports both SSH (git@github.com:owner/repo.git) and HTTPS
/// (https://github.com/owner/repo.git) formats. Returns None for
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

    // -----------------------------------------------------------------------
    // GhCliClient JSON parsing fixture tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_pr_all_fields_populated() {
        let json = r#"{
            "number": 14,
            "title": "Refactor backend",
            "headRefName": "refactor-backend",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/14",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "ci"},
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.number, 14);
        assert_eq!(pr.title, "Refactor backend");
        assert_eq!(pr.head_branch, "refactor-backend");
        assert_eq!(pr.state, "OPEN");
        assert!(!pr.is_draft);
        assert_eq!(pr.url, "https://github.com/owner/repo/pull/14");
        assert_eq!(pr.review_decision, "APPROVED");
        assert_eq!(pr.status_check_rollup, "SUCCESS");
        // No headRepositoryOwner in JSON -> None
        assert_eq!(pr.head_repo_owner, None);
    }

    #[test]
    fn parse_pr_head_repository_owner() {
        let json = r#"{
            "number": 30,
            "title": "Fork PR",
            "headRefName": "fix-typo",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/30",
            "reviewDecision": "",
            "statusCheckRollup": [],
            "headRepositoryOwner": {"login": "contributor"}
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.head_repo_owner, Some("contributor".to_string()));

        // Null headRepositoryOwner -> None
        let json_null = r#"{
            "number": 31,
            "title": "PR with null owner",
            "headRefName": "feature",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/31",
            "headRepositoryOwner": null
        }"#;
        let v2: Value = serde_json::from_str(json_null).unwrap();
        let pr2 = parse_pr_from_value(&v2).unwrap();
        assert_eq!(pr2.head_repo_owner, None);
    }

    #[test]
    fn parse_pr_empty_check_rollup() {
        let json = r#"{
            "number": 5,
            "title": "Quick fix",
            "headRefName": "quick-fix",
            "state": "OPEN",
            "isDraft": true,
            "url": "https://github.com/owner/repo/pull/5",
            "reviewDecision": "",
            "statusCheckRollup": []
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.number, 5);
        assert!(pr.is_draft);
        assert_eq!(pr.review_decision, "");
        assert_eq!(pr.status_check_rollup, "");
    }

    #[test]
    fn parse_pr_null_check_rollup() {
        let json = r#"{
            "number": 6,
            "title": "No checks",
            "headRefName": "no-checks",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/6",
            "reviewDecision": null
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.status_check_rollup, "");
        assert_eq!(pr.review_decision, "");
    }

    #[test]
    fn parse_pr_mixed_check_statuses_failure_wins() {
        let json = r#"{
            "number": 88,
            "title": "Fix auth",
            "headRefName": "112-fix-auth",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/88",
            "reviewDecision": "CHANGES_REQUESTED",
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"},
                {"status": "COMPLETED", "conclusion": "FAILURE", "name": "test"},
                {"status": "IN_PROGRESS", "conclusion": "", "name": "deploy"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.status_check_rollup, "FAILURE");
        assert_eq!(pr.review_decision, "CHANGES_REQUESTED");
    }

    #[test]
    fn parse_pr_mixed_check_statuses_pending_without_failure() {
        let json = r#"{
            "number": 90,
            "title": "WIP feature",
            "headRefName": "wip-feature",
            "state": "OPEN",
            "isDraft": true,
            "url": "https://github.com/owner/repo/pull/90",
            "reviewDecision": "REVIEW_REQUIRED",
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS", "name": "lint"},
                {"status": "IN_PROGRESS", "conclusion": "", "name": "test"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();

        assert_eq!(pr.status_check_rollup, "PENDING");
    }

    #[test]
    fn parse_pr_closed_and_merged_states() {
        let closed_json = r#"{
            "number": 100,
            "title": "Closed PR",
            "headRefName": "closed-branch",
            "state": "CLOSED",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/100",
            "reviewDecision": "",
            "statusCheckRollup": []
        }"#;

        let merged_json = r#"{
            "number": 101,
            "title": "Merged PR",
            "headRefName": "merged-branch",
            "state": "MERGED",
            "isDraft": false,
            "url": "https://github.com/owner/repo/pull/101",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": []
        }"#;

        let v_closed: Value = serde_json::from_str(closed_json).unwrap();
        let pr_closed = parse_pr_from_value(&v_closed).unwrap();
        assert_eq!(pr_closed.state, "CLOSED");

        let v_merged: Value = serde_json::from_str(merged_json).unwrap();
        let pr_merged = parse_pr_from_value(&v_merged).unwrap();
        assert_eq!(pr_merged.state, "MERGED");
    }

    #[test]
    fn parse_issue_all_fields() {
        let json = r#"{
            "number": 7,
            "title": "Add authentication",
            "state": "OPEN",
            "labels": [
                {"name": "enhancement"},
                {"name": "security"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let issue = parse_issue_from_value(&v).unwrap();

        assert_eq!(issue.number, 7);
        assert_eq!(issue.title, "Add authentication");
        assert_eq!(issue.state, "OPEN");
        assert_eq!(issue.labels, vec!["enhancement", "security"]);
    }

    #[test]
    fn parse_issue_closed_state() {
        let json = r#"{
            "number": 12,
            "title": "Fixed bug",
            "state": "CLOSED",
            "labels": []
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let issue = parse_issue_from_value(&v).unwrap();

        assert_eq!(issue.state, "CLOSED");
        assert!(issue.labels.is_empty());
    }

    #[test]
    fn parse_issue_no_labels_field() {
        let json = r#"{
            "number": 15,
            "title": "No labels",
            "state": "OPEN"
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        let issue = parse_issue_from_value(&v).unwrap();

        assert!(issue.labels.is_empty());
    }

    #[test]
    fn parse_review_decision_variants() {
        let cases = vec![
            (r#"{"reviewDecision": "APPROVED"}"#, "APPROVED"),
            (
                r#"{"reviewDecision": "CHANGES_REQUESTED"}"#,
                "CHANGES_REQUESTED",
            ),
            (
                r#"{"reviewDecision": "REVIEW_REQUIRED"}"#,
                "REVIEW_REQUIRED",
            ),
            (r#"{"reviewDecision": ""}"#, ""),
            (r#"{"reviewDecision": null}"#, ""),
            (r#"{}"#, ""),
        ];

        for (json, expected) in cases {
            let v: Value = serde_json::from_str(json).unwrap();
            assert_eq!(
                parse_review_decision_raw(&v),
                expected,
                "failed for input: {json}"
            );
        }
    }

    #[test]
    fn parse_check_status_all_success() {
        let json = r#"{
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS"},
                {"status": "COMPLETED", "conclusion": "NEUTRAL"},
                {"status": "COMPLETED", "conclusion": "SKIPPED"}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        assert_eq!(parse_check_status_raw(&v), "SUCCESS");
    }

    #[test]
    fn parse_check_status_queued_is_pending() {
        let json = r#"{
            "statusCheckRollup": [
                {"status": "QUEUED", "conclusion": ""}
            ]
        }"#;

        let v: Value = serde_json::from_str(json).unwrap();
        assert_eq!(parse_check_status_raw(&v), "PENDING");
    }

    #[test]
    fn parse_pr_list_json_array() {
        // Simulate a full gh pr list --json response
        let json = r#"[
            {
                "number": 1,
                "title": "First PR",
                "headRefName": "feature-a",
                "state": "OPEN",
                "isDraft": false,
                "url": "https://github.com/o/r/pull/1",
                "reviewDecision": "APPROVED",
                "statusCheckRollup": [
                    {"status": "COMPLETED", "conclusion": "SUCCESS"}
                ]
            },
            {
                "number": 2,
                "title": "Second PR",
                "headRefName": "feature-b",
                "state": "OPEN",
                "isDraft": true,
                "url": "https://github.com/o/r/pull/2",
                "reviewDecision": "",
                "statusCheckRollup": []
            }
        ]"#;

        let items: Vec<Value> = serde_json::from_str(json).unwrap();
        let prs: Vec<GithubPr> = items
            .iter()
            .map(|v| parse_pr_from_value(v).unwrap())
            .collect();

        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[0].status_check_rollup, "SUCCESS");
        assert_eq!(prs[1].number, 2);
        assert!(prs[1].is_draft);
        assert_eq!(prs[1].status_check_rollup, "");
    }

    // -----------------------------------------------------------------------
    // reviewRequests parsing (user vs team split)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_pr_review_requests_absent_empty_vecs() {
        let json = r#"{
            "number": 1,
            "title": "No review data",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/1"
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert!(pr.requested_reviewer_logins.is_empty());
        assert!(pr.requested_team_slugs.is_empty());
    }

    #[test]
    fn parse_pr_review_requests_user_entries() {
        let json = r#"{
            "number": 2,
            "title": "Two users requested",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/2",
            "reviewRequests": [
                {"__typename": "User", "login": "alice"},
                {"__typename": "User", "login": "bob"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert_eq!(pr.requested_reviewer_logins, vec!["alice", "bob"]);
        assert!(pr.requested_team_slugs.is_empty());
    }

    #[test]
    fn parse_pr_review_requests_team_slug() {
        let json = r#"{
            "number": 3,
            "title": "Team requested",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/3",
            "reviewRequests": [
                {"__typename": "Team", "slug": "core-team"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert!(pr.requested_reviewer_logins.is_empty());
        assert_eq!(pr.requested_team_slugs, vec!["core-team"]);
    }

    #[test]
    fn parse_pr_review_requests_team_name_fallback() {
        // Some gh versions may expose the team under "name" instead of
        // "slug". The parser should still capture it.
        let json = r#"{
            "number": 4,
            "title": "Team requested (name)",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/4",
            "reviewRequests": [
                {"name": "backend-team"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert_eq!(pr.requested_team_slugs, vec!["backend-team"]);
    }

    #[test]
    fn parse_pr_review_requests_mixed_users_and_teams() {
        let json = r#"{
            "number": 5,
            "title": "Mixed",
            "headRefName": "x",
            "state": "OPEN",
            "isDraft": false,
            "url": "https://example.com/5",
            "reviewRequests": [
                {"__typename": "User", "login": "alice"},
                {"__typename": "Team", "slug": "core-team"},
                {"__typename": "User", "login": "bob"},
                {"__typename": "Team", "slug": "frontend"}
            ]
        }"#;
        let v: Value = serde_json::from_str(json).unwrap();
        let pr = parse_pr_from_value(&v).unwrap();
        assert_eq!(pr.requested_reviewer_logins, vec!["alice", "bob"]);
        assert_eq!(pr.requested_team_slugs, vec!["core-team", "frontend"]);
    }
}
