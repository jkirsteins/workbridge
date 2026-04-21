//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

#[test]
fn stage_transition_without_harness_choice_surfaces_error() {
    // Regression guard for the CLAUDE.md [ABSOLUTE] rule that
    // silent fallbacks to a default harness are P0. Previously
    // `spawn_session` -> `begin_session_open` -> `finish_session_open`
    // would resolve the per-work-item backend via
    // `backend_for_work_item(id).unwrap_or_else(|| self.agent_backend)`
    // which silently ran Claude against the user's code even when
    // they had never picked a harness (or picked Codex and lost
    // the choice on restart). The fix is an abort-with-toast at
    // `begin_session_open` that matches `spawn_review_gate` and
    // `spawn_rebase_gate`.
    //
    // The test exercises `begin_session_open` directly rather
    // than `apply_stage_change` because `apply_stage_change`
    // reassembles `self.work_items` from the backend's `list()`
    // mid-call (the stage-change writes through to storage and
    // then re-reads), and the test-only `CountingPlanBackend`
    // does not persist items. `begin_session_open` is the
    // function that actually holds the abort check under test;
    // the stage-change + auto-spawn chain is pinned separately
    // by `apply_stage_change_cancels_pending_session_open`.
    let backend = Arc::new(CountingPlanBackend::default());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/no-harness-stage-change.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "no-harness-stage-change".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/no-harness-stage-change-repo"),
            branch: Some("feature/no-harness".into()),
            worktree_path: Some(PathBuf::from("/tmp/no-harness-stage-change-wt")),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // No harness_choice inserted on purpose.
    assert!(!app.harness_choice.contains_key(&wi_id));

    let cwd = PathBuf::from("/tmp/no-harness-stage-change-wt");
    app.begin_session_open(&wi_id, &cwd);

    // The [ABSOLUTE] rule: no silent substitution. The session
    // MUST NOT have been opened - no pending receiver, no spawn
    // receiver, no spinner.
    assert!(
        !app.session_open_rx.contains_key(&wi_id),
        "session open must be aborted when harness_choice is unset; \
         silently falling back to agent_backend violates the [ABSOLUTE] \
         'no silent fallback' rule in CLAUDE.md"
    );
    assert!(
        !app.session_spawn_rx.contains_key(&wi_id),
        "aborted session-open must not reach the Phase 2 spawn receiver"
    );
    assert!(
        app.activities.current().is_none(),
        "aborted session-open must not leave a spinner behind"
    );

    // The abort MUST be visible to the user. A toast advertising
    // the c / x recovery path satisfies the CLAUDE.md "explicit
    // error on unresolvable intent" requirement.
    let all_toasts: Vec<String> = app.toasts.iter().map(|t| t.text.clone()).collect();
    let toast_text = app
        .toasts
        .iter()
        .find(|t| t.text.contains("no harness chosen"))
        .map_or_else(
            || {
                panic!(
                    "abort must surface a user-visible toast; toasts were {all_toasts:?}, \
                 status_message: {:?}",
                    app.status_message
                )
            },
            |t| t.text.clone(),
        );
    assert!(
        toast_text.contains("c / x"),
        "toast must name the recovery keybinding, got: {toast_text}"
    );
}

/// A `WorktreeService` whose `remove_worktree` always fails. Used to
/// verify that `spawn_orphan_worktree_cleanup` surfaces failures
/// through the per-spawn `OrphanCleanupFinished` completion message
/// instead of dropping them.
#[cfg(test)]
pub struct FailingRemoveWorktreeService;

#[cfg(test)]
impl WorktreeService for FailingRemoveWorktreeService {
    fn list_worktrees(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<Vec<crate::worktree_service::WorktreeInfo>, crate::worktree_service::WorktreeError>
    {
        Ok(Vec::new())
    }

    fn create_worktree(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
        _target_dir: &std::path::Path,
    ) -> Result<crate::worktree_service::WorktreeInfo, crate::worktree_service::WorktreeError> {
        Err(crate::worktree_service::WorktreeError::GitError(
            "unsupported in this stub".into(),
        ))
    }

    fn remove_worktree(
        &self,
        _repo_path: &std::path::Path,
        _worktree_path: &std::path::Path,
        _delete_branch: bool,
        _force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Err(crate::worktree_service::WorktreeError::GitError(
            "simulated remove failure".into(),
        ))
    }

    fn delete_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
        _force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Err(crate::worktree_service::WorktreeError::GitError(
            "simulated branch delete failure".into(),
        ))
    }

    fn default_branch(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<String, crate::worktree_service::WorktreeError> {
        Ok("main".to_string())
    }

    fn github_remote(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<Option<(String, String)>, crate::worktree_service::WorktreeError> {
        Ok(None)
    }

    fn fetch_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }

    fn create_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }
    fn prune_worktrees(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }
}

#[test]
fn spawn_orphan_worktree_cleanup_surfaces_failures_via_status_message() {
    // Codex finding: `spawn_orphan_worktree_cleanup` previously
    // discarded `remove_worktree` and `delete_branch` errors with
    // `let _ = ...`, leaving leaked worktrees/branches with no
    // user-visible warning. The fix routes failures through the
    // per-spawn `OrphanCleanupFinished` completion message so
    // `poll_orphan_cleanup_finished` can surface them in the status
    // bar AND end the matching status-bar activity.
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend) as Arc<dyn WorkItemBackend>,
        Arc::new(FailingRemoveWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );

    // Drain any pre-existing status message so the assertion
    // below is unambiguous.
    app.status_message = None;

    app.spawn_orphan_worktree_cleanup(
        PathBuf::from("/tmp/codex-orphan-repo"),
        PathBuf::from("/tmp/codex-orphan-repo/.worktrees/feature/codex-orphan"),
        Some("feature/codex-orphan".into()),
    );

    // Spawning must register a status-bar activity per
    // `docs/UI.md` "Activity indicator placement".
    assert!(
        app.activities.current().is_some(),
        "spawn_orphan_worktree_cleanup must register a status-bar activity",
    );

    // Wait for the single completion message to land in the channel.
    let recv_start = crate::side_effects::clock::instant_now();
    loop {
        if !app.orphan_cleanup_finished_rx.is_empty() {
            break;
        }
        // 60s of mock-clock budget (6000 iterations of the 10ms
        // mock `sleep`) to absorb OS-scheduler jitter on loaded CI
        // hosts. `clock::sleep` is pure `yield_now` in tests, so
        // each iteration is only a few hundred microseconds of
        // real time - 6000 yields gives the background thread
        // ample real-time opportunity to make progress while the
        // mock clock advances. A true livelock still trips this
        // cap deterministically.
        if crate::side_effects::clock::elapsed_since(recv_start)
            > std::time::Duration::from_secs(60)
        {
            panic!("orphan cleanup background thread did not enqueue completion message");
        }
        crate::side_effects::clock::sleep(std::time::Duration::from_millis(10));
    }

    app.poll_orphan_cleanup_finished();

    let msg = app
        .status_message
        .as_ref()
        .expect("poll_orphan_cleanup_finished must surface a status message");
    assert!(
        msg.contains("Orphan worktree cleanup failed"),
        "status message must mention the worktree failure, got: {msg}",
    );
    assert!(
        msg.contains("Orphan branch cleanup failed"),
        "status message must mention the branch failure, got: {msg}",
    );
    assert!(
        msg.contains("feature/codex-orphan"),
        "status message must include the branch name, got: {msg}",
    );
    assert!(
        app.activities.current().is_none(),
        "poll_orphan_cleanup_finished must end the spawned activity even on failure",
    );
}

#[test]
fn poll_orphan_cleanup_finished_is_silent_on_idle_channel() {
    // The idle path: an empty channel means no cleanup has finished;
    // `poll_orphan_cleanup_finished` must NOT clobber an unrelated
    // status message and must NOT touch any activity.
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    app.status_message = Some("unrelated status message".into());

    app.poll_orphan_cleanup_finished();

    assert_eq!(
        app.status_message.as_deref(),
        Some("unrelated status message"),
        "empty completion channel must not clobber unrelated status messages",
    );
}

#[test]
fn spawn_orphan_worktree_cleanup_ends_activity_on_success() {
    // Success path: the cleanup closure runs against `StubWorktreeService`
    // (whose `remove_worktree` / `delete_branch` succeed), sends an
    // `OrphanCleanupFinished` with no warnings, and the poll
    // function must end the registered status-bar activity without
    // touching `status_message`.
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    app.status_message = None;

    app.spawn_orphan_worktree_cleanup(
        PathBuf::from("/tmp/orphan-success-repo"),
        PathBuf::from("/tmp/orphan-success-repo/.worktrees/feature/orphan-success"),
        Some("feature/orphan-success".into()),
    );

    assert!(
        app.activities.current().is_some(),
        "spawn_orphan_worktree_cleanup must register a status-bar activity",
    );

    // Wait for the single completion message to arrive.
    let recv_start = crate::side_effects::clock::instant_now();
    loop {
        if !app.orphan_cleanup_finished_rx.is_empty() {
            break;
        }
        // 60s of mock-clock budget (6000 iterations of the 10ms
        // mock `sleep`) to absorb OS-scheduler jitter on loaded CI
        // hosts. `clock::sleep` is pure `yield_now` in tests, so
        // each iteration is only a few hundred microseconds of
        // real time - 6000 yields gives the background thread
        // ample real-time opportunity to make progress while the
        // mock clock advances. A true livelock still trips this
        // cap deterministically.
        if crate::side_effects::clock::elapsed_since(recv_start)
            > std::time::Duration::from_secs(60)
        {
            panic!("orphan cleanup background thread did not enqueue completion message");
        }
        crate::side_effects::clock::sleep(std::time::Duration::from_millis(10));
    }

    app.poll_orphan_cleanup_finished();

    assert!(
        app.activities.current().is_none(),
        "poll_orphan_cleanup_finished must end the spawned activity on success",
    );
    assert!(
        app.status_message.is_none(),
        "successful orphan cleanup must not set status_message, got {:?}",
        app.status_message,
    );
}

#[test]
fn cleanup_session_state_ends_spinner_for_pending_open() {
    // Regression guard for R2-F-3's symmetric cleanup path:
    // `cleanup_session_state_for` is called when a work item is
    // deleted mid-open. It must route through
    // `drop_session_open_entry` so the spinner is not leaked.
    let backend = Arc::new(CountingPlanBackend::default());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/r2f3-cleanup.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "r2f3-cleanup".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/r2f3-cleanup-repo"),
            branch: Some("feature/r2f3c".into()),
            worktree_path: Some(PathBuf::from("/tmp/r2f3-cleanup-wt")),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Record a harness choice so `begin_session_open` does not
    // short-circuit on the "no harness chosen" abort.
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);

    let cwd = PathBuf::from("/tmp/r2f3-cleanup-wt");
    app.begin_session_open(&wi_id, &cwd);
    assert!(app.activities.current().is_some());

    // Delete-flavour cleanup: spinner must be cleared.
    app.cleanup_session_state_for(&wi_id);
    assert!(
        app.activities.current().is_none(),
        "cleanup_session_state_for must end the session-open spinner",
    );
    assert!(
        !app.session_open_rx.contains_key(&wi_id),
        "pending session-open entry must be removed on cleanup",
    );
}

/// Gap 1 regression: `drain_pr_identity_backfill` must end the
/// status-bar activity AND clear the receiver on the Disconnected
/// branch. The activity is started in `salsa.rs::app_init` when the
/// backfill request set is non-empty; the only terminal state for
/// that one-shot stream is sender-dropped (background thread done),
/// so `drain_pr_identity_backfill` is the sole place the activity
/// can be ended without leaking a spinner.
#[test]
fn drain_pr_identity_backfill_ends_activity_on_disconnect() {
    let mut app = App::new();

    // Manually wire a disconnected channel + a registered activity:
    // create the channel, drop the tx half so the next try_recv
    // returns Disconnected, store the rx on App and start the
    // matching status-bar activity.
    let (tx, rx) =
        crossbeam_channel::unbounded::<Result<crate::app::PrIdentityBackfillResult, String>>();
    drop(tx);
    app.pr_identity_backfill_rx = Some(rx);
    let aid = app.activities.start("Backfilling merged PR identities...");
    app.pr_identity_backfill_activity = Some(aid);

    let changed = app.drain_pr_identity_backfill();

    assert!(
        !changed,
        "no Ok messages were sent so changed must be false",
    );
    assert!(
        app.pr_identity_backfill_rx.is_none(),
        "Disconnected branch must drop the receiver",
    );
    assert!(
        app.pr_identity_backfill_activity.is_none(),
        "Disconnected branch must take the ActivityId",
    );
    assert!(
        app.activities.current().is_none(),
        "drain_pr_identity_backfill must end the status-bar activity \
         on Disconnected so the spinner does not leak",
    );
}

/// Gap 3 regression: the disconnected arm of `poll_review_gate`
/// must end the review gate's status-bar activity. Routing through
/// `drop_review_gate` is the structural guarantee.
#[test]
fn poll_review_gate_disconnect_ends_status_bar_activity() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    // Drop the tx half so the next try_recv yields Disconnected.
    let (tx, rx) = crossbeam_channel::unbounded::<ReviewGateMessage>();
    drop(tx);
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    assert!(
        app.activities.current().is_some(),
        "test gate must register an activity to begin with",
    );

    app.poll_review_gate();

    assert!(
        !app.review_gates.contains_key(&wi_id),
        "Disconnected gate must be dropped",
    );
    assert!(
        app.activities.current().is_none(),
        "Disconnected arm of poll_review_gate must end the gate activity",
    );
}

/// Gap 3 regression: the Blocked arm of `poll_review_gate` must
/// end the review gate's status-bar activity. Use a Tui origin so
/// the test does not need a live session map - the Tui branch only
/// surfaces the reason and drops the gate, which is exactly what
/// the test wants to observe.
#[test]
fn poll_review_gate_blocked_ends_status_bar_activity() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Blocked {
        work_item_id: wi_id.clone(),
        reason: "Cannot enter Review: no plan exists".into(),
    })
    .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Tui);

    assert!(app.activities.current().is_some());

    app.poll_review_gate();

    assert!(
        !app.review_gates.contains_key(&wi_id),
        "Blocked gate must be dropped",
    );
    assert!(
        app.activities.current().is_none(),
        "Blocked arm of poll_review_gate must end the gate activity",
    );
}

/// Gap 3 regression: the Result arm of `poll_review_gate` must
/// end the review gate's status-bar activity, both for the approve
/// path and the reject path.
///
/// The reject path additionally kills and respawns the session,
/// which starts its own "Opening session..." activity - so we
/// cannot assert that `activities.current()` is None after polling.
/// Instead, we capture the gate's `ActivityId` before polling and
/// verify that exact ID is no longer in `app.activities`.
#[test]
fn poll_review_gate_result_ends_status_bar_activity_reject() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Result(ReviewGateResult {
        work_item_id: wi_id.clone(),
        approved: false,
        detail: "missing tests".into(),
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
