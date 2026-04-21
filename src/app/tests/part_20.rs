//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// Gap 3 regression: same property on the approve path. The
/// approve path advances the work item to Review and spawns a
/// session for the new stage, so other activities may exist
/// afterwards - we assert only that the gate's specific ID is
/// gone.
#[test]
fn poll_review_gate_result_ends_status_bar_activity_approve() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Result(ReviewGateResult {
        work_item_id: wi_id.clone(),
        approved: true,
        detail: "looks good".into(),
    }))
    .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    let gate_aid = app
        .review_gates
        .get(&wi_id)
        .map(|g| g.activity)
        .expect("inserted gate must expose its ActivityId");

    app.poll_review_gate();

    assert!(
        !app.review_gates.contains_key(&wi_id),
        "Result gate must be dropped",
    );
    assert!(
        !app.activities.entries.iter().any(|a| a.id == gate_aid),
        "Result arm of poll_review_gate must end the gate's specific \
         ActivityId via drop_review_gate",
    );
}

// -- User action guard --

/// `try_begin_user_action` followed by `end_user_action` admits a
/// single action, starts one activity, and clears it cleanly.
#[test]
fn user_action_try_begin_then_end_roundtrip() {
    let mut app = App::new();
    let aid = app
        .try_begin_user_action(UserActionKey::PrCreate, Duration::ZERO, "Creating PR...")
        .expect("first admit must succeed");
    assert!(app.is_user_action_in_flight(&UserActionKey::PrCreate));
    assert!(app.activities.entries.iter().any(|a| a.id == aid));
    app.end_user_action(&UserActionKey::PrCreate);
    assert!(!app.is_user_action_in_flight(&UserActionKey::PrCreate));
    assert!(!app.activities.entries.iter().any(|a| a.id == aid));
}

/// Calling `try_begin_user_action` twice without an intermediate
/// `end_user_action` must reject the second call.
#[test]
fn user_action_try_begin_rejects_second_concurrent_call() {
    let mut app = App::new();
    let first = app
        .try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging...")
        .expect("first admit must succeed");
    let second = app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging...");
    assert!(second.is_none(), "second concurrent admit must return None");
    // First activity is still owned by the helper.
    assert!(app.activities.entries.iter().any(|a| a.id == first));
}

/// A debounce window blocks a fresh admit even after the previous
/// one has been ended.
#[test]
fn user_action_debounce_window_blocks_repeat() {
    let mut app = App::new();
    app.try_begin_user_action(
        UserActionKey::GithubRefresh,
        Duration::from_millis(500),
        "Refreshing...",
    )
    .expect("first admit must succeed");
    app.end_user_action(&UserActionKey::GithubRefresh);
    // Immediate retry within the debounce window is rejected.
    let retry = app.try_begin_user_action(
        UserActionKey::GithubRefresh,
        Duration::from_millis(500),
        "Refreshing...",
    );
    assert!(retry.is_none(), "debounce must reject rapid retry");
}

/// Once the debounce window has elapsed, a fresh admit is
/// accepted.
#[test]
fn user_action_debounce_elapsed_allows_retry() {
    let mut app = App::new();
    // Use a very short (10ms) debounce so the test does not
    // actually have to sleep in production CI. The plan pins
    // debounce values at the call site, so direct overrides are
    // the supported way to test.
    app.try_begin_user_action(
        UserActionKey::GithubRefresh,
        Duration::from_millis(10),
        "Refreshing...",
    )
    .expect("first admit must succeed");
    app.end_user_action(&UserActionKey::GithubRefresh);
    crate::side_effects::clock::sleep(Duration::from_millis(20));
    let retry = app.try_begin_user_action(
        UserActionKey::GithubRefresh,
        Duration::from_millis(10),
        "Refreshing...",
    );
    assert!(retry.is_some(), "debounce should allow retry after elapse");
}

/// `end_user_action` is idempotent: calling it a second time is a
/// silent no-op (no panic, no spurious activity cleanup).
#[test]
fn user_action_end_is_idempotent() {
    let mut app = App::new();
    app.try_begin_user_action(UserActionKey::ReviewSubmit, Duration::ZERO, "Submitting...")
        .expect("admit must succeed");
    app.end_user_action(&UserActionKey::ReviewSubmit);
    // Second end is a no-op.
    app.end_user_action(&UserActionKey::ReviewSubmit);
    // Third end on a key that was never admitted is also a no-op.
    app.end_user_action(&UserActionKey::DeleteCleanup);
}

/// Unit test for `try_begin_user_action`: a second admit on the
/// same key while the first is still in flight is rejected. This
/// only covers the helper-level in-flight check; the full Ctrl+R
/// dispatch path (including the `activities.pending_fetch_count` hard gate
/// and the status message wiring) is exercised by
/// `ctrl_r_rapid_double_press_through_handle_key_is_gated` in
/// `src/event.rs`.
#[test]
fn user_action_second_admit_rejected_while_in_flight() {
    let mut app = App::new();
    // First admit succeeds.
    let first = app.try_begin_user_action(
        UserActionKey::GithubRefresh,
        Duration::from_millis(500),
        "Refreshing GitHub data",
    );
    assert!(first.is_some(), "first admit must succeed");
    // While the helper entry is still in flight, a second admit is
    // rejected by the in-flight check.
    let second = app.try_begin_user_action(
        UserActionKey::GithubRefresh,
        Duration::from_millis(500),
        "Refreshing GitHub data",
    );
    assert!(second.is_none(), "second admit must be rejected");
}

/// `reset_fetch_state` is the single site that tears down all
/// fetcher-derived UI state on a structural restart (see the
/// salsa.rs `fetcher_repos_changed` block). It must reset three
/// invariants together:
///   1. drop `fetch_rx`
///   2. zero `activities.pending_fetch_count`
///   3. end both possible spinner owners (the `GithubRefresh`
///      helper entry AND `activities.structural_fetch`)
///
/// This test seeds the derived state as if two `FetchStarted`
/// messages had been counted but their paired terminal messages
/// were stranded on the old channel, then asserts that the reset
/// leaves the app in a clean slate that does NOT strand the Ctrl+R
/// count gate for the rest of the process lifetime.
#[test]
fn reset_fetch_state_clears_all_fetcher_derived_state() {
    let mut app = App::new();

    // Seed state as if the fetcher had started and the Ctrl+R
    // helper entry was admitted (covers the path where a Ctrl+R
    // was in flight when the restart happened).
    app.try_begin_user_action(
        UserActionKey::GithubRefresh,
        Duration::ZERO,
        "Refreshing GitHub data",
    )
    .expect("admit must succeed");
    // Simulate two repos' `FetchStarted` counted but not yet
    // paired with `RepoData`/`FetcherError`. These are exactly
    // the messages that would be stranded when the old channel is
    // dropped by the restart.
    app.activities.pending_fetch_count = 2;

    // Sanity-check the seeded state.
    assert!(app.is_user_action_in_flight(&UserActionKey::GithubRefresh));
    assert_eq!(app.activities.pending_fetch_count, 2);
    assert!(!app.activities.is_empty());

    // Simulate the salsa restart block.
    app.reset_fetch_state();

    // All three invariants must be clear.
    assert!(
        app.fetch_rx.is_none(),
        "fetch_rx must be dropped by reset_fetch_state",
    );
    assert_eq!(
        app.activities.pending_fetch_count, 0,
        "activities.pending_fetch_count must be reset to 0 - otherwise the Ctrl+R \
         hard gate in src/event.rs permanently locks out refresh",
    );
    assert!(
        !app.is_user_action_in_flight(&UserActionKey::GithubRefresh),
        "GithubRefresh helper entry must be cleared",
    );
    assert!(
        app.activities.structural_fetch.is_none(),
        "activities.structural_fetch must be cleared",
    );
    assert!(
        app.activities.is_empty(),
        "no stray status-bar spinners may survive the reset",
    );
}

/// `reset_fetch_state` must also handle the structural-fallback
/// ownership path: when `FetchStarted` arrived without a prior
/// Ctrl+R admit (manage/unmanage, work-item create, delete
/// cleanup, etc.), the spinner is owned by
/// `activities.structural_fetch` rather than the helper entry. The
/// reset must end that activity too, not just the helper.
#[test]
fn reset_fetch_state_ends_structural_fetch_activity() {
    let mut app = App::new();
    // Simulate `drain_fetch_results` on the structural-restart
    // path: no helper entry, but a counted FetchStarted and an
    // owned structural activity.
    let id = app.activities.start("Refreshing GitHub data");
    app.activities.structural_fetch = Some(id);
    app.activities.pending_fetch_count = 1;
    assert!(!app.is_user_action_in_flight(&UserActionKey::GithubRefresh));

    app.reset_fetch_state();

    assert_eq!(app.activities.pending_fetch_count, 0);
    assert!(app.activities.structural_fetch.is_none());
    assert!(
        app.activities.is_empty(),
        "activities.structural_fetch id must be removed from the activity list",
    );
}

// -----------------------------------------------------------------------
// `selected_pr_target` - resolves selection -> PR URL for the `o` key.
// The tests target the pure helper rather than
// `open_selected_pr_in_browser` so the suite stays hermetic (no thread
// spawn, no shell-out to `open`).
// -----------------------------------------------------------------------

pub fn sample_pr_info(number: u64, url: &str) -> crate::work_item::PrInfo {
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
    PrInfo {
        number,
        title: format!("PR {number}"),
        state: PrState::Open,
        is_draft: false,
        review_decision: ReviewDecision::None,
        checks: CheckStatus::None,
        mergeable: MergeableState::Unknown,
        url: url.to_string(),
    }
}

#[test]
fn open_pr_resolves_review_request_url() {
    let mut app = App::new();
    app.review_requested_prs
        .push(crate::work_item::ReviewRequestedPr {
            repo_path: PathBuf::from("/repo"),
            pr: sample_pr_info(42, "https://github.com/o/r/pull/42"),
            branch: "feat/x".into(),
            requested_reviewer_logins: Vec::new(),
            requested_team_slugs: Vec::new(),
        });
    app.display_list.push(DisplayEntry::ReviewRequestItem(0));
    app.selected_item = Some(0);

    let target = app.selected_pr_target();
    assert_eq!(
        target,
        Some(("https://github.com/o/r/pull/42".into(), "PR #42".into())),
    );
}

#[test]
fn open_pr_resolves_unlinked_url() {
    let mut app = App::new();
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: PathBuf::from("/repo"),
        pr: sample_pr_info(7, "https://github.com/o/r/pull/7"),
        branch: "feat/y".into(),
    });
    app.display_list.push(DisplayEntry::UnlinkedItem(0));
    app.selected_item = Some(0);

    let target = app.selected_pr_target();
    assert_eq!(
        target,
        Some(("https://github.com/o/r/pull/7".into(), "PR #7".into())),
    );
}

#[test]
fn open_pr_resolves_workitem_with_pr() {
    use crate::work_item::RepoAssociation;
    let mut app = App::new();
    // First association has no PR, second has one - asserts that the
    // "first PR-bearing association wins" rule is deterministic and
    // does NOT require the first association to be PR-bearing.
    app.work_items.push(WorkItem {
        id: WorkItemId::LocalFile(PathBuf::from("/data/wi.json")),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        display_id: None,
        title: "Work".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![
            RepoAssociation {
                repo_path: PathBuf::from("/repo-a"),
                branch: Some("feat/a".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            },
            RepoAssociation {
                repo_path: PathBuf::from("/repo-b"),
                branch: Some("feat/b".into()),
                worktree_path: None,
                pr: Some(sample_pr_info(99, "https://github.com/o/r/pull/99")),
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            },
        ],
        errors: vec![],
    });
    app.display_list.push(DisplayEntry::WorkItemEntry(0));
    app.selected_item = Some(0);

    let target = app.selected_pr_target();
    assert_eq!(
        target,
        Some(("https://github.com/o/r/pull/99".into(), "PR #99".into())),
    );
}

#[test]
fn open_pr_none_for_workitem_without_pr() {
    use crate::work_item::RepoAssociation;
    let mut app = App::new();
    app.work_items.push(WorkItem {
        id: WorkItemId::LocalFile(PathBuf::from("/data/wi.json")),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        display_id: None,
        title: "Work".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![RepoAssociation {
            repo_path: PathBuf::from("/repo-a"),
            branch: Some("feat/a".into()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
    app.display_list.push(DisplayEntry::WorkItemEntry(0));
    app.selected_item = Some(0);

    assert_eq!(app.selected_pr_target(), None);
}

#[test]
fn open_pr_none_for_group_header() {
    let mut app = App::new();
    app.display_list.push(DisplayEntry::GroupHeader {
        label: "ACTIVE".into(),
        count: 0,
        kind: GroupHeaderKind::Normal,
    });
    app.selected_item = Some(0);

    assert_eq!(app.selected_pr_target(), None);
}

#[test]
fn open_pr_sets_no_pr_status_when_no_pr() {
    // Smoke test for the `open_selected_pr_in_browser` wrapper on the
    // None path: asserts that the user-visible status message is set
    // without spawning the `open` subprocess. The Some path is not
    // exercised here because it spawns a thread and shells out.
    let mut app = App::new();
    app.display_list.push(DisplayEntry::GroupHeader {
        label: "ACTIVE".into(),
        count: 0,
        kind: GroupHeaderKind::Normal,
    });
    app.selected_item = Some(0);
    app.open_selected_pr_in_browser();
    assert_eq!(app.shell.status_message.as_deref(), Some("No PR to open"));
}

// -----------------------------------------------------------------------
// `selected_rebase_target` - resolves selection -> rebase target for `m`.
// The tests target the pure helper rather than `start_rebase_on_main`
// because the latter would call `spawn_rebase_gate`, which spawns a
// thread and shells out to `git fetch` / `claude`. Hermetic.
// -----------------------------------------------------------------------

#[test]
fn rebase_target_resolves_workitem_with_worktree_and_branch() {
    use crate::work_item::RepoAssociation;
    let mut app = App::new();
    // First association has no worktree, second has both - asserts
    // that the helper picks the first repo with a worktree AND a
    // branch, so unresolved associations do not block.
    app.work_items.push(WorkItem {
        id: WorkItemId::LocalFile(PathBuf::from("/data/wi.json")),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        display_id: None,
        title: "Work".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![
            RepoAssociation {
                repo_path: PathBuf::from("/repo-a"),
                branch: Some("feat/a".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            },
            RepoAssociation {
                repo_path: PathBuf::from("/repo-b"),
                branch: Some("feat/b".into()),
                worktree_path: Some(PathBuf::from("/repo-b/.worktrees/feat/b")),
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            },
        ],
        errors: vec![],
    });
    app.display_list.push(DisplayEntry::WorkItemEntry(0));
    app.selected_item = Some(0);

    let target = app
        .selected_rebase_target()
        .expect("workitem with a worktreed branch must produce a rebase target");
    // Critical: the target must carry the WORKTREE path, not the
    // registered repo path. The rebase later runs `git -C <path>`
    // and `Command::current_dir(<path>)` against this; if it were
    // the repo path, the rebase would target whatever the main
    // checkout had checked out instead of `feat/b`.
    assert_eq!(
        target.worktree_path,
        PathBuf::from("/repo-b/.worktrees/feat/b")
    );
    assert_eq!(target.branch, "feat/b");
}

#[test]
fn rebase_target_none_for_workitem_without_worktree() {
    use crate::work_item::RepoAssociation;
    let mut app = App::new();
    app.work_items.push(WorkItem {
        id: WorkItemId::LocalFile(PathBuf::from("/data/wi.json")),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        display_id: None,
        title: "Work".into(),
        description: None,
        status: WorkItemStatus::Backlog,
        status_derived: false,
        repo_associations: vec![RepoAssociation {
            repo_path: PathBuf::from("/repo-a"),
            branch: Some("feat/a".into()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
    app.display_list.push(DisplayEntry::WorkItemEntry(0));
    app.selected_item = Some(0);

    assert!(
        app.selected_rebase_target().is_none(),
        "no worktree => no rebase target",
    );
}

#[test]
fn rebase_target_none_for_unlinked() {
    let mut app = App::new();
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: PathBuf::from("/repo"),
        pr: sample_pr_info(7, "https://github.com/o/r/pull/7"),
        branch: "feat/y".into(),
    });
    app.display_list.push(DisplayEntry::UnlinkedItem(0));
    app.selected_item = Some(0);
    assert!(app.selected_rebase_target().is_none());
}

#[test]
fn rebase_target_none_for_review_request() {
    let mut app = App::new();
    app.review_requested_prs
        .push(crate::work_item::ReviewRequestedPr {
            repo_path: PathBuf::from("/repo"),
            pr: sample_pr_info(42, "https://github.com/o/r/pull/42"),
            branch: "feat/x".into(),
            requested_reviewer_logins: Vec::new(),
            requested_team_slugs: Vec::new(),
        });
    app.display_list.push(DisplayEntry::ReviewRequestItem(0));
    app.selected_item = Some(0);
    assert!(app.selected_rebase_target().is_none());
}

#[test]
fn rebase_target_none_for_group_header() {
    let mut app = App::new();
    app.display_list.push(DisplayEntry::GroupHeader {
        label: "ACTIVE".into(),
        count: 0,
        kind: GroupHeaderKind::Normal,
    });
    app.selected_item = Some(0);
    assert!(app.selected_rebase_target().is_none());
}

#[test]
fn start_rebase_on_main_sets_status_when_nothing_to_rebase() {
    // Smoke test for `start_rebase_on_main` on the None path: the
    // helper must surface a user-visible status message without
    // spawning a thread or shelling out. The Some path is not
    // exercised here because it spawns a background thread that
    // calls `git fetch` and `claude`.
    let mut app = App::new();
    app.display_list.push(DisplayEntry::GroupHeader {
        label: "ACTIVE".into(),
        count: 0,
        kind: GroupHeaderKind::Normal,
    });
    app.selected_item = Some(0);
    app.start_rebase_on_main();
    assert_eq!(
        app.shell.status_message.as_deref(),
        Some("No branch to rebase")
    );
}
