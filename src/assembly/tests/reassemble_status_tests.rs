//! Tests for derived fields produced by `reassemble`: git state projection,
//! status derivation from merged PRs, backend-type inference, `display_id`
//! pass-through, and issue-lookup error plumbing.

use std::collections::HashMap;
use std::path::PathBuf;

use super::{
    DEFAULT_ISSUE_PATTERN, create_mock_issue, create_mock_pr, create_mock_record,
    create_mock_repo_data, create_mock_worktree, repo_path,
};
use crate::assembly::reassemble;
use crate::github_client::GithubError;
use crate::work_item::{
    BackendType, RepoFetchResult, WorkItemError, WorkItemId, WorkItemKind, WorkItemStatus,
};
use crate::work_item_backend::{RepoAssociationRecord, WorkItemRecord};
use crate::worktree_service::WorktreeInfo;

/// Cache-projected cleanliness fields on `WorktreeInfo`
/// (`dirty` / `untracked` / `unpushed` / `behind_remote`) must
/// flow into the derived `GitState`. `dirty` on `GitState` is the
/// union of tracked-dirty and untracked so the UI chip renders
/// for either - the merge guard separates them via
/// `MergeReadiness::classify`, which reads the raw fields.
#[test]
fn git_state_flows_from_worktree_info_fields() {
    let rp = repo_path("alpha");
    let branch = "feature-dirty";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Review,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    let wt = WorktreeInfo {
        path: PathBuf::from("/worktrees/feature-dirty"),
        branch: Some(branch.to_string()),
        is_main: false,
        dirty: Some(true),
        untracked: Some(false),
        unpushed: Some(2),
        behind_remote: Some(1),
        ..WorktreeInfo::default()
    };
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![wt], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    let gs = items[0].repo_associations[0]
        .git_state
        .as_ref()
        .expect("git_state must be populated when a worktree matches");
    assert!(gs.dirty, "dirty=true must flow through");
    assert_eq!(gs.ahead, 2, "unpushed must flow into GitState.ahead");
    assert_eq!(gs.behind, 1, "behind_remote must flow into GitState.behind");
}

/// Tracked-dirty=false + untracked=true must still set
/// `GitState.dirty=true` because the chip treats both the same.
#[test]
fn git_state_dirty_is_union_of_tracked_and_untracked() {
    let rp = repo_path("alpha");
    let branch = "feature-untracked";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Review,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    let wt = WorktreeInfo {
        path: PathBuf::from("/worktrees/feature-untracked"),
        branch: Some(branch.to_string()),
        is_main: false,
        dirty: Some(false),
        untracked: Some(true),
        unpushed: Some(0),
        behind_remote: Some(0),
        ..WorktreeInfo::default()
    };
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![wt], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    let gs = items[0].repo_associations[0]
        .git_state
        .as_ref()
        .expect("git_state must be populated");
    assert!(
        gs.dirty,
        "untracked-only must still set GitState.dirty so the chip renders",
    );
    assert_eq!(gs.ahead, 0);
    assert_eq!(gs.behind, 0);
}

/// `None` cleanliness fields (fetcher check failed / skipped) must
/// collapse to the safe default: clean/zero counts.
#[test]
fn git_state_none_cleanliness_fields_default_to_clean() {
    let rp = repo_path("alpha");
    let branch = "feature-unknown";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Review,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // All cleanliness fields = None.
    let wt = create_mock_worktree("/worktrees/feature-unknown", Some(branch));
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![wt], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    let gs = items[0].repo_associations[0]
        .git_state
        .as_ref()
        .expect("git_state must be populated when worktree matches");
    assert!(!gs.dirty, "None dirty must default to clean");
    assert_eq!(gs.ahead, 0, "None unpushed must default to 0");
    assert_eq!(gs.behind, 0, "None behind_remote must default to 0");
}

#[test]
fn reassemble_propagates_display_id() {
    // The assembly layer must pass `display_id` through from the
    // backend record unchanged. It is not derived - a legacy
    // record with `None` must produce a `WorkItem` with `None`,
    // and a record with `Some("foo-42")` must produce a
    // `WorkItem` with the same string.
    let rp = repo_path("alpha");
    let mut record = create_mock_record(
        "wi-display",
        "title",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: None,
            pr_identity: None,
        }],
    );
    record.display_id = Some("alpha-42".into());

    let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].display_id.as_deref(),
        Some("alpha-42"),
        "display_id must be passed through from the record"
    );

    // Legacy record without display_id -> None on the WorkItem.
    let legacy = create_mock_record(
        "wi-legacy",
        "title",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: None,
            pr_identity: None,
        }],
    );
    assert!(legacy.display_id.is_none());
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);
    let (items, _, _, _) = reassemble(&[legacy], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
    assert_eq!(items[0].display_id, None);
}

#[test]
fn issue_extraction_from_branch() {
    let rp = repo_path("alpha");
    let branch = "42-fix-bug";

    let record = create_mock_record(
        "wi-1",
        "",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    let issue = create_mock_issue(42, "Fix the bug");

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![], vec![(42, Ok(issue))]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, r"^(\d+)-", ".worktrees");

    assert_eq!(items.len(), 1);
    let assoc = &items[0].repo_associations[0];
    let issue_info = assoc.issue.as_ref().expect("should have issue info");
    assert_eq!(issue_info.number, 42);
    assert_eq!(issue_info.title, "Fix the bug");

    // Title should be the issue title (no PR available, empty backend title).
    assert_eq!(items[0].title, "Fix the bug");
}

#[test]
fn issue_not_found_when_fetcher_attempted() {
    // IssueNotFound should only fire when the fetcher attempted the
    // lookup (issue number present in fetch.issues) but got an error.
    let rp = repo_path("alpha");
    let branch = "99-missing-issue";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // Fetcher attempted issue #99 but got an error (not found).
    let (rp_key, fetch) = create_mock_repo_data(
        rp,
        vec![],
        vec![],
        vec![(99, Err(GithubError::ApiError("not found".into())))],
    );
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert!(
        items[0].errors.iter().any(|e| matches!(
            e,
            WorkItemError::IssueNotFound {
                issue_number: 99,
                ..
            }
        )),
        "expected IssueNotFound error, got: {:?}",
        items[0].errors
    );
}

#[test]
fn no_issue_not_found_when_fetcher_did_not_attempt() {
    // When the issue number is NOT in fetch.issues at all (fetcher
    // never tried, e.g. no worktree for this branch), we should NOT
    // emit IssueNotFound - just leave issue as None.
    let rp = repo_path("alpha");
    let branch = "99-missing-issue";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // Repo data exists but fetcher did not attempt issue #99.
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert!(
        items[0].errors.is_empty(),
        "expected no errors when fetcher did not attempt issue lookup, got: {:?}",
        items[0].errors
    );
    assert!(items[0].repo_associations[0].issue.is_none());
}

#[test]
fn no_issue_not_found_when_fetch_data_absent() {
    // When repo_data has no entry for the repo (e.g. startup, before
    // first fetch), we must NOT produce an IssueNotFound error.
    let rp = repo_path("alpha");
    let branch = "99-missing-issue";

    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp,
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    // Empty repo_data - simulates startup before any fetch completes.
    let repo_data: HashMap<PathBuf, RepoFetchResult> = HashMap::new();
    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert!(
        items[0].errors.is_empty(),
        "expected no errors when fetch data is absent, got: {:?}",
        items[0].errors
    );
    // Issue info should be None (not yet available).
    assert!(items[0].repo_associations[0].issue.is_none());
}

#[test]
fn invalid_issue_pattern_does_not_panic() {
    let rp = repo_path("alpha");
    let record = create_mock_record(
        "wi-1",
        "Work",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some("42-fix-bug".to_string()),
            pr_identity: None,
        }],
    );
    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    // Invalid regex pattern should not panic - just skip issue extraction.
    let (items, _, _, _) = reassemble(&[record], &repo_data, "[invalid(", ".worktrees");
    assert_eq!(items.len(), 1);
    assert!(items[0].repo_associations[0].issue.is_none());
    // No IssueNotFound error either, since extraction was skipped.
    assert!(items[0].errors.is_empty());
}

#[test]
fn status_derived_from_backend_record() {
    let rp = repo_path("alpha");

    let todo_record = create_mock_record(
        "wi-1",
        "Todo work",
        WorkItemStatus::Backlog,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: None,
            pr_identity: None,
        }],
    );

    let in_progress_record = create_mock_record(
        "wi-2",
        "In progress work",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp,
            branch: None,
            pr_identity: None,
        }],
    );

    let repo_data = HashMap::new();
    let (items, _, _, _) = reassemble(
        &[todo_record, in_progress_record],
        &repo_data,
        DEFAULT_ISSUE_PATTERN,
        ".worktrees",
    );

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].status, WorkItemStatus::Backlog);
    assert_eq!(items[1].status, WorkItemStatus::Implementing);
}

#[test]
fn backend_type_derived_from_id() {
    let records = vec![
        WorkItemRecord {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/data/wi.json")),
            title: "Local".to_string(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo_path("alpha"),
                branch: None,
                pr_identity: None,
            }],
            plan: None,
            done_at: None,
        },
        WorkItemRecord {
            display_id: None,
            id: WorkItemId::GithubIssue {
                owner: "o".to_string(),
                repo: "r".to_string(),
                number: 1,
            },
            title: "GH Issue".to_string(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo_path("alpha"),
                branch: None,
                pr_identity: None,
            }],
            plan: None,
            done_at: None,
        },
        WorkItemRecord {
            display_id: None,
            id: WorkItemId::GithubProject {
                node_id: "node123".to_string(),
            },
            title: "GH Project".to_string(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo_path("alpha"),
                branch: None,
                pr_identity: None,
            }],
            plan: None,
            done_at: None,
        },
    ];

    let repo_data = HashMap::new();
    let (items, _, _, _) = reassemble(&records, &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items[0].backend_type, BackendType::LocalFile);
    assert_eq!(items[1].backend_type, BackendType::GithubIssue);
    assert_eq!(items[2].backend_type, BackendType::GithubProject);
}

#[test]
fn merged_pr_derives_done_status() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    // Backend record says InProgress, but the PR is merged.
    let record = create_mock_record(
        "wi-1",
        "Ship it",
        WorkItemStatus::Implementing,
        vec![RepoAssociationRecord {
            repo_path: rp.clone(),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }],
    );

    let mut pr = create_mock_pr(10, "Ship it", branch, "APPROVED", "SUCCESS");
    pr.state = "MERGED".to_string();

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].status,
        WorkItemStatus::Done,
        "merged PR should derive Done status",
    );
}

#[test]
fn open_pr_keeps_backend_status() {
    let rp = repo_path("alpha");
    let branch = "feature-x";

    // Backend record says InProgress, PR is open -> stays InProgress.
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

    let pr = create_mock_pr(10, "Work", branch, "", "");

    let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![pr], vec![]);
    let repo_data = HashMap::from([(rp_key, fetch)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].status,
        WorkItemStatus::Implementing,
        "open PR should keep backend InProgress status",
    );
}

#[test]
fn multi_repo_one_merged_pr_derives_done() {
    let rp_a = repo_path("alpha");
    let rp_b = repo_path("beta");

    let record = create_mock_record(
        "wi-1",
        "Cross-repo",
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

    // alpha PR is open, beta PR is merged -> Done (any merged = Done).
    let pr_a = create_mock_pr(10, "PR alpha", "feature-x", "", "");
    let mut pr_b = create_mock_pr(20, "PR beta", "feature-x", "", "");
    pr_b.state = "MERGED".to_string();

    let (key_a, fetch_a) = create_mock_repo_data(rp_a, vec![], vec![pr_a], vec![]);
    let (key_b, fetch_b) = create_mock_repo_data(rp_b, vec![], vec![pr_b], vec![]);
    let repo_data = HashMap::from([(key_a, fetch_a), (key_b, fetch_b)]);

    let (items, _, _, _) = reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].status,
        WorkItemStatus::Done,
        "any merged PR should derive Done status",
    );
}
