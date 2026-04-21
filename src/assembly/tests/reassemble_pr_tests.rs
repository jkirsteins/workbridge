//! Fork-PR filtering and persisted-`pr_identity` fallback tests.
//!
//! These cover the two PR-matching invariants that are most prone to
//! regress: rejecting fork PRs whose head repo differs from the local
//! repo even when branch names collide, and using a persisted
//! `pr_identity` only when the backend record is Done so a reverted
//! merge cannot lock an item into Done forever.

use std::collections::HashMap;

use super::{
    DEFAULT_ISSUE_PATTERN, create_mock_pr, create_mock_pr_with_owner, create_mock_record,
    create_mock_repo_data, repo_path,
};
use crate::assembly::reassemble;
use crate::work_item::{PrState, WorkItemStatus};
use crate::work_item_backend::RepoAssociationRecord;

// -- Round 7 regression tests --

/// F-1: Fork PRs with the same branch name as a local branch must not
/// be matched. Only same-repo PRs (where `head_repo_owner` matches the
/// repo owner) should match.
#[test]
fn fork_pr_not_matched_to_local_branch() {
    let rp = repo_path("alpha");
    let branch = "fix-typo";

    let record = create_mock_record(
        "wi-1",
        "Fix typo",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // Same-repo PR (owner matches).
    let same_repo_pr = create_mock_pr_with_owner(
        1,
        "Fix typo (same repo)",
        branch,
        "APPROVED",
        "SUCCESS",
        Some("owner"),
    );
    // Fork PR (different owner, same branch name).
    let fork_pr = create_mock_pr_with_owner(
        2,
        "Fix typo (fork)",
        branch,
        "",
        "PENDING",
        Some("contributor"),
    );

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![same_repo_pr, fork_pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, unlinked, _, _) =
        reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let item = &items[0];

    // Only the same-repo PR should be matched.
    let pr_info = item.repo_associations[0]
        .pr
        .as_ref()
        .expect("should have PR info");
    assert_eq!(pr_info.number, 1, "should match same-repo PR, not fork PR");
    assert_eq!(pr_info.title, "Fix typo (same repo)");

    // No MultiplePrsForBranch error since the fork PR is filtered out.
    assert!(
        item.errors.is_empty(),
        "fork PR should be filtered out, no MultiplePrsForBranch: {:?}",
        item.errors,
    );

    // After F-1 fix (Round 10): fork PR whose (repo_path, branch) is
    // already claimed by a work item should NOT re-appear as unlinked.
    // The branch "fix-typo" is claimed by wi-1, so the fork PR is
    // excluded from the unlinked list.
    assert_eq!(
        unlinked.len(),
        0,
        "fork PR with a claimed branch should not appear as unlinked, got {} unlinked",
        unlinked.len(),
    );
}

/// F-1: When `head_repo_owner` is None (gh CLI did not return the field),
/// PRs are still matched (backwards compatibility).
#[test]
fn pr_without_head_repo_owner_still_matches() {
    let rp = repo_path("alpha");
    let branch = "fix-typo";

    let record = create_mock_record(
        "wi-1",
        "Fix typo",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // PR without head_repo_owner (None).
    let pr = create_mock_pr(1, "Fix typo", branch, "", "");

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert!(
        items[0].repo_associations[0].pr.is_some(),
        "PR with no head_repo_owner should still match",
    );
}

// -- Round 8 regression tests --

/// F-2 (Round 8) + F-1 fix (Round 10): Fork PRs appear in `unlinked_prs`
/// only when their (`repo_path`, branch) is NOT claimed by a work item.
/// Once imported, the fork PR's branch is claimed and it disappears
/// from the unlinked list.
#[test]
fn fork_pr_appears_in_unlinked_prs() {
    let rp = repo_path("alpha");

    // Work item claims branch "fix-readme".
    let record = create_mock_record(
        "wi-1",
        "Fix readme",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some("fix-readme".to_string()),
            pr_identity: None,
        }],
    );

    // Same-repo PR on the claimed branch - should NOT be unlinked.
    let same_repo_pr = create_mock_pr_with_owner(
        1,
        "Fix readme (same repo)",
        "fix-readme",
        "APPROVED",
        "SUCCESS",
        Some("owner"),
    );
    // Fork PR on a DIFFERENT branch - should appear as unlinked.
    let fork_pr = create_mock_pr_with_owner(
        2,
        "Fix readme (fork)",
        "fork-fix-readme",
        "",
        "PENDING",
        Some("contributor"),
    );

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![same_repo_pr, fork_pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, unlinked, _, _) =
        reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    // The work item should have the same-repo PR matched.
    assert_eq!(items.len(), 1);
    let pr_info = items[0].repo_associations[0]
        .pr
        .as_ref()
        .expect("should have same-repo PR matched");
    assert_eq!(pr_info.number, 1);

    // The fork PR on a different branch should appear as unlinked.
    assert_eq!(
        unlinked.len(),
        1,
        "fork PR on unclaimed branch should appear in unlinked list, got {}",
        unlinked.len(),
    );
    assert_eq!(unlinked[0].pr.number, 2);
    assert_eq!(unlinked[0].pr.title, "Fix readme (fork)");
    assert_eq!(unlinked[0].branch, "fork-fix-readme");
}

// -- PR identity fallback tests --

/// Persisted `pr_identity` produces a `PrInfo` when the backend record is
/// Done and no live PR is found.
#[test]
fn pr_identity_fallback_produces_pr_info_for_done_item() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    let record = create_mock_record(
        "wi-1",
        "Shipped feature",
        WorkItemStatus::Done,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: Some(crate::work_item_backend::PrIdentityRecord {
                number: 42,
                title: "Ship it".into(),
                url: "https://github.com/o/r/pull/42".into(),
            }),
        }],
    );

    // No live PRs in repo data.
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.status, WorkItemStatus::Done);
    assert!(
        item.status_derived,
        "status_derived should be true when pr_identity fallback injects PrState::Merged",
    );

    let assoc = &item.repo_associations[0];
    let pr = assoc
        .pr
        .as_ref()
        .expect("pr_identity fallback should produce PrInfo");
    assert_eq!(pr.number, 42);
    assert_eq!(pr.title, "Ship it");
    assert_eq!(pr.state, PrState::Merged);
    assert_eq!(pr.url, "https://github.com/o/r/pull/42");
}

/// A live PR takes precedence over persisted `pr_identity`.
#[test]
fn live_pr_takes_precedence_over_pr_identity() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: Some(crate::work_item_backend::PrIdentityRecord {
                number: 42,
                title: "Old merged PR".into(),
                url: "https://github.com/o/r/pull/42".into(),
            }),
        }],
    );

    // A live PR exists on the same branch.
    let live_pr = create_mock_pr(99, "New PR on same branch", branch, "", "PENDING");

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![live_pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let assoc = &items[0].repo_associations[0];
    let pr = assoc.pr.as_ref().expect("should have live PR info");
    assert_eq!(
        pr.number, 99,
        "live PR should take precedence over pr_identity"
    );
    assert_eq!(pr.title, "New PR on same branch");
    assert_eq!(pr.state, PrState::Open);
}

/// Persisted `pr_identity` is ignored when the item is NOT Done.
/// This prevents an irreversible Done lock when a merge is reverted
/// and the user moves the item back.
#[test]
fn pr_identity_ignored_when_item_not_done() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    // Backend record is Implementing (user moved it back after merge revert).
    let record = create_mock_record(
        "wi-1",
        "Reverted merge",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: Some(crate::work_item_backend::PrIdentityRecord {
                number: 42,
                title: "Old merged PR".into(),
                url: "https://github.com/o/r/pull/42".into(),
            }),
        }],
    );

    // No live PRs.
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let item = &items[0];

    // pr_identity should NOT be used because record.status != Done.
    assert!(
        item.repo_associations[0].pr.is_none(),
        "pr_identity should be ignored for non-Done items, got: {:?}",
        item.repo_associations[0].pr,
    );

    // Status should remain Implementing, NOT be derived to Done.
    assert_eq!(
        item.status,
        WorkItemStatus::Implementing,
        "non-Done item with pr_identity should not be forced to Done",
    );
    assert!(
        !item.status_derived,
        "status_derived should be false for non-Done item with pr_identity",
    );
}

/// F-1 regression: After importing a fork PR, reassembling should NOT
/// show it as unlinked again. The import creates a work item that
/// claims the (`repo_path`, branch), so `collect_unlinked_prs` must
/// respect that claim for fork PRs too.
#[test]
fn imported_fork_pr_not_re_listed_as_unlinked() {
    let rp = repo_path("alpha");
    let fork_branch = "fix-typo";

    // Simulate state AFTER importing the fork PR: a work item now
    // claims (repo_path, fork_branch).
    let record = create_mock_record(
        "wi-imported",
        "Fix typo (fork)",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(fork_branch.to_string()),
            pr_identity: None,
        }],
    );

    // The fork PR still exists in the fetched PR list.
    let fork_pr = create_mock_pr_with_owner(
        99,
        "Fix typo (fork)",
        fork_branch,
        "",
        "PENDING",
        Some("contributor"),
    );

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![fork_pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (_items, unlinked, _, _) =
        reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    // The fork PR's branch is now claimed by the imported work item,
    // so it must NOT appear as unlinked.
    assert_eq!(
        unlinked.len(),
        0,
        "imported fork PR should not re-appear as unlinked, got {} unlinked: {:?}",
        unlinked.len(),
        unlinked.iter().map(|u| &u.pr.title).collect::<Vec<_>>(),
    );
}
