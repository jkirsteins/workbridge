//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// Live-recheck hint when `git_state.dirty` is set, even if the
/// PR is mergeable and CI passes.
#[test]
fn merge_confirm_hint_returns_live_recheck_when_dirty() {
    use crate::work_item::{CheckStatus, MergeableState};

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-dirty.json"));
    push_review_item_with_state(
        &mut app,
        &wi_id,
        &ReviewItemState {
            dirty: true,
            ahead: 0,
            behind: 0,
            pr_checks: CheckStatus::Passing,
            pr_mergeable: MergeableState::Mergeable,
        },
    );

    assert_eq!(
        app.merge_confirm_hint(&wi_id),
        Some("Live re-check will run before merging."),
    );
}

/// Live-recheck hint when `git_state.ahead > 0` (unpushed commits).
#[test]
fn merge_confirm_hint_returns_live_recheck_when_unpushed() {
    use crate::work_item::{CheckStatus, MergeableState};

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-unpushed.json"));
    push_review_item_with_state(
        &mut app,
        &wi_id,
        &ReviewItemState {
            dirty: false,
            ahead: 3,
            behind: 0,
            pr_checks: CheckStatus::Passing,
            pr_mergeable: MergeableState::Mergeable,
        },
    );

    assert_eq!(
        app.merge_confirm_hint(&wi_id),
        Some("Live re-check will run before merging."),
    );
}

/// Live-recheck hint when the PR is CONFLICTING (even with a
/// clean worktree and passing CI).
#[test]
fn merge_confirm_hint_returns_live_recheck_when_pr_conflict() {
    use crate::work_item::{CheckStatus, MergeableState};

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-pr-conflict.json"));
    push_review_item_with_state(
        &mut app,
        &wi_id,
        &ReviewItemState {
            dirty: false,
            ahead: 0,
            behind: 0,
            pr_checks: CheckStatus::Passing,
            pr_mergeable: MergeableState::Conflicting,
        },
    );

    assert_eq!(
        app.merge_confirm_hint(&wi_id),
        Some("Live re-check will run before merging."),
    );
}

/// Live-recheck hint when CI is failing (even with a clean
/// worktree and mergeable PR).
#[test]
fn merge_confirm_hint_returns_live_recheck_when_ci_failing() {
    use crate::work_item::{CheckStatus, MergeableState};

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-ci-fail.json"));
    push_review_item_with_state(
        &mut app,
        &wi_id,
        &ReviewItemState {
            dirty: false,
            ahead: 0,
            behind: 0,
            pr_checks: CheckStatus::Failing,
            pr_mergeable: MergeableState::Mergeable,
        },
    );

    assert_eq!(
        app.merge_confirm_hint(&wi_id),
        Some("Live re-check will run before merging."),
    );
}

/// Pending-only hint: clean worktree + mergeable PR + CI still
/// running surfaces the branch-protection reassurance instead of
/// the hard-block hint. Pending CI does not block the merge - it
/// simply queues on branch protection, which this wording
/// communicates.
#[test]
fn merge_confirm_hint_returns_ci_pending_variant() {
    use crate::work_item::{CheckStatus, MergeableState};

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-ci-pending.json"));
    push_review_item_with_state(
        &mut app,
        &wi_id,
        &ReviewItemState {
            dirty: false,
            ahead: 0,
            behind: 0,
            pr_checks: CheckStatus::Pending,
            pr_mergeable: MergeableState::Mergeable,
        },
    );

    assert_eq!(
        app.merge_confirm_hint(&wi_id),
        Some("CI still running; merge will queue on branch protection."),
    );
}

/// Hard-block hint takes priority over pending CI: a dirty
/// worktree + pending CI returns the "Live re-check" variant,
/// not the "CI still running" variant.
#[test]
fn merge_confirm_hint_hard_block_wins_over_pending() {
    use crate::work_item::{CheckStatus, MergeableState};

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-dirty-and-pending.json"));
    push_review_item_with_state(
        &mut app,
        &wi_id,
        &ReviewItemState {
            dirty: true,
            ahead: 0,
            behind: 0,
            pr_checks: CheckStatus::Pending,
            pr_mergeable: MergeableState::Mergeable,
        },
    );

    assert_eq!(
        app.merge_confirm_hint(&wi_id),
        Some("Live re-check will run before merging."),
    );
}

/// `merge_confirm_hint` on a non-existent work item returns
/// `None` without panicking. Mirrors defensive UI code paths that
/// may hold a `WorkItemId` after reassembly dropped the item.
#[test]
fn merge_confirm_hint_returns_none_for_missing_wi_id() {
    let app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/hint-missing.json"));
    assert_eq!(app.merge_confirm_hint(&wi_id), None);
}

/// Regression: a stale `dirty: true` cache on the primary
/// association must NOT prevent `advance_stage` from opening the
/// merge confirm modal. The cached guard that used to live here
/// short-circuited the live `WorktreeService::list_worktrees`
/// precheck (which only runs from `execute_merge`, downstream of
/// the modal), so users whose cache had gone stale across a long
/// session would see "Uncommitted changes" forever even after
/// committing. The authoritative merge guard now lives entirely
/// in `execute_merge` -> `spawn_merge_precheck`; this test pins
/// the absence of the cached guard so a future regression cannot
/// re-introduce it without rewriting the assertions.
#[test]
fn advance_stage_review_to_done_opens_modal_when_cache_dirty() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/merge-guard-dirty");
    let branch = "feature/dirty";
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        branch,
        Some(true),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-guard-dirty.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, branch);

    app.advance_stage();

    assert!(
        app.confirm_merge,
        "merge modal must open even when cache says dirty - the live precheck in execute_merge is the only authority",
    );
    assert_eq!(
        app.merge_wi_id.as_ref(),
        Some(&wi_id),
        "merge_wi_id must be set so the modal knows which item it's gating",
    );
    assert!(
        app.alert_message.is_none(),
        "no alert should fire from advance_stage; got: {:?}",
        app.alert_message,
    );
    assert_eq!(
        app.work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status,
        WorkItemStatus::Review,
        "item stays in Review while the modal is open",
    );
}

/// Same regression as `..._opens_modal_when_cache_dirty` but for
/// a stale `untracked: true` cache. See that test's doc comment
/// for the why - the cached guard had a per-variant arm that
/// also needs to be gone.
#[test]
fn advance_stage_review_to_done_opens_modal_when_cache_untracked() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/merge-guard-untracked");
    let branch = "feature/untracked";
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        branch,
        Some(false),
        Some(true),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-guard-untracked.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, branch);

    app.advance_stage();

    assert!(app.confirm_merge);
    assert!(app.alert_message.is_none(), "{:?}", app.alert_message);
}

/// Same regression as `..._opens_modal_when_cache_dirty` but for
/// a stale `unpushed > 0` cache.
#[test]
fn advance_stage_review_to_done_opens_modal_when_cache_unpushed() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/merge-guard-unpushed");
    let branch = "feature/unpushed";
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        branch,
        Some(false),
        Some(false),
        Some(3),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-guard-unpushed.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, branch);

    app.advance_stage();

    assert!(app.confirm_merge);
    assert!(app.alert_message.is_none(), "{:?}", app.alert_message);
}

/// `BehindOnly` is a soft warning and must NOT block the merge.
#[test]
fn advance_stage_review_to_done_allows_behind_only() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/merge-guard-behind-only");
    let branch = "feature/behind";
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        branch,
        Some(false),
        Some(false),
        Some(0),
        Some(5),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-guard-behind.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, branch);

    app.advance_stage();

    assert!(
        app.confirm_merge,
        "BehindOnly must fall through to the merge modal",
    );
    assert!(
        app.alert_message.is_none(),
        "no alert should fire for BehindOnly, got: {:?}",
        app.alert_message,
    );
}

/// Fully clean worktree advances normally.
#[test]
fn advance_stage_review_to_done_allows_clean() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/merge-guard-clean");
    let branch = "feature/clean";
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        branch,
        Some(false),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-guard-clean.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, branch);

    app.advance_stage();

    assert!(app.confirm_merge, "clean worktree must open merge modal");
    assert!(app.alert_message.is_none());
}

/// Regression: a stale `dirty: true` entry in the fetcher cache must
/// no longer immediately block `execute_merge`. The merge guard now
/// runs as a live `WorktreeService::list_worktrees` precheck on a
/// background thread; the cached `repo_data` value is consulted only
/// for the unclean-worktree chip in the list view, not for the
/// merge decision. Verifies:
///
/// 1. No alert is surfaced from the synchronous portion of
///    `execute_merge` even though the cache says the worktree is
///    dirty.
/// 2. The `UserActionKey::PrMerge` slot has been admitted (the
///    helper reserves the slot across BOTH the precheck phase and
///    the actual merge phase).
/// 3. `merge_in_progress` is set so the modal renders the
///    "Refreshing remote state..." spinner from the moment the
///    user pressed merge.
/// 4. The slot's payload is `UserActionPayload::PrMergePrecheck`,
///    i.e. the precheck phase is in flight (and
///    `is_merge_precheck_phase()` returns true).
/// 5. The merge confirm modal stays open (it transitions to the
///    spinner state, not to the dismissed state).
#[test]
fn execute_merge_with_stale_dirty_cache_admits_precheck_slot() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/exec-merge-stale-dirty");
    let branch = "feature/exec-stale-dirty";
    // Pre-populate the fetcher cache with a STALE dirty WorktreeInfo.
    // Pre-fix this would have caused execute_merge to immediately
    // surface the "Uncommitted changes" alert and refuse to admit
    // the user-action slot.
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        branch,
        Some(true),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/exec-merge-stale-dirty.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, branch);
    // Open the merge modal so we can verify it stays open across the
    // precheck transition.
    app.confirm_merge = true;
    app.merge_wi_id = Some(wi_id.clone());

    app.execute_merge(&wi_id, "squash");

    assert!(
        app.alert_message.is_none(),
        "stale-dirty cache must NOT surface an immediate alert; got: {:?}",
        app.alert_message,
    );
    assert!(
        app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "execute_merge must admit the PrMerge slot for the precheck phase",
    );
    assert!(
        app.merge_in_progress,
        "merge_in_progress must be set so the modal spinner renders during precheck",
    );
    assert!(
        app.is_merge_precheck_phase(),
        "spawn_merge_precheck must attach a PrMergePrecheck payload",
    );
    assert!(
        app.confirm_merge,
        "merge confirm modal must stay open across the precheck transition",
    );
}

/// `poll_merge_precheck` on a `Ready` message must hand off to
/// `perform_merge_after_precheck` without re-admitting the slot
/// and without surfacing any alert. The slot's payload must
/// transition from `PrMergePrecheck` to `PrMerge` (the precheck
/// receiver is dropped in the same step via the structural
/// `attach_user_action_payload` swap).
#[test]
fn poll_merge_precheck_ready_hands_off_without_alert() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/precheck-ready.json"));

    // Pre-admit the slot and set merge_in_progress, mirroring what
    // execute_merge does immediately before spawn_merge_precheck.
    app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
        .expect("helper admit should succeed in test setup");
    app.merge_in_progress = true;
    app.confirm_merge = true;
    app.merge_wi_id = Some(wi_id.clone());

    // Inject a Ready message via a synthetic channel - skips the
    // background thread entirely so the test is deterministic and
    // independent of any real `gh` binary.
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(MergePreCheckMessage::Ready {
        wi_id,
        strategy: "squash".into(),
        branch: "feature/precheck-ready".into(),
        repo_path: PathBuf::from("/tmp/precheck-ready-repo"),
        owner_repo: "owner/repo".into(),
    })
    .unwrap();
    app.attach_user_action_payload(
        &UserActionKey::PrMerge,
        UserActionPayload::PrMergePrecheck { rx },
    );

    app.poll_merge_precheck();

    assert!(
        !app.is_merge_precheck_phase(),
        "Ready hand-off must replace the precheck payload with the merge payload",
    );
    assert!(
        app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "Ready hand-off must keep the PrMerge slot reserved for the merge thread",
    );
    assert!(
        app.merge_in_progress,
        "merge_in_progress must stay true while the merge thread runs",
    );
    assert!(
        app.confirm_merge,
        "merge confirm modal must stay open while the merge thread runs",
    );
    assert!(
        app.alert_message.is_none(),
        "Ready hand-off must not surface an alert; got: {:?}",
        app.alert_message,
    );
}

/// `poll_merge_precheck` on a `Blocked` message must release the
/// slot (which structurally drops the precheck receiver), clear
/// the modal state, and surface the reason as an alert.
#[test]
fn poll_merge_precheck_blocked_releases_slot_and_alerts() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/precheck-blocked.json"));

    app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
        .expect("helper admit should succeed in test setup");
    app.merge_in_progress = true;
    app.confirm_merge = true;
    app.merge_wi_id = Some(wi_id);

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(MergePreCheckMessage::Blocked {
        reason: "Uncommitted changes. Commit & push before merging.".into(),
    })
    .unwrap();
    app.attach_user_action_payload(
        &UserActionKey::PrMerge,
        UserActionPayload::PrMergePrecheck { rx },
    );

    app.poll_merge_precheck();

    assert!(
        !app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "Blocked outcome must release the PrMerge slot",
    );
    assert!(
        !app.is_merge_precheck_phase(),
        "the precheck payload must be gone after Blocked",
    );
    assert!(!app.merge_in_progress);
    assert!(!app.confirm_merge);
    assert!(app.merge_wi_id.is_none());
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Uncommitted changes"),
        "alert must surface the precheck reason; got: {msg}",
    );
}

/// Regression: `retreat_stage` must drop the in-flight precheck
/// receiver in the same step as releasing the `PrMerge` slot.
/// With the structural-ownership refactor, the receiver lives
/// inside `UserActionPayload::PrMergePrecheck`, so dropping the
/// helper entry via `end_user_action` automatically drops the
/// receiver. This test pins that contract: after `retreat_stage`,
/// `is_merge_precheck_phase()` must return false even though no
/// sibling cleanup line exists in the production code.
#[test]
fn retreat_stage_drops_merge_precheck_payload() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/retreat-drops-precheck");
    let branch = "feature/retreat-drops-precheck";
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        branch,
        Some(false),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/retreat-drops-precheck.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, branch);

    // Mirror the post-`execute_merge` precheck-phase state: slot
    // admitted, payload swapped to `PrMergePrecheck` with a
    // never-completing receiver.
    app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
        .expect("helper admit should succeed in test setup");
    app.merge_in_progress = true;
    app.confirm_merge = true;
    app.merge_wi_id = Some(wi_id.clone());
    let (_tx_keep_alive, rx) = crossbeam_channel::bounded::<MergePreCheckMessage>(1);
    app.attach_user_action_payload(
        &UserActionKey::PrMerge,
        UserActionPayload::PrMergePrecheck { rx },
    );

    app.retreat_stage();

    assert!(
        !app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "retreat must release the PrMerge slot",
    );
    assert!(
        !app.is_merge_precheck_phase(),
        "releasing the slot must structurally drop the precheck payload",
    );
    assert!(!app.merge_in_progress);
    assert!(!app.confirm_merge);
    assert!(app.merge_wi_id.is_none());
}
