//! Type-conversion helpers that translate raw GitHub fetch results and
//! backend records into the display-ready types consumed by the UI.
//!
//! Every function in this module is a pure projection: same input always
//! produces the same output, no I/O, no mutable state. The `reassemble`
//! driver in `super` composes them with the lookup helpers in
//! [`super::query`] to build `WorkItem`s from `WorkItemRecord`s and
//! per-repo fetch results.

use crate::github_client::{GithubIssue, GithubPr};
use crate::work_item::{
    BackendType, CheckStatus, IssueInfo, MergeableState, PrInfo, PrState, ReviewDecision,
    WorkItemId,
};

/// Convert a raw `GithubPr` into a display-ready `PrInfo`.
pub(super) fn convert_pr(pr: &GithubPr) -> PrInfo {
    PrInfo {
        number: pr.number,
        title: pr.title.clone(),
        state: convert_pr_state(&pr.state),
        is_draft: pr.is_draft,
        review_decision: convert_review_decision(&pr.review_decision),
        checks: convert_check_status(&pr.status_check_rollup),
        mergeable: convert_mergeable_state(&pr.mergeable),
        url: pr.url.clone(),
    }
}

/// Convert a raw state string from GitHub into a `PrState` enum.
pub(super) fn convert_pr_state(raw: &str) -> PrState {
    match raw.to_uppercase().as_str() {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

/// Convert a raw review decision string from GitHub into a `ReviewDecision` enum.
pub(super) fn convert_review_decision(raw: &str) -> ReviewDecision {
    match raw {
        "APPROVED" => ReviewDecision::Approved,
        "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
        "REVIEW_REQUIRED" => ReviewDecision::Pending,
        _ => ReviewDecision::None,
    }
}

/// Convert a raw status check rollup string into a `CheckStatus` enum.
pub(super) fn convert_check_status(raw: &str) -> CheckStatus {
    match raw {
        "SUCCESS" => CheckStatus::Passing,
        "PENDING" => CheckStatus::Pending,
        "FAILURE" => CheckStatus::Failing,
        "" => CheckStatus::None,
        _ => CheckStatus::Unknown,
    }
}

/// Convert a raw mergeable string from GitHub into a `MergeableState` enum.
pub(super) fn convert_mergeable_state(raw: &str) -> MergeableState {
    match raw {
        "MERGEABLE" => MergeableState::Mergeable,
        "CONFLICTING" => MergeableState::Conflicting,
        _ => MergeableState::Unknown,
    }
}

/// Convert a raw `GithubIssue` into a display-ready `IssueInfo`.
pub(super) fn convert_issue(issue: &GithubIssue) -> IssueInfo {
    IssueInfo {
        number: issue.number,
        title: issue.title.clone(),
        labels: issue.labels.clone(),
    }
}

/// Derive a fallback title from backend title or branch name.
///
/// Used by `reassemble` when no PR or issue title is available. The
/// priority is: non-empty backend title > first branch name > the literal
/// `"untitled"`.
pub fn derive_fallback_title(backend_title: &str, first_branch: Option<&String>) -> String {
    if !backend_title.is_empty() {
        backend_title.to_string()
    } else if let Some(branch) = first_branch {
        branch.clone()
    } else {
        "untitled".to_string()
    }
}

/// Derive the `BackendType` from a `WorkItemId`.
pub(super) const fn backend_type_from_id(id: &WorkItemId) -> BackendType {
    match id {
        WorkItemId::LocalFile(_) => BackendType::LocalFile,
        WorkItemId::GithubIssue { .. } => BackendType::GithubIssue,
        WorkItemId::GithubProject { .. } => BackendType::GithubProject,
    }
}
