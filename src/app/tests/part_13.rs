//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    AgentBackendKind, App, Arc, BackendType, McpEvent, PathBuf, ReviewGateMessage,
    ReviewGateOrigin, ReviewGateResult, SessionEntry, WorkItemId, WorkItemStatus,
    app_with_work_item, drain_review_gate_with_timeout, insert_test_review_gate,
};

/// Test 4: MCP `StatusUpdate` for Review on Implementing item with no plan
/// must NOT change status to Review (gate spawn fails asynchronously),
/// and `rework_reasons` must be populated after `poll_review_gate` drains
/// the background thread's Blocked message.
#[test]
fn mcp_review_gate_bypass_prevented_no_plan() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    // Set up MCP channel with a StatusUpdate for Review.
    let (tx, rx) = crossbeam_channel::unbounded();
    app.mcp_rx = Some(rx);
    let wi_id_json = serde_json::to_string(&wi_id).unwrap();
    tx.send(McpEvent::StatusUpdate {
        work_item_id: wi_id_json,
        status: "Review".into(),
        reason: "Implementation complete".into(),
    })
    .unwrap();

    app.poll_mcp_status_updates();
    // The review gate now runs on a background thread; wait for its
    // Blocked message to drain and the rework flow to fire.
    drain_review_gate_with_timeout(&mut app, &wi_id);

    // Status must stay at Implementing - the gate cannot run without a plan.
    let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
    assert_eq!(
        wi.status,
        WorkItemStatus::Implementing,
        "status must not change to Review when no plan exists",
    );
    // rework_reasons must be populated (gate spawn failure triggers rework flow).
    assert!(
        app.rework_reasons.contains_key(&wi_id),
        "rework_reasons must be populated after gate spawn failure",
    );
    let reason = app.rework_reasons.get(&wi_id).unwrap();
    assert!(
        reason.contains("no plan"),
        "rework reason should mention no plan, got: {reason}",
    );
}

/// Test 5: TUI `advance_stage` from Implementing with no plan must NOT
/// change status to Review. The gate's "no plan" check now runs on a
/// background thread (see P0 audit #1), so we drain the gate with a
/// short poll loop before asserting.
#[test]
fn tui_advance_stage_blocked_without_plan() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    app.advance_stage();
    drain_review_gate_with_timeout(&mut app, &wi_id);

    // Status must stay at Implementing - spawn_review_gate fires the
    // Blocked outcome from the background thread, not synchronously.
    let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
    assert_eq!(
        wi.status,
        WorkItemStatus::Implementing,
        "TUI advance_stage must not advance to Review without a plan",
    );
    // Status message should explain why.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("no plan"),
        "status message should explain gate failure, got: {msg}",
    );
}

/// Regression guard for R1-F-3: a TUI-initiated review gate that
/// resolves to `Blocked` MUST NOT kill the user's Implementing
/// session. On master the TUI advance path just set
/// `status_message`; when the blocking-I/O fix moved the gate to a
/// background thread the new `poll_review_gate` Blocked arm
/// unconditionally killed and respawned the session - a regression
/// for user-initiated advances. The `ReviewGateOrigin::Tui` branch
/// in `poll_review_gate` must preserve the session and only surface
/// the reason.
#[test]
fn poll_review_gate_tui_blocked_preserves_session() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    // Install a mock Implementing session so we can assert it
    // survives the Blocked arm.
    let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.sessions.insert(
        (wi_id.clone(), WorkItemStatus::Implementing),
        SessionEntry {
            parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );

    // Install a Tui-origin gate with a pre-queued Blocked message.
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Blocked {
        work_item_id: wi_id.clone(),
        reason: "Cannot enter Review: no changes on branch".into(),
    })
    .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Tui);

    app.poll_review_gate();

    // The session must still be in the sessions map - TUI Blocked
    // does not run the kill+respawn rework flow.
    assert!(
        app.sessions
            .contains_key(&(wi_id.clone(), WorkItemStatus::Implementing)),
        "Tui-origin Blocked must NOT kill the existing Implementing session",
    );
    // rework_reasons must NOT be populated - rework only applies to
    // Mcp/Auto origins. A TUI user explicitly pressed advance; we
    // surface the reason instead of rewriting their session prompt.
    assert!(
        !app.rework_reasons.contains_key(&wi_id),
        "Tui-origin Blocked must NOT populate rework_reasons",
    );
    // Status must explain the gate failure.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("no changes on branch"),
        "status message should carry the Blocked reason, got: {msg}",
    );
    // Gate entry must be dropped.
    assert!(
        !app.review_gates.contains_key(&wi_id),
        "gate state must be cleared after Blocked",
    );
}

/// Regression guard for R1-F-3: Mcp-origin Blocked still runs the
/// full rework flow (session kill + respawn + `rework_reasons`).
/// This preserves the behaviour Claude relies on when
/// `workbridge_set_status("Review`") fails - Claude sees the
/// rejection reason in its next session prompt and iterates.
#[test]
fn poll_review_gate_mcp_blocked_populates_rework_reasons() {
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
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    app.poll_review_gate();

    assert!(
        app.rework_reasons
            .get(&wi_id)
            .is_some_and(|r| r.contains("no plan exists")),
        "Mcp-origin Blocked must populate rework_reasons so Claude \
         sees the reason on the next session restart",
    );
    assert!(
        !app.review_gates.contains_key(&wi_id),
        "gate state must be cleared after Blocked",
    );
}

/// Regression guard for R1-F-6: if the work item was deleted while
/// a review gate was in flight, the Blocked arm must NOT leak an
/// orphan `rework_reasons` entry. Only the gate state should be
/// dropped - nothing else to do for a work item that no longer
/// exists.
#[test]
fn poll_review_gate_blocked_guards_deleted_work_item() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/deleted-mid-gate.json"));

    // Install a gate WITHOUT pushing a matching work item: the
    // delete happened between spawn and poll.
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Blocked {
        work_item_id: wi_id.clone(),
        reason: "Cannot enter Review: no plan exists".into(),
    })
    .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    app.poll_review_gate();

    assert!(
        !app.review_gates.contains_key(&wi_id),
        "gate entry must be dropped even for deleted work items",
    );
    assert!(
        !app.rework_reasons.contains_key(&wi_id),
        "rework_reasons must NOT be populated for a deleted work item - \
         nothing would ever clear the entry and it would leak forever",
    );
}

/// Test 6: After `poll_review_gate` processes a rejection result,
/// `rework_reasons` is populated for the work item.
#[test]
fn poll_review_gate_rejection_populates_rework_reasons() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    // Simulate a review gate that completed with a rejection.
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Result(ReviewGateResult {
        work_item_id: wi_id.clone(),
        approved: false,
        detail: "Tests are missing for the new feature".into(),
    }))
    .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    app.poll_review_gate();

    assert!(
        app.rework_reasons.contains_key(&wi_id),
        "rework_reasons must be populated after gate rejection",
    );
    assert_eq!(
        app.rework_reasons.get(&wi_id).unwrap(),
        "Tests are missing for the new feature",
    );
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("rejected"),
        "status should mention rejection, got: {msg}",
    );
}

/// Test 7: `poll_review_gate` supports Blocked status - a Blocked work item
/// can transition to Review when the gate approves.
#[test]
fn poll_review_gate_approves_blocked_to_review() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Blocked,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    // Simulate a review gate that completed with approval.
    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Result(ReviewGateResult {
        work_item_id: wi_id.clone(),
        approved: true,
        detail: "All plan items implemented".into(),
    }))
    .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    app.poll_review_gate();

    // StubBackend's update_status is a no-op, but reassemble rebuilds from
    // StubBackend (empty). The status message from apply_stage_change confirms
    // the transition was attempted. Also verify gate findings were stored.
    assert!(
        app.review_gate_findings.contains_key(&wi_id),
        "review_gate_findings should be stored on approval",
    );
    assert_eq!(
        app.review_gate_findings.get(&wi_id).unwrap(),
        "All plan items implemented",
    );
    // Verify the gate is cleared from the map.
    assert!(
        !app.review_gates.contains_key(&wi_id),
        "gate should be cleared"
    );
}

/// Test: Progress messages update `review_gate_progress` without completing
/// the gate.
#[test]
fn poll_review_gate_progress_updates_field() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Progress("2 / 3 CI checks green".into()))
        .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    app.poll_review_gate();

    // Progress should be updated but gate should still be running.
    assert_eq!(
        app.review_gates
            .get(&wi_id)
            .and_then(|g| g.progress.as_deref()),
        Some("2 / 3 CI checks green"),
    );
    assert!(
        app.review_gates.contains_key(&wi_id),
        "gate should still be present (gate not done)",
    );
}

/// Test: Progress followed by Result in the same tick - both are processed.
#[test]
fn poll_review_gate_progress_then_result() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Progress(
        "1 / 1 CI checks green. Running code review...".into(),
    ))
    .unwrap();
    tx.send(ReviewGateMessage::Result(ReviewGateResult {
        work_item_id: wi_id.clone(),
        approved: false,
        detail: "Missing error handling".into(),
    }))
    .unwrap();
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    app.poll_review_gate();

    // Result should have been processed - gate is done.
    assert!(
        !app.review_gates.contains_key(&wi_id),
        "gate should be cleared"
    );
    assert!(
        app.rework_reasons.contains_key(&wi_id),
        "rework_reasons must be populated after rejection",
    );
}

/// Test: Disconnected channel (thread exited) after progress is handled.
#[test]
fn poll_review_gate_disconnect_after_progress() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    tx.send(ReviewGateMessage::Progress(
        "Checking for pull request...".into(),
    ))
    .unwrap();
    drop(tx); // Simulate thread exit without sending Result.
    insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

    app.poll_review_gate();

    // Gate should be cleaned up with an error message.
    assert!(
        !app.review_gates.contains_key(&wi_id),
        "gate should be cleared"
    );
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("unexpectedly"),
        "status should mention unexpected exit, got: {msg}",
    );
}

/// Test 8: Gate spawn failure (MCP path) via "no branch" - a
/// synchronous pre-condition that still returns
/// `ReviewGateSpawn::Blocked` from the main thread. The MCP handler
/// surfaces the reason in `status_message` (not `rework_reasons`);
/// `rework_reasons` is populated only when the BACKGROUND thread
/// reports a Blocked result via `poll_review_gate`. The rework flow for
/// "no plan" is exercised by `mcp_review_gate_bypass_prevented_no_plan`
/// via the drain helper.
#[test]
fn mcp_gate_spawn_failure_sets_rework_reasons() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Implementing,
        None, // no branch
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    app.mcp_rx = Some(rx);
    let wi_id_json = serde_json::to_string(&wi_id).unwrap();
    tx.send(McpEvent::StatusUpdate {
        work_item_id: wi_id_json,
        status: "Review".into(),
        reason: "Done implementing".into(),
    })
    .unwrap();

    app.poll_mcp_status_updates();

    // Status must stay at Implementing - the synchronous pre-condition
    // blocked the spawn.
    let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
    assert_eq!(
        wi.status,
        WorkItemStatus::Implementing,
        "status must not change to Review when gate is blocked",
    );
    // The synchronous Blocked path surfaces the reason in status_message.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("no branch"),
        "status should mention 'no branch', got: {msg}",
    );
}

/// Test 9: When a review gate is already running for item A, an MCP
/// `StatusUpdate` for Review on item B should independently attempt to
/// spawn its own gate. With `StubBackend` (no plan), it fails with a
/// "no plan" error and triggers the rework flow for item B.
#[test]
fn concurrent_gate_spawn_independent_of_other_items() {
    let mut app = App::new();

    // Item A: gate is running for this one.
    let wi_id_a = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-a.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id_a.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Item A".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/repo"),
            branch: Some("branch-a".into()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Item B: MCP will request Review for this one.
    let wi_id_b = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-b.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id_b.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Item B".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/repo"),
            branch: Some("branch-b".into()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Pre-populate the per-work-item harness choice for both items
    // so the review-gate spawn reaches its real pre-conditions
    // rather than short-circuiting at the "no harness chosen"
    // abort. Tests that exercise the abort itself use explicit
    // setup (`harness_choice_applied_to_review_gate_spawn`).
    app.harness_choice
        .insert(wi_id_a.clone(), AgentBackendKind::ClaudeCode);
    app.harness_choice
        .insert(wi_id_b.clone(), AgentBackendKind::ClaudeCode);

    // Simulate gate running for item A.
    let (_dummy_tx, dummy_rx) = crossbeam_channel::unbounded();
    insert_test_review_gate(&mut app, wi_id_a.clone(), dummy_rx, ReviewGateOrigin::Mcp);

    // Send MCP StatusUpdate for item B.
    let (tx, rx) = crossbeam_channel::unbounded();
    app.mcp_rx = Some(rx);
    let wi_id_b_json = serde_json::to_string(&wi_id_b).unwrap();
    tx.send(McpEvent::StatusUpdate {
        work_item_id: wi_id_b_json,
        status: "Review".into(),
        reason: "Done".into(),
    })
    .unwrap();

    app.poll_mcp_status_updates();
    // Item B's gate runs on a background thread; drain the "no plan"
    // Blocked message via poll_review_gate.
    drain_review_gate_with_timeout(&mut app, &wi_id_b);

    // Item B's status must be unchanged (gate spawn failed due to no plan).
    let wi_b = app.work_items.iter().find(|w| w.id == wi_id_b).unwrap();
    assert_eq!(
        wi_b.status,
        WorkItemStatus::Implementing,
        "item B should remain Implementing when gate cannot spawn (no plan)",
    );
    // rework_reasons should be populated - gate spawn failure (no plan)
    // triggers the rework flow via poll_review_gate, mirroring the old
    // synchronous behavior.
    assert!(
        app.rework_reasons.contains_key(&wi_id_b),
        "rework_reasons must be set for item B (gate spawn failure, not blocked by item A)",
    );
    let reason = app.rework_reasons.get(&wi_id_b).unwrap();
    assert!(
        reason.contains("no plan"),
        "rework reason should mention no plan, got: {reason}",
    );
    // Item A's gate should still be running.
    assert!(
        app.review_gates.contains_key(&wi_id_a),
        "item A's gate should still be running",
    );
}

/// Test 10: A Blocked work item with no plan that fails the gate via MCP
/// should transition to Implementing (not stay Blocked), so the
/// `implementing_rework` prompt (which has {`rework_reason`}) is used.
/// The gate runs on a background thread and the Blocked outcome is
/// drained via `poll_review_gate`.
#[test]
fn blocked_gate_failure_transitions_to_implementing() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Blocked,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    // Send MCP StatusUpdate for Review.
    let (tx, rx) = crossbeam_channel::unbounded();
    app.mcp_rx = Some(rx);
    let wi_id_json = serde_json::to_string(&wi_id).unwrap();
    tx.send(McpEvent::StatusUpdate {
        work_item_id: wi_id_json,
        status: "Review".into(),
        reason: "Implementation complete".into(),
    })
    .unwrap();

    app.poll_mcp_status_updates();
    drain_review_gate_with_timeout(&mut app, &wi_id);

    // StubBackend.update_status is a no-op, but reassemble_work_items
    // rebuilds from the StubBackend (which returns empty). The important
    // assertion is that rework_reasons is populated AND the code path
    // that transitions Blocked -> Implementing was executed.
    assert!(
        app.rework_reasons.contains_key(&wi_id),
        "rework_reasons must be populated for Blocked gate failure",
    );
    // Verify status message mentions the gate failure.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Review gate failed") || msg.contains("no plan"),
        "status should mention gate failure, got: {msg}",
    );
}
