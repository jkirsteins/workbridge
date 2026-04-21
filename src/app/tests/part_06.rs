//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

#[test]
fn display_list_unlinked_with_grouped_items() {
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
    let mut app = App::new();
    app.unlinked_prs = vec![UnlinkedPr {
        repo_path: PathBuf::from("/repo"),
        branch: "fix-typo".to_string(),
        pr: PrInfo {
            number: 1,
            title: "Fix typo".to_string(),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: String::new(),
        },
    }];
    app.work_items = vec![
        make_work_item("/repos/alpha", "Active item", WorkItemStatus::Implementing),
        make_work_item("/repos/alpha", "Backlog item", WorkItemStatus::Backlog),
    ];
    app.build_display_list();

    let headers: Vec<_> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::GroupHeader { label, count, .. } => Some((label.as_str(), *count)),
            _ => None,
        })
        .collect();
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[0], ("UNLINKED", 1));
    assert_eq!(headers[1], ("ACTIVE (alpha)", 1));
    assert_eq!(headers[2], ("BACKLOGGED (alpha)", 1));
}

#[test]
fn display_list_review_requests_sorted_direct_first_stable() {
    use crate::work_item::{
        CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision, ReviewRequestedPr,
    };

    fn make_rr(number: u64, reviewers: &[&str], teams: &[&str]) -> ReviewRequestedPr {
        ReviewRequestedPr {
            repo_path: PathBuf::from("/repo"),
            pr: PrInfo {
                number,
                title: format!("PR {number}"),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::Pending,
                checks: CheckStatus::None,
                mergeable: MergeableState::Unknown,
                url: format!("https://example.com/{number}"),
            },
            branch: format!("feature-{number}"),
            requested_reviewer_logins: reviewers.iter().map(|s| (*s).to_string()).collect(),
            requested_team_slugs: teams.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    // Input order (as returned by gh):
    //   1 team-only (core-team)
    //   2 direct (alice)
    //   3 team-only (backend)
    //   4 direct (alice)
    //   5 team-only (frontend)
    //
    // Expected order after sort:
    //   2 direct, 4 direct, 1 team, 3 team, 5 team.
    // Within each bucket the original gh order is preserved (stable).
    let mut app = App::new();
    app.current_user_login = Some("alice".into());
    app.review_requested_prs = vec![
        make_rr(1, &[], &["core-team"]),
        make_rr(2, &["alice"], &[]),
        make_rr(3, &[], &["backend"]),
        make_rr(4, &["alice"], &["core-team"]),
        make_rr(5, &[], &["frontend"]),
    ];
    app.build_display_list();

    let review_numbers: Vec<u64> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::ReviewRequestItem(i) => Some(app.review_requested_prs[*i].pr.number),
            _ => None,
        })
        .collect();
    assert_eq!(review_numbers, vec![2, 4, 1, 3, 5]);
}

#[test]
fn display_list_review_requests_no_reorder_when_login_unknown() {
    use crate::work_item::{
        CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision, ReviewRequestedPr,
    };

    let mut app = App::new();
    // Login unknown - first fetch tick hasn't resolved it yet.
    app.current_user_login = None;
    let make = |number: u64, reviewers: &[&str]| ReviewRequestedPr {
        repo_path: PathBuf::from("/repo"),
        pr: PrInfo {
            number,
            title: format!("PR {number}"),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::Pending,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: String::new(),
        },
        branch: format!("b{number}"),
        requested_reviewer_logins: reviewers.iter().map(|s| (*s).to_string()).collect(),
        requested_team_slugs: Vec::new(),
    };
    app.review_requested_prs = vec![make(1, &["alice"]), make(2, &["bob"]), make(3, &["alice"])];
    app.build_display_list();

    let review_numbers: Vec<u64> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::ReviewRequestItem(i) => Some(app.review_requested_prs[*i].pr.number),
            _ => None,
        })
        .collect();
    // Stable sort with equal keys preserves input order.
    assert_eq!(review_numbers, vec![1, 2, 3]);
}

#[test]
fn display_list_multiple_repos_get_separate_groups() {
    let mut app = App::new();
    app.work_items = vec![
        make_work_item("/repos/alpha", "Alpha task", WorkItemStatus::Implementing),
        make_work_item("/repos/beta", "Beta task", WorkItemStatus::Implementing),
        make_work_item("/repos/alpha", "Alpha backlog", WorkItemStatus::Backlog),
    ];
    app.build_display_list();

    let headers: Vec<_> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::GroupHeader { label, count, .. } => Some((label.as_str(), *count)),
            _ => None,
        })
        .collect();
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[0], ("ACTIVE (alpha)", 1));
    assert_eq!(headers[1], ("ACTIVE (beta)", 1));
    assert_eq!(headers[2], ("BACKLOGGED (alpha)", 1));
}

/// F-3: `create_work_item_with` returns error when ALL repos lack `git_dir`.
#[test]
fn create_work_item_with_errors_when_all_repos_lack_git_dir() {
    let mut app = App::new();

    // Populate cache with only repos missing git dirs.
    app.active_repo_cache = vec![RepoEntry {
        path: PathBuf::from("/repos/no-git"),
        source: RepoSource::Explicit,
        git_dir_present: false,
    }];

    let result = app.create_work_item_with(
        "Test item".into(),
        None,
        vec![PathBuf::from("/repos/no-git")],
        "feature/test".into(),
    );
    assert!(
        result.is_err(),
        "create should fail when all repos lack git dir"
    );

    let msg = app.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("No selected repos have a git directory"),
        "expected git directory error in status, got: {msg}",
    );
}

// -- Feature: merge prompt on Review -> Done --

/// `advance_stage` from Review sets `confirm_merge` instead of immediately advancing.
#[test]
fn advance_stage_review_to_done_shows_merge_prompt() {
    let mut app = App::new();
    // Manually inject a work item in Review status.
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-test.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Merge test".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    app.advance_stage();

    assert!(app.confirm_merge, "should show merge prompt");
    assert_eq!(
        app.merge_wi_id.as_ref(),
        Some(&wi_id),
        "merge_wi_id should be set to the work item",
    );
    // The merge prompt is now a dialog overlay; it no longer sets status_message.
}

// -- Feature: unclean worktree indicator + merge guard --

/// Install a `RepoFetchResult` that carries a worktree with the
/// given cleanliness fields. Used by the merge-guard tests to
/// stage stale-cache scenarios that exercise the live precheck
/// path: the cache says one thing, the live
/// `WorktreeService::list_worktrees` mock returns another, and
/// the test verifies the precheck is the authority.
pub fn install_cached_repo_with_cleanliness(
    app: &mut App,
    repo_path: &std::path::Path,
    branch: &str,
    dirty: Option<bool>,
    untracked: Option<bool>,
    unpushed: Option<u32>,
    behind_remote: Option<u32>,
) {
    let wt = crate::worktree_service::WorktreeInfo {
        path: repo_path.join(".worktrees").join(branch),
        branch: Some(branch.to_string()),
        is_main: false,
        dirty,
        untracked,
        unpushed,
        behind_remote,
        ..crate::worktree_service::WorktreeInfo::default()
    };
    app.repo_data.insert(
        repo_path.to_path_buf(),
        crate::work_item::RepoFetchResult {
            repo_path: repo_path.to_path_buf(),
            github_remote: Some(("owner".into(), "repo".into())),
            worktrees: Ok(vec![wt]),
            prs: Ok(Vec::new()),
            review_requested_prs: Ok(Vec::new()),
            issues: Vec::new(),
            current_user_login: None,
        },
    );
}

/// Push a Review-stage work item with a single repo association
/// on the given branch, and select it in the display list. Mirrors
/// the shape `advance_stage` expects to see.
pub fn push_selected_review_item(
    app: &mut App,
    wi_id: &WorkItemId,
    repo_path: &std::path::Path,
    branch: &str,
) {
    app.work_items.push(crate::work_item::WorkItem {
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "cleanliness-test".into(),
        display_id: None,
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: repo_path.to_path_buf(),
            branch: Some(branch.to_string()),
            worktree_path: Some(repo_path.join(".worktrees").join(branch)),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);
}

/// Priority ordering contract of `MergeReadiness::classify`:
///   Dirty > Untracked > Unpushed > `PrConflict` > `CiFailing` >
///   `BehindOnly` > Clean
///
/// This is the single canonical classifier used by the live
/// merge precheck (`spawn_merge_precheck`). Any drift on the
/// priority would make a worktree with multiple concurrent
/// blockers surface a wrong-flavor block message, so the order
/// is pinned here.
#[test]
fn merge_readiness_classifies_priority_order() {
    fn wt(
        dirty: Option<bool>,
        untracked: Option<bool>,
        unpushed: Option<u32>,
        behind: Option<u32>,
    ) -> crate::worktree_service::WorktreeInfo {
        crate::worktree_service::WorktreeInfo {
            path: PathBuf::from("/tmp/priority/.worktrees/b"),
            branch: Some("b".into()),
            is_main: false,
            dirty,
            untracked,
            unpushed,
            behind_remote: behind,
            ..crate::worktree_service::WorktreeInfo::default()
        }
    }

    use crate::github_client::LivePrState;
    use crate::work_item::{CheckStatus, MergeableState};

    let clean_pr = LivePrState::no_pr();
    let conflict_pr = LivePrState {
        mergeable: MergeableState::Conflicting,
        check_rollup: CheckStatus::Passing,
        has_open_pr: true,
    };
    let ci_failing_pr = LivePrState {
        mergeable: MergeableState::Mergeable,
        check_rollup: CheckStatus::Failing,
        has_open_pr: true,
    };
    let ci_pending_pr = LivePrState {
        mergeable: MergeableState::Mergeable,
        check_rollup: CheckStatus::Pending,
        has_open_pr: true,
    };
    let mergeable_pr = LivePrState {
        mergeable: MergeableState::Mergeable,
        check_rollup: CheckStatus::Passing,
        has_open_pr: true,
    };

    // Dirty wins over every other blocker (including PR conflict
    // + CI failing).
    let wt_all_bad = wt(Some(true), Some(true), Some(5), Some(5));
    assert_eq!(
        MergeReadiness::classify(Some(&wt_all_bad), &conflict_pr),
        MergeReadiness::Dirty,
    );
    assert_eq!(
        MergeReadiness::classify(Some(&wt_all_bad), &ci_failing_pr),
        MergeReadiness::Dirty,
    );

    // Untracked wins over Unpushed + BehindOnly.
    let wt_untracked = wt(Some(false), Some(true), Some(3), Some(3));
    assert_eq!(
        MergeReadiness::classify(Some(&wt_untracked), &clean_pr),
        MergeReadiness::Untracked,
    );

    // Unpushed wins over BehindOnly (and over any PR state).
    let wt_unpushed = wt(Some(false), Some(false), Some(2), Some(4));
    assert_eq!(
        MergeReadiness::classify(Some(&wt_unpushed), &conflict_pr),
        MergeReadiness::Unpushed(2),
    );

    // Clean worktree + PR conflict => PrConflict.
    let wt_clean = wt(Some(false), Some(false), Some(0), Some(0));
    assert_eq!(
        MergeReadiness::classify(Some(&wt_clean), &conflict_pr),
        MergeReadiness::PrConflict,
    );

    // Clean worktree + mergeable PR + failing CI => CiFailing.
    assert_eq!(
        MergeReadiness::classify(Some(&wt_clean), &ci_failing_pr),
        MergeReadiness::CiFailing,
    );

    // Clean worktree + mergeable PR + pending CI => Clean
    // (pending does not block).
    assert_eq!(
        MergeReadiness::classify(Some(&wt_clean), &ci_pending_pr),
        MergeReadiness::Clean,
    );

    // Clean worktree + mergeable PR + passing CI => Clean.
    assert_eq!(
        MergeReadiness::classify(Some(&wt_clean), &mergeable_pr),
        MergeReadiness::Clean,
    );

    // No local worktree + PR conflict => PrConflict (PR-only
    // items still block on the remote signal).
    assert_eq!(
        MergeReadiness::classify(None, &conflict_pr),
        MergeReadiness::PrConflict,
    );

    // No local worktree + no PR => Clean (nothing to protect
    // and no remote constraints).
    assert_eq!(
        MergeReadiness::classify(None, &clean_pr),
        MergeReadiness::Clean,
    );

    // BehindOnly when worktree is only behind and PR is mergeable.
    let wt_behind = wt(Some(false), Some(false), Some(0), Some(7));
    assert_eq!(
        MergeReadiness::classify(Some(&wt_behind), &mergeable_pr),
        MergeReadiness::BehindOnly(7),
    );

    // All zero / clean.
    assert_eq!(
        MergeReadiness::classify(Some(&wt_clean), &clean_pr),
        MergeReadiness::Clean,
    );

    // All-None fields (fetcher check not yet run) => Clean: the
    // safe default that lets the live precheck proceed and
    // re-classify against fresh data.
    let wt_all_none = wt(None, None, None, None);
    assert_eq!(
        MergeReadiness::classify(Some(&wt_all_none), &clean_pr),
        MergeReadiness::Clean,
    );
}

/// `merge_block_message` is the single source of truth for
/// "does this state block the Review -> Done merge?": every
/// blocking variant returns `Some` and every non-blocking
/// variant returns `None`. The merge precheck path uses the
/// `Some` / `None` discriminant directly, so this test pins
/// both that contract and the user-facing wording so copy
/// edits to `merge_block_message` get caught.
#[test]
fn merge_readiness_merge_block_message_classifies_correctly() {
    for (state, blocking) in [
        (MergeReadiness::Clean, false),
        (MergeReadiness::Dirty, true),
        (MergeReadiness::Untracked, true),
        (MergeReadiness::Unpushed(1), true),
        (MergeReadiness::PrConflict, true),
        (MergeReadiness::CiFailing, true),
        (MergeReadiness::BehindOnly(1), false),
    ] {
        assert_eq!(
            state.merge_block_message().is_some(),
            blocking,
            "{state:?} message presence must reflect blocking-ness",
        );
    }
    // Spot-check the specific wording so copy edits are caught.
    assert!(
        MergeReadiness::Dirty
            .merge_block_message()
            .unwrap()
            .contains("Uncommitted changes"),
    );
    assert!(
        MergeReadiness::Untracked
            .merge_block_message()
            .unwrap()
            .contains("Untracked files"),
    );
    assert!(
        MergeReadiness::Unpushed(3)
            .merge_block_message()
            .unwrap()
            .contains("Unpushed commits"),
    );
    assert!(
        MergeReadiness::PrConflict
            .merge_block_message()
            .unwrap()
            .contains("PR has conflicts"),
    );
    assert!(
        MergeReadiness::CiFailing
            .merge_block_message()
            .unwrap()
            .contains("CI failing"),
    );
}

// -------------------------------------------------------------------
// App::merge_confirm_hint - soft, cache-based hint shown in the
// pre-confirm merge modal. Never refuses; always advisory.
// -------------------------------------------------------------------

/// Bundle of state fields consumed by `push_review_item_with_state`.
/// Consolidates the dirty / ahead / behind / checks / mergeable
/// knobs into a single struct so the helper signature stays short.
pub struct ReviewItemState {
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    pub pr_checks: crate::work_item::CheckStatus,
    pub pr_mergeable: crate::work_item::MergeableState,
}

/// Helper used by the hint tests: push a Review-stage item with a
/// single association whose `PrInfo` + `GitState` are fully
/// configurable. Mirrors `push_selected_review_item` but exposes
/// the state fields the hint reads.
pub fn push_review_item_with_state(app: &mut App, wi_id: &WorkItemId, state: &ReviewItemState) {
    use crate::work_item::{GitState, PrInfo, PrState, ReviewDecision};
    let repo_path = PathBuf::from("/tmp/hint-tests-repo");
    let branch = "feature/hint-tests".to_string();
    app.work_items.push(crate::work_item::WorkItem {
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "hint-test".into(),
        display_id: None,
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path,
            branch: Some(branch),
            worktree_path: None,
            pr: Some(PrInfo {
                number: 42,
                title: "hint-test PR".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::Approved,
                checks: state.pr_checks.clone(),
                mergeable: state.pr_mergeable.clone(),
                url: "https://example.com/pr/42".into(),
            }),
            issue: None,
            git_state: Some(GitState {
                dirty: state.dirty,
                ahead: state.ahead,
                behind: state.behind,
            }),
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
}

/// No-hint case: everything is clean / passing / mergeable.
#[test]
fn merge_confirm_hint_returns_none_when_all_clean() {
    use crate::work_item::{CheckStatus, MergeableState};

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-all-clean.json"));
    push_review_item_with_state(
        &mut app,
        &wi_id,
        &ReviewItemState {
            dirty: false,
            ahead: 0,
            behind: 0,
            pr_checks: CheckStatus::Passing,
            pr_mergeable: MergeableState::Mergeable,
        },
    );

    assert_eq!(app.merge_confirm_hint(&wi_id), None);
}
