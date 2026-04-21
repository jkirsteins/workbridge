use std::process::Command;
use std::sync::OnceLock;

use serde_json::Value;

use super::{GithubClient, GithubError, GithubIssue, GithubPr, LivePrState};
use crate::work_item::{CheckStatus, MergeableState};

mod parsers;

use parsers::{parse_check_status_raw, parse_issue_from_value, parse_pr_from_value};

/// `GhCliClient` shells out to the `gh` CLI to interact with the GitHub API.
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
    pub const fn new() -> Self {
        Self {
            current_user_login_cache: OnceLock::new(),
        }
    }

    /// Run a `gh` command and return its stdout on success.
    ///
    /// Returns `GithubError::CliNotFound` if the gh binary is not found,
    /// `GithubError::AuthRequired` if the error output mentions authentication,
    /// and `GithubError::ApiError` for other non-zero exits.
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

    fn fetch_live_merge_state(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<LivePrState, GithubError> {
        let repo_arg = format!("{owner}/{repo}");
        // `--json mergeable,statusCheckRollup,state` gives both
        // signals in a single gh call. `state` is fetched so the
        // "no open PR" fallback can distinguish "branch exists but
        // its PR was closed/merged" from the actual error path.
        let result = self.run_gh(&[
            "pr",
            "view",
            branch,
            "--repo",
            &repo_arg,
            "--json",
            "mergeable,statusCheckRollup,state",
        ]);

        let stdout = match result {
            Ok(s) => s,
            Err(GithubError::ApiError(msg)) => {
                // `gh pr view` exits non-zero with a stderr like
                // "no pull requests found for branch ..." when the
                // branch has no open PR. Surface that as the
                // structural "no PR" sentinel so the merge precheck
                // falls through to the existing `NoPr` outcome
                // instead of blocking the merge on a fetch error.
                let low = msg.to_lowercase();
                if low.contains("no pull requests found")
                    || low.contains("no pull request")
                    || low.contains("not found")
                {
                    return Ok(LivePrState::no_pr());
                }
                return Err(GithubError::ApiError(msg));
            }
            Err(e) => return Err(e),
        };

        let value: Value = serde_json::from_str(stdout.trim())
            .map_err(|e| GithubError::ParseError(format!("failed to parse pr view JSON: {e}")))?;

        let state = value
            .get("state")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_uppercase();
        // `gh pr view <branch>` returns the most recent PR on the
        // branch regardless of state. If it is CLOSED / MERGED, no
        // open PR exists for the branch - treat the same as "no PR".
        if state == "CLOSED" || state == "MERGED" {
            return Ok(LivePrState::no_pr());
        }

        let mergeable_raw = value
            .get("mergeable")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let mergeable = match mergeable_raw {
            "MERGEABLE" => MergeableState::Mergeable,
            "CONFLICTING" => MergeableState::Conflicting,
            _ => MergeableState::Unknown,
        };

        let rollup_raw = parse_check_status_raw(&value);
        let check_rollup = match rollup_raw.as_str() {
            "SUCCESS" => CheckStatus::Passing,
            "PENDING" => CheckStatus::Pending,
            "FAILURE" => CheckStatus::Failing,
            "" => CheckStatus::None,
            _ => CheckStatus::Unknown,
        };

        Ok(LivePrState {
            mergeable,
            check_rollup,
            has_open_pr: true,
        })
    }
}
