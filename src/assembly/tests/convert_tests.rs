//! Tests for the pure projection helpers in `super::super::convert`.

use crate::assembly::convert::{
    convert_check_status, convert_issue, convert_mergeable_state, convert_pr_state,
    convert_review_decision,
};
use crate::github_client::GithubIssue;
use crate::work_item::{CheckStatus, MergeableState, PrState, ReviewDecision};

#[test]
fn convert_pr_state_variants() {
    assert_eq!(convert_pr_state("OPEN"), PrState::Open);
    assert_eq!(convert_pr_state("CLOSED"), PrState::Closed);
    assert_eq!(convert_pr_state("MERGED"), PrState::Merged);
    assert_eq!(convert_pr_state("open"), PrState::Open);
    assert_eq!(convert_pr_state("merged"), PrState::Merged);
    assert_eq!(convert_pr_state(""), PrState::Open);
}

#[test]
fn convert_review_decision_variants() {
    assert_eq!(
        convert_review_decision("APPROVED"),
        ReviewDecision::Approved
    );
    assert_eq!(
        convert_review_decision("CHANGES_REQUESTED"),
        ReviewDecision::ChangesRequested
    );
    assert_eq!(
        convert_review_decision("REVIEW_REQUIRED"),
        ReviewDecision::Pending
    );
    assert_eq!(convert_review_decision(""), ReviewDecision::None);
    assert_eq!(convert_review_decision("UNKNOWN"), ReviewDecision::None);
}

#[test]
fn convert_check_status_variants() {
    assert_eq!(convert_check_status("SUCCESS"), CheckStatus::Passing);
    assert_eq!(convert_check_status("PENDING"), CheckStatus::Pending);
    assert_eq!(convert_check_status("FAILURE"), CheckStatus::Failing);
    assert_eq!(convert_check_status(""), CheckStatus::None);
    assert_eq!(convert_check_status("SOMETHING"), CheckStatus::Unknown);
}

#[test]
fn convert_mergeable_state_variants() {
    assert_eq!(
        convert_mergeable_state("MERGEABLE"),
        MergeableState::Mergeable
    );
    assert_eq!(
        convert_mergeable_state("CONFLICTING"),
        MergeableState::Conflicting
    );
    assert_eq!(convert_mergeable_state("UNKNOWN"), MergeableState::Unknown);
    assert_eq!(convert_mergeable_state(""), MergeableState::Unknown);
}

#[test]
fn convert_issue_preserves_number_title_and_labels() {
    let open_issue = GithubIssue {
        number: 1,
        title: "Open".to_string(),
        state: "OPEN".to_string(),
        labels: vec!["bug".to_string()],
    };
    let info = convert_issue(&open_issue);
    assert_eq!(info.number, 1);
    assert_eq!(info.title, "Open");
    assert_eq!(info.labels, vec!["bug"]);

    let closed_issue = GithubIssue {
        number: 2,
        title: "Closed".to_string(),
        state: "CLOSED".to_string(),
        labels: vec![],
    };
    let info = convert_issue(&closed_issue);
    assert_eq!(info.number, 2);
    assert_eq!(info.title, "Closed");
}
