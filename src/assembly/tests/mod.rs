//! Tests for the assembly layer.
//!
//! Split into three topic files so each stays well under the 700-line
//! budget: reassembly driver behaviour, pure conversion helpers, and
//! query / collector helpers. Shared fixture builders live here so all
//! three test files can reuse them.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::github_client::{GithubError, GithubIssue, GithubPr};
use crate::work_item::{RepoFetchResult, WorkItemId, WorkItemStatus};
use crate::work_item_backend::{RepoAssociationRecord, WorkItemRecord};
use crate::worktree_service::WorktreeInfo;

mod convert_tests;
mod query_tests;
mod reassemble_basic_tests;
mod reassemble_pr_tests;
mod reassemble_status_tests;

pub(super) const DEFAULT_ISSUE_PATTERN: &str = r"^(\d+)-";

pub(super) fn repo_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/repos/{name}"))
}

pub(super) fn create_mock_record(
    id_suffix: &str,
    title: &str,
    status: WorkItemStatus,
    associations: Vec<RepoAssociationRecord>,
) -> WorkItemRecord {
    WorkItemRecord {
        display_id: None,
        id: WorkItemId::LocalFile(PathBuf::from(format!("/data/{id_suffix}.json"))),
        title: title.to_string(),
        description: None,
        status,
        kind: crate::work_item::WorkItemKind::Own,
        repo_associations: associations,
        plan: None,
        done_at: None,
    }
}

pub(super) fn create_mock_pr(
    number: u64,
    title: &str,
    branch: &str,
    review: &str,
    checks: &str,
) -> GithubPr {
    GithubPr {
        number,
        title: title.to_string(),
        state: "OPEN".to_string(),
        is_draft: false,
        head_branch: branch.to_string(),
        url: format!("https://github.com/o/r/pull/{number}"),
        review_decision: review.to_string(),
        status_check_rollup: checks.to_string(),
        head_repo_owner: None,
        author: Some("testuser".to_string()),
        mergeable: String::new(),
        requested_reviewer_logins: Vec::new(),
        requested_team_slugs: Vec::new(),
    }
}

pub(super) fn create_mock_pr_with_owner(
    number: u64,
    title: &str,
    branch: &str,
    review: &str,
    checks: &str,
    owner: Option<&str>,
) -> GithubPr {
    GithubPr {
        number,
        title: title.to_string(),
        state: "OPEN".to_string(),
        is_draft: false,
        head_branch: branch.to_string(),
        url: format!("https://github.com/o/r/pull/{number}"),
        review_decision: review.to_string(),
        status_check_rollup: checks.to_string(),
        head_repo_owner: owner.map(std::string::ToString::to_string),
        author: Some("testuser".to_string()),
        mergeable: String::new(),
        requested_reviewer_logins: Vec::new(),
        requested_team_slugs: Vec::new(),
    }
}

pub(super) fn create_mock_issue(number: u64, title: &str) -> GithubIssue {
    GithubIssue {
        number,
        title: title.to_string(),
        state: "OPEN".to_string(),
        labels: vec![],
    }
}

pub(super) fn create_mock_worktree(path: &str, branch: Option<&str>) -> WorktreeInfo {
    WorktreeInfo {
        path: PathBuf::from(path),
        branch: branch.map(std::string::ToString::to_string),
        is_main: false,
        has_commits_ahead: None,
        dirty: None,
        untracked: None,
        unpushed: None,
        behind_remote: None,
    }
}

pub(super) fn create_mock_repo_data(
    path: PathBuf,
    worktrees: Vec<WorktreeInfo>,
    prs: Vec<GithubPr>,
    issues: Vec<(u64, Result<GithubIssue, GithubError>)>,
) -> (PathBuf, RepoFetchResult) {
    let fetch = RepoFetchResult {
        repo_path: path.clone(),
        github_remote: Some(("owner".to_string(), "repo".to_string())),
        worktrees: Ok(worktrees),
        prs: Ok(prs),
        review_requested_prs: Ok(vec![]),
        current_user_login: None,
        issues,
    };
    (path, fetch)
}

/// Build a `repo_data` map with a single review-requested PR on the given
/// branch and review decision. Helper for the filter tests.
pub(super) fn repo_data_with_review_request(
    rp: PathBuf,
    branch: &str,
    review_decision: &str,
) -> HashMap<PathBuf, RepoFetchResult> {
    let pr = GithubPr {
        number: 1,
        title: "Needs your review".to_string(),
        state: "OPEN".to_string(),
        is_draft: false,
        head_branch: branch.to_string(),
        url: "https://github.com/o/r/pull/1".to_string(),
        review_decision: review_decision.to_string(),
        status_check_rollup: String::new(),
        head_repo_owner: Some("other".to_string()),
        author: Some("someone-else".to_string()),
        mergeable: String::new(),
        requested_reviewer_logins: Vec::new(),
        requested_team_slugs: Vec::new(),
    };
    let fetch = RepoFetchResult {
        repo_path: rp.clone(),
        github_remote: Some(("owner".to_string(), "repo".to_string())),
        worktrees: Ok(vec![]),
        prs: Ok(vec![]),
        review_requested_prs: Ok(vec![pr]),
        issues: vec![],
        current_user_login: None,
    };
    HashMap::from([(rp, fetch)])
}
