//! Tests for the lookup helpers and public collectors in
//! `super::super::query`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use regex::Regex;

use super::{create_mock_pr, create_mock_repo_data, repo_data_with_review_request, repo_path};
use crate::assembly::query::{
    collect_review_requested_prs, collect_unlinked_prs, extract_issue_number,
};
use crate::work_item::ReviewDecision;

#[test]
fn extract_issue_number_various_patterns() {
    let pat = Regex::new(r"^(\d+)-").unwrap();
    assert_eq!(extract_issue_number("42-fix-bug", &pat), Some(42));
    assert_eq!(extract_issue_number("123-add-feature", &pat), Some(123));
    assert_eq!(extract_issue_number("no-number-here", &pat), None);
    assert_eq!(extract_issue_number("", &pat), None);

    // Pattern with different format.
    let pat2 = Regex::new(r"issue-(\d+)").unwrap();
    assert_eq!(extract_issue_number("issue-55-fix", &pat2), Some(55));
    assert_eq!(extract_issue_number("feature-branch", &pat2), None);
}

#[test]
fn closed_prs_excluded_from_unlinked() {
    let mut closed_pr = create_mock_pr(10, "Closed PR", "closed-branch", "", "");
    closed_pr.state = "CLOSED".to_string();
    let mut merged_pr = create_mock_pr(11, "Merged PR", "merged-branch", "", "");
    merged_pr.state = "MERGED".to_string();
    let open_pr = create_mock_pr(12, "Open PR", "open-branch", "", "");

    let repo_path = PathBuf::from("/repo");
    let mut repo_data = HashMap::new();
    repo_data.insert(
        repo_path.clone(),
        create_mock_repo_data(
            repo_path,
            vec![],
            vec![closed_pr, merged_pr, open_pr],
            vec![],
        )
        .1,
    );

    let claimed = HashSet::new();
    let unlinked = collect_unlinked_prs(&repo_data, &claimed);

    assert_eq!(unlinked.len(), 1, "only the OPEN PR should be unlinked");
    assert_eq!(unlinked[0].pr.title, "Open PR");
}

// -----------------------------------------------------------------------
// collect_review_requested_prs filter tests
// -----------------------------------------------------------------------

#[test]
fn review_requests_hidden_when_approved() {
    let repo_data = repo_data_with_review_request(repo_path("alpha"), "feat-a", "APPROVED");
    let claimed: HashSet<(PathBuf, String)> = HashSet::new();
    let result = collect_review_requested_prs(&repo_data, &claimed);
    assert!(
        result.is_empty(),
        "Approved review requests must not appear in the sidebar, got: {:?}",
        result.iter().map(|r| &r.branch).collect::<Vec<_>>(),
    );
}

#[test]
fn review_requests_hidden_when_changes_requested() {
    let repo_data =
        repo_data_with_review_request(repo_path("alpha"), "feat-a", "CHANGES_REQUESTED");
    let claimed: HashSet<(PathBuf, String)> = HashSet::new();
    let result = collect_review_requested_prs(&repo_data, &claimed);
    assert!(
        result.is_empty(),
        "ChangesRequested review requests must not appear in the sidebar, got: {:?}",
        result.iter().map(|r| &r.branch).collect::<Vec<_>>(),
    );
}

#[test]
fn review_requests_shown_when_pending() {
    let repo_data = repo_data_with_review_request(repo_path("alpha"), "feat-a", "REVIEW_REQUIRED");
    let claimed: HashSet<(PathBuf, String)> = HashSet::new();
    let result = collect_review_requested_prs(&repo_data, &claimed);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].branch, "feat-a");
    assert_eq!(result[0].pr.review_decision, ReviewDecision::Pending);
}

#[test]
fn review_requests_shown_when_no_decision() {
    let repo_data = repo_data_with_review_request(repo_path("alpha"), "feat-a", "");
    let claimed: HashSet<(PathBuf, String)> = HashSet::new();
    let result = collect_review_requested_prs(&repo_data, &claimed);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].branch, "feat-a");
    assert_eq!(result[0].pr.review_decision, ReviewDecision::None);
}

#[test]
fn review_requests_hidden_when_claimed_regardless_of_decision() {
    // Regression guard on the existing claim filter: even a
    // genuinely-actionable Pending review request must be hidden
    // when the branch is already tracked by a work item.
    let rp = repo_path("alpha");
    let repo_data = repo_data_with_review_request(rp.clone(), "feat-a", "REVIEW_REQUIRED");
    let mut claimed: HashSet<(PathBuf, String)> = HashSet::new();
    claimed.insert((rp, "feat-a".to_string()));
    let result = collect_review_requested_prs(&repo_data, &claimed);
    assert!(
        result.is_empty(),
        "Claimed review requests must not appear regardless of decision",
    );
}
