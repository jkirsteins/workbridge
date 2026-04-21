//! Basic assembly-driver tests: end-to-end shape of a produced
//! `WorkItem`, title derivation priority, per-repo matching, and the
//! low-level "branch=None" / detached-worktree edge cases.

use std::collections::HashMap;
use std::path::PathBuf;

use super::{
    DEFAULT_ISSUE_PATTERN, create_mock_issue, create_mock_pr, create_mock_record,
    create_mock_repo_data, create_mock_worktree, repo_path,
};
use crate::assembly::reassemble;
use crate::work_item::{
    CheckStatus, PrState, RepoFetchResult, ReviewDecision, WorkItemError, WorkItemStatus,
};
use crate::work_item_backend::RepoAssociationRecord;

#[test]
fn assembles_basic_work_item() {
    let rp = repo_path("alpha");
    let branch = "42-fix-bug";

    let record = create_mock_record(
        "wi-1",
        "Backend title",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    let pr = create_mock_pr(10, "Fix the bug", branch, "APPROVED", "SUCCESS");
    let issue = create_mock_issue(42, "Bug report");
    let wt = create_mock_worktree("/worktrees/42-fix-bug", Some(branch));

    let (rp_key, fetch) =
        create_mock_repo_data(rp.clone(), vec![wt], vec![pr], vec![(42, Ok(issue))]);
    let repo_data: HashMap<PathBuf, RepoFetchResult> = HashMap::from([(rp_key, fetch)]);

    let (items, unlinked, _, _) =
        reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert_eq!(unlinked.len(), 0);

    let item = &items[0];
    // PR title wins over issue title and backend title.
    assert_eq!(item.title, "Fix the bug");
    assert_eq!(item.status, WorkItemStatus::Implementing);
    assert!(item.errors.is_empty());

    let assoc = &item.repo_associations[0];
    assert_eq!(assoc.repo_path, rp);
    assert_eq!(assoc.branch, Some(branch.to_string()));
    assert_eq!(
        assoc.worktree_path,
        Some(PathBuf::from("/worktrees/42-fix-bug"))
    );

    let pr_info = assoc.pr.as_ref().expect("should have PR info");
    assert_eq!(pr_info.number, 10);
    assert_eq!(pr_info.title, "Fix the bug");
    assert_eq!(pr_info.state, PrState::Open);
    assert_eq!(pr_info.review_decision, ReviewDecision::Approved);
    assert_eq!(pr_info.checks, CheckStatus::Passing);

    let issue_info = assoc.issue.as_ref().expect("should have issue info");
    assert_eq!(issue_info.number, 42);
    assert_eq!(issue_info.title, "Bug report");
}

#[test]
fn title_derivation_priority() {
    let rp = repo_path("alpha");
    let branch = "42-fix-bug";

    // Level 1: PR title wins
    {
        let record = create_mock_record(
            "wi-1",
            "Backend title",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );
        let pr = create_mock_pr(10, "PR title", branch, "", "");
        let issue = create_mock_issue(42, "Issue title");
        let (rp_key, fetch) =
            create_mock_repo_data(rp.clone(), vec![], vec![pr], vec![(42, Ok(issue))]);
        let repo_data = HashMap::from([(rp_key, fetch)]);
        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
        assert_eq!(items[0].title, "PR title");
    }

    // Level 2: Issue title (no PR)
    {
        let record = create_mock_record(
            "wi-2",
            "Backend title",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );
        let issue = create_mock_issue(42, "Issue title");
        let (rp_key, fetch) =
            create_mock_repo_data(rp.clone(), vec![], vec![], vec![(42, Ok(issue))]);
        let repo_data = HashMap::from([(rp_key, fetch)]);
        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
        assert_eq!(items[0].title, "Issue title");
    }

    // Level 3: Backend title (no PR, no issue)
    {
        let record = create_mock_record(
            "wi-3",
            "Backend title",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);
        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
        assert_eq!(items[0].title, "Backend title");
    }

    // Level 4: Branch name (no PR, no issue, empty backend title)
    {
        let record = create_mock_record(
            "wi-4",
            "",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some("my-feature".to_string()),
                pr_identity: None,
            }],
        );
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);
        // Use a pattern that won't match "my-feature"
        let (items, _, _, _) = reassemble(&[record], &repo_data, r"^(\d+)-", ".worktrees");
        assert_eq!(items[0].title, "my-feature");
    }

    // Level 5: "untitled" (no PR, no issue, empty backend title, no branch)
    {
        let record = create_mock_record(
            "wi-5",
            "",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp,
                branch: None,
                pr_identity: None,
            }],
        );
        let repo_data = HashMap::new();
        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
        assert_eq!(items[0].title, "untitled");
    }
}

#[test]
fn unlinked_pr_detection() {
    let rp = repo_path("alpha");

    // Work item claims branch "feature-a"
    let record = create_mock_record(
        "wi-1",
        "My work",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some("feature-a".to_string()),
            pr_identity: None,
        }],
    );

    // Repo has PRs for "feature-a" (claimed) and "feature-b" (unlinked)
    let pr_a = create_mock_pr(1, "PR A", "feature-a", "", "");
    let pr_b = create_mock_pr(2, "PR B", "feature-b", "APPROVED", "SUCCESS");

    let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![pr_a, pr_b], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, unlinked, _, _) =
        reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert_eq!(unlinked.len(), 1);
    assert_eq!(unlinked[0].branch, "feature-b");
    assert_eq!(unlinked[0].pr.number, 2);
    assert_eq!(unlinked[0].pr.title, "PR B");
    assert_eq!(unlinked[0].repo_path, rp);
}

#[test]
fn multi_repo_work_item() {
    let rp_a = repo_path("alpha");
    let rp_b = repo_path("beta");

    let record = create_mock_record(
        "wi-1",
        "Cross-repo work",
        WorkItemStatus::Implementing,
        vec![
            RepoAssociationRecord {
                repo_path: rp_a.clone(),
                branch: Some("feature-x".to_string()),
                pr_identity: None,
            },
            RepoAssociationRecord {
                repo_path: rp_b.clone(),
                branch: Some("feature-x".to_string()),
                pr_identity: None,
            },
        ],
    );

    let pr_a = create_mock_pr(10, "PR in alpha", "feature-x", "APPROVED", "SUCCESS");
    let pr_b = create_mock_pr(20, "PR in beta", "feature-x", "", "PENDING");

    let (key_a, fetch_a) = create_mock_repo_data(rp_a.clone(), vec![], vec![pr_a], vec![]);
    let (key_b, fetch_b) = create_mock_repo_data(rp_b.clone(), vec![], vec![pr_b], vec![]);
    let repo_data = HashMap::from([(key_a, fetch_a), (key_b, fetch_b)]);

    let (items, unlinked, _, _) =
        reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert_eq!(unlinked.len(), 0);

    let item = &items[0];
    assert_eq!(item.repo_associations.len(), 2);

    // First association's PR title becomes the title.
    assert_eq!(item.title, "PR in alpha");

    // Each association has its own PR info.
    let assoc_a = item
        .repo_associations
        .iter()
        .find(|a| a.repo_path == rp_a)
        .expect("should have alpha association");
    assert_eq!(assoc_a.pr.as_ref().unwrap().number, 10);
    assert_eq!(
        assoc_a.pr.as_ref().unwrap().review_decision,
        ReviewDecision::Approved
    );

    let assoc_b = item
        .repo_associations
        .iter()
        .find(|a| a.repo_path == rp_b)
        .expect("should have beta association");
    assert_eq!(assoc_b.pr.as_ref().unwrap().number, 20);
    assert_eq!(assoc_b.pr.as_ref().unwrap().checks, CheckStatus::Pending);
}

#[test]
fn branch_none_skips_matching() {
    let rp = repo_path("alpha");

    let record = create_mock_record(
        "wi-1",
        "Planning item",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: None,
            pr_identity: None,
        }],
    );

    // Repo has a PR and worktree, but none should match since branch is None.
    let pr = create_mock_pr(1, "Some PR", "some-branch", "", "");
    let wt = create_mock_worktree("/worktrees/some-branch", Some("some-branch"));

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![wt], vec![pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, unlinked, _, _) =
        reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let assoc = &items[0].repo_associations[0];
    assert_eq!(assoc.branch, None);
    assert_eq!(assoc.worktree_path, None);
    assert!(assoc.pr.is_none());
    assert!(assoc.issue.is_none());
    assert!(assoc.git_state.is_none());

    // The PR on "some-branch" should be unlinked since no work item claims it.
    assert_eq!(unlinked.len(), 1);
    assert_eq!(unlinked[0].branch, "some-branch");
}

#[test]
fn multiple_prs_for_branch() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // Two PRs on the same branch.
    let pr1 = create_mock_pr(1, "PR one", branch, "", "");
    let pr2 = create_mock_pr(2, "PR two", branch, "", "");

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![pr1, pr2], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let item = &items[0];

    // Should have a MultiplePrsForBranch error.
    assert!(
        item.errors.iter().any(|e| matches!(
            e,
            WorkItemError::MultiplePrsForBranch {
                branch: b,
                count: 2,
                ..
            } if b == "feature-x"
        )),
        "expected MultiplePrsForBranch error, got: {:?}",
        item.errors
    );

    // Should still fill the first PR.
    assert!(item.repo_associations[0].pr.is_some());
    assert_eq!(item.repo_associations[0].pr.as_ref().unwrap().number, 1);
}

/// A detached worktree (branch=None) does not match any work item and
/// is not associated with any work item. This is the correct behavior
/// since there is no branch to match on.
#[test]
fn detached_worktree_not_associated_with_work_item() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // Only a detached worktree exists - no worktree on feature-x.
    let wt_detached = create_mock_worktree("/worktrees/some-detached", None);

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![wt_detached], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let item = &items[0];

    // The detached worktree should not be associated with the work item.
    assert!(
        item.repo_associations[0].worktree_path.is_none(),
        "detached worktree should not match work item, got: {:?}",
        item.repo_associations[0].worktree_path,
    );
    assert!(
        item.repo_associations[0].git_state.is_none(),
        "git_state should be None when no worktree matches",
    );
    // No errors should be produced.
    assert!(
        item.errors.is_empty(),
        "no errors expected for detached worktrees, got: {:?}",
        item.errors,
    );
}

#[test]
fn empty_inputs() {
    let repo_data: HashMap<PathBuf, RepoFetchResult> = HashMap::new();
    let (items, unlinked, _, _) = reassemble(&[], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
    assert!(items.is_empty());
    assert!(unlinked.is_empty());
}

#[test]
fn worktree_not_found_leaves_path_none() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // Repo data has a worktree on a different branch.
    let wt = create_mock_worktree("/worktrees/other-branch", Some("other-branch"));

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![wt], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let assoc = &items[0].repo_associations[0];
    assert_eq!(assoc.worktree_path, None);
    assert!(assoc.git_state.is_none());
    // Missing worktrees are represented as a cleared worktree
    // path with no error variant attached; see WorkItemError.
    assert!(items[0].errors.is_empty());
}

#[test]
fn unlinked_prs_from_multiple_repos() {
    let rp_a = repo_path("alpha");
    let rp_b = repo_path("beta");

    // No work items at all.
    let records: Vec<crate::work_item_backend::WorkItemRecord> = vec![];

    let pr_a = create_mock_pr(1, "PR in alpha", "feature-a", "", "");
    let pr_b = create_mock_pr(2, "PR in beta", "feature-b", "", "");

    let (key_a, fetch_a) = create_mock_repo_data(rp_a, vec![], vec![pr_a], vec![]);
    let (key_b, fetch_b) = create_mock_repo_data(rp_b, vec![], vec![pr_b], vec![]);
    let repo_data = HashMap::from([(key_a, fetch_a), (key_b, fetch_b)]);

    let (items, unlinked, _, _) =
        reassemble(&records, &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert!(items.is_empty());
    assert_eq!(unlinked.len(), 2);

    let branches: Vec<&str> = unlinked.iter().map(|u| u.branch.as_str()).collect();
    assert!(branches.contains(&"feature-a"));
    assert!(branches.contains(&"feature-b"));
}
