//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// Regression guard for the side-car file leak on cancellation.
/// `cancel_session_open_entry` must drain the worker's
/// `committed_files` mutex into the cleanup call so files the
/// worker already wrote are removed even when the worker's
/// `tx.send(...)` is silently dropped against a closed receiver.
/// Pre-fix the path was carried only via the `written_files` Vec
/// inside `SessionOpenPlanResult`, which got discarded along with
/// the result on a cancellation race.
#[test]
fn cancel_session_open_entry_cleans_committed_side_car_files() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-cancel-cleanup.json"));

    // Create two real tempfiles that mimic the side-car files
    // the worker would have written by the time the user
    // cancels. Use tempfile's unique-name tempdir so parallel
    // test threads cannot collide on a shared fixed path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let tmp_root = tmp.path();
    let mcp_config_path = tmp_root.join("workbridge-cancel-test-mcp.json");
    let side_car_path = tmp_root.join("workbridge-cancel-test-sidecar.json");
    std::fs::write(&mcp_config_path, b"{}").expect("create mcp_config tempfile");
    std::fs::write(&side_car_path, b"{}").expect("create side-car tempfile");

    // Synthesize a SessionOpenPending entry that looks like a
    // worker mid-flight after Phase C (side-car file written
    // and pushed into `committed_files`).
    let cancelled = Arc::new(AtomicBool::new(false));
    let committed_files = Arc::new(Mutex::new(vec![side_car_path.clone()]));
    let (_tx, rx) = crossbeam_channel::bounded::<SessionOpenPlanResult>(1);
    let activity = app.activities.start("Opening session...");
    app.session_open_rx.insert(
        wi_id.clone(),
        SessionOpenPending {
            rx,
            activity,
            cancelled: Arc::clone(&cancelled),
            mcp_config_path: mcp_config_path.clone(),
            committed_files: Arc::clone(&committed_files),
        },
    );

    // Cancel the entry. This should:
    // 1. Set the cancelled flag.
    // 2. Drain committed_files into the cleanup call.
    // 3. Push mcp_config_path into the same cleanup call.
    // 4. Schedule a background `spawn_agent_file_cleanup`.
    app.cancel_session_open_entry(&wi_id);

    assert!(
        cancelled.load(Ordering::Acquire),
        "cancel_session_open_entry must set the cancelled flag",
    );
    assert!(
        committed_files.lock().unwrap().is_empty(),
        "cancel_session_open_entry must drain the committed_files mutex",
    );
    assert!(
        !app.session_open_rx.contains_key(&wi_id),
        "cancel_session_open_entry must remove the pending entry",
    );

    // The cleanup runs on a detached background thread; poll
    // until both files are gone or the bounded timeout elapses.
    wait_until_file_removed(&mcp_config_path, std::time::Duration::from_secs(5));
    wait_until_file_removed(&side_car_path, std::time::Duration::from_secs(5));
    assert!(
        !mcp_config_path.exists(),
        "spawn_agent_file_cleanup should have removed the temp \
         --mcp-config file",
    );
    assert!(
        !side_car_path.exists(),
        "spawn_agent_file_cleanup should have removed the side-car \
         file the worker pushed into committed_files",
    );
}

/// Companion to `flush_pty_buffers_preserves_global_bytes_when_no_session`:
/// once a live `global_session` exists, the next
/// `flush_pty_buffers` MUST drain the buffered bytes. Without
/// this, the keystrokes parked during the async session-open
/// window would never reach the PTY.
#[test]
fn flush_pty_buffers_drains_global_bytes_once_session_alive() {
    let mut app = App::new();

    // Stash bytes that accumulated during the in-flight open.
    app.buffer_bytes_to_global(b"hello");

    // Install a SessionEntry with no actual `Session` handle
    // (PTY-less, but `alive == true`). The send path will
    // observe `session.is_none()` and skip the write, but
    // crucially the gate in `flush_pty_buffers` requires
    // `session.is_some()`, so the buffer should still NOT be
    // drained in this half-installed state. This guards
    // against a regression where the gate is too loose.
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.global_session = Some(SessionEntry {
        parser,
        alive: true,
        session: None,
        scrollback_offset: 0,
        selection: None,
        agent_written_files: Vec::new(),
    });

    app.flush_pty_buffers();

    assert_eq!(
        app.pending_global_pty_bytes, b"hello",
        "flush_pty_buffers must keep the buffer when the session \
         entry exists but has no PTY handle",
    );
}

/// The close branch of `toggle_global_drawer` must run the teardown so
/// the next open starts from a blank slate. Exercising the close
/// branch directly (rather than round-tripping through the open
/// branch) avoids spawning a real `claude` subprocess in tests.
#[test]
fn toggle_global_drawer_close_tears_down_session() {
    let mut app = App::new();

    // Simulate a drawer that is already open with live state.
    app.global_drawer_open = true;
    app.pre_drawer_focus = app.focus;

    let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.global_session = Some(SessionEntry {
        parser,
        alive: true,
        session: None,
        scrollback_offset: 0,
        selection: None,
        agent_written_files: Vec::new(),
    });

    let tmp = tempfile::tempdir().expect("tempdir");
    let temp_path = tmp.path().join("workbridge-toggle-close-test.json");
    std::fs::write(&temp_path, b"{}").expect("create temp mcp config");
    app.global_mcp_config_path = Some(temp_path.clone());
    app.pending_global_pty_bytes.extend_from_slice(b"leftover");

    // Close branch: no spawn involved, so this is safe in any test env.
    app.toggle_global_drawer();

    assert!(!app.global_drawer_open, "drawer must be closed");
    assert!(
        app.global_session.is_none(),
        "close must clear global_session",
    );
    assert!(
        app.global_mcp_server.is_none(),
        "close must clear global_mcp_server",
    );
    assert!(
        app.global_mcp_config_path.is_none(),
        "close must clear global_mcp_config_path",
    );
    assert!(
        app.pending_global_pty_bytes.is_empty(),
        "close must drain pending_global_pty_bytes",
    );
    // Mirrors the `teardown_global_session_clears_all_state`
    // wait: file removal runs on a detached background thread.
    wait_until_file_removed(&temp_path, std::time::Duration::from_secs(5));
    assert!(
        !temp_path.exists(),
        "close must delete the temp MCP config file \
         (via the background `spawn_agent_file_cleanup` worker)",
    );
}

// -- Feature: plan_from_branch (no-plan recovery) --

/// `plan_from_branch` accepts a Blocked item and applies the transition.
#[test]
fn plan_from_branch_accepts_blocked_item() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/plan-from-branch.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Plan from branch test".into(),
        description: None,
        status: WorkItemStatus::Blocked,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    app.plan_from_branch(&wi_id);

    // StubBackend persists nothing, so we verify via the status message
    // that apply_stage_change was called (it sets "Moved to [PL]").
    let msg = app.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("[PL]"),
        "should show Planning transition message, got: {msg}",
    );
}

/// `plan_from_branch` rejects a work item that is not Blocked.
#[test]
fn plan_from_branch_rejects_non_blocked() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/plan-not-blocked.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Not blocked test".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    app.plan_from_branch(&wi_id);

    // Item should remain unchanged - verify via status message.
    let msg = app.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("no longer blocked"),
        "should show informational message, got: {msg}",
    );
    // Work item should still be in original status.
    let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
    assert_eq!(
        wi.status,
        WorkItemStatus::Implementing,
        "should remain in Implementing when not Blocked",
    );
}

// -- Feature: BLOCKED sidebar group --

/// Blocked items appear in a BLOCKED group, not in ACTIVE.
#[test]
fn display_list_blocked_items_in_blocked_group() {
    let mut app = App::new();
    // Add one Blocked and one Implementing item.
    let blocked_id = WorkItemId::LocalFile(PathBuf::from("/tmp/blocked.json"));
    let active_id = WorkItemId::LocalFile(PathBuf::from("/tmp/active.json"));
    let repo = PathBuf::from("/repos/test");
    for (id, status) in [
        (blocked_id, WorkItemStatus::Blocked),
        (active_id, WorkItemStatus::Implementing),
    ] {
        app.work_items.push(crate::work_item::WorkItem {
            display_id: None,
            id,
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: format!("{status:?} item"),
            description: None,
            status,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: repo.clone(),
                branch: Some("test-branch".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            errors: vec![],
        });
    }

    app.build_display_list();

    let headers: Vec<&str> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::GroupHeader { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        headers.contains(&"BLOCKED (test)"),
        "should have BLOCKED group, got: {headers:?}",
    );
    assert!(
        headers.contains(&"ACTIVE (test)"),
        "should have ACTIVE group, got: {headers:?}",
    );
    // BLOCKED should come before ACTIVE.
    let blocked_pos = headers
        .iter()
        .position(|h| h.starts_with("BLOCKED"))
        .unwrap();
    let active_pos = headers
        .iter()
        .position(|h| h.starts_with("ACTIVE"))
        .unwrap();
    assert!(
        blocked_pos < active_pos,
        "BLOCKED group should come before ACTIVE",
    );
}

/// BLOCKED group header uses `GroupHeaderKind::Blocked`.
#[test]
fn display_list_blocked_header_kind() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/blocked-kind.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id,
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Blocked kind test".into(),
        description: None,
        status: WorkItemStatus::Blocked,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/repos/test"),
            branch: Some("branch".into()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    app.build_display_list();

    let blocked_header = app.display_list.iter().find(|e| {
        matches!(
            e,
            DisplayEntry::GroupHeader { label, .. } if label.starts_with("BLOCKED")
        )
    });
    assert!(blocked_header.is_some(), "should have BLOCKED header");
    if let Some(DisplayEntry::GroupHeader { kind, .. }) = blocked_header {
        assert_eq!(
            *kind,
            GroupHeaderKind::Blocked,
            "BLOCKED header should use Blocked kind"
        );
    }
}

// Blocked + Review auto-start rules are covered by
// `build_agent_cmd_blocked_no_auto_start` and
// `build_agent_cmd_review_with_findings_uses_review_auto_start` in
// this same module. Those tests go through the same code path the
// three spawn sites use.

/// Review-gate findings are stored per work item and influence prompt key.
#[test]
fn review_gate_findings_stored_per_work_item() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-findings.json"));
    app.review_gate_findings
        .insert(wi_id.clone(), "All plan items implemented correctly".into());

    assert_eq!(
        app.review_gate_findings
            .get(&wi_id)
            .map(std::string::String::as_str),
        Some("All plan items implemented correctly"),
    );
}

// -- Activity indicator tests --

#[test]
fn start_activity_returns_unique_ids() {
    let mut app = App::new();
    let id1 = app.activities.start("First");
    let id2 = app.activities.start("Second");
    assert_ne!(id1, id2);
    assert_eq!(app.activities.len(), 2);
}

#[test]
fn end_activity_removes_by_id() {
    let mut app = App::new();
    let id1 = app.activities.start("First");
    let id2 = app.activities.start("Second");
    app.activities.end(id1);
    assert_eq!(app.activities.len(), 1);
    assert_eq!(app.activities.current(), Some("Second"));
    app.activities.end(id2);
    assert!(app.activities.is_empty());
    assert_eq!(app.activities.current(), None);
}

#[test]
fn end_activity_noop_for_unknown_id() {
    let mut app = App::new();
    let id = app.activities.start("Test");
    app.activities.end(ActivityId(999));
    assert_eq!(app.activities.len(), 1);
    app.activities.end(id);
    assert!(app.activities.is_empty());
}

#[test]
fn current_activity_returns_last() {
    let mut app = App::new();
    assert_eq!(app.activities.current(), None);
    app.activities.start("First");
    assert_eq!(app.activities.current(), Some("First"));
    app.activities.start("Second");
    assert_eq!(app.activities.current(), Some("Second"));
}

#[test]
fn current_activity_pops_to_previous_on_end() {
    let mut app = App::new();
    let _id1 = app.activities.start("First");
    let id2 = app.activities.start("Second");
    app.activities.end(id2);
    assert_eq!(app.activities.current(), Some("First"));
}

#[test]
fn has_visible_status_bar_with_activity() {
    let mut app = App::new();
    assert!(!app.has_visible_status_bar());
    let id = app.activities.start("Working...");
    assert!(app.has_visible_status_bar());
    app.activities.end(id);
    assert!(!app.has_visible_status_bar());
}

#[test]
fn has_visible_status_bar_with_message() {
    let mut app = App::new();
    app.status_message = Some("test".into());
    assert!(app.has_visible_status_bar());
}

#[test]
fn has_visible_status_bar_activity_overrides_message() {
    let mut app = App::new();
    app.status_message = Some("test".into());
    let id = app.activities.start("Working...");
    assert!(app.has_visible_status_bar());
    // Activity takes precedence in rendering, but bar is visible either way.
    assert_eq!(app.activities.current(), Some("Working..."));
    app.activities.end(id);
    // Status message still keeps bar visible.
    assert!(app.has_visible_status_bar());
}

// -- Review gate regression tests --

/// Helper: create an App with a single work item at the given status,
/// with an optional repo association (branch + `repo_path`).
/// Poll `poll_review_gate` in a short busy loop until the review gate
/// for `wi_id` is no longer in-flight, or a short timeout elapses.
///
/// Tests that trigger `spawn_review_gate` via `MCP/advance_stage` need
/// this because the gate now runs on a real background thread - the
/// synchronous Blocked branch was removed to keep `git diff` off the
/// UI thread (see P0 audit #1). The background thread will immediately
/// send a `Blocked` message for stub-backend cases (no plan, etc.) so
/// the loop normally returns within a single millisecond.
pub fn drain_review_gate_with_timeout(app: &mut App, wi_id: &WorkItemId) {
    let deadline = crate::side_effects::clock::instant_now() + std::time::Duration::from_secs(5);
    while app.review_gates.contains_key(wi_id)
        && crate::side_effects::clock::instant_now() < deadline
    {
        app.poll_review_gate();
        if !app.review_gates.contains_key(wi_id) {
            return;
        }
        crate::side_effects::clock::sleep(std::time::Duration::from_millis(5));
    }
    // Final poll to catch any message that arrived during the last sleep.
    app.poll_review_gate();
}

/// Poll a state-flag that flips when a background worker thread
/// completes. The iteration cap matches `clock::bounded_recv`
/// (6000); we deliberately oversize the budget because mock
/// `clock::sleep` is pure `yield_now` under cfg(test) and the
/// worker thread may need many scheduler turns to run on a
/// single-core-ish CI host before the main thread's poll sees the
/// flag clear. On macOS 1000 iterations was empirically enough;
/// on Ubuntu CI runners 1000 is too tight and the loop panics
/// spuriously even though the worker is not actually wedged.
/// 6000 iterations absorbs that jitter while still bounding a
/// real livelock (the thread-local safety cap in
/// `side_effects::clock::sleep` would trip at `100_000` anyway).
pub fn drain_worktree_creation(app: &mut App) {
    for _ in 0..6_000 {
        app.poll_worktree_creation();
        if !app.is_user_action_in_flight(&UserActionKey::WorktreeCreate) {
            return;
        }
        crate::side_effects::clock::sleep(std::time::Duration::from_millis(1));
    }
    panic!("worktree creation did not finish");
}

pub fn drain_delete_cleanup(app: &mut App) {
    for _ in 0..6_000 {
        app.poll_delete_cleanup();
        if !app.delete_in_progress {
            return;
        }
        crate::side_effects::clock::sleep(std::time::Duration::from_millis(1));
    }
    panic!("background delete cleanup did not finish");
}

/// Test helper: insert a manually-constructed `ReviewGateState`
/// after starting a status-bar activity for it. Mirrors the
/// behaviour of `spawn_review_gate` so the production
/// `drop_review_gate` invariant (always end the activity on every
/// drop site) is exercised by the tests.
pub fn insert_test_review_gate(
    app: &mut App,
    wi_id: WorkItemId,
    rx: crossbeam_channel::Receiver<ReviewGateMessage>,
    origin: ReviewGateOrigin,
) {
    let activity = app.activities.start("test review gate");
    app.review_gates.insert(
        wi_id,
        ReviewGateState {
            rx,
            progress: None,
            origin,
            activity,
        },
    );
}

pub fn app_with_work_item(
    status: WorkItemStatus,
    branch: Option<&str>,
    repo_path: Option<&str>,
) -> (App, WorkItemId) {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-test.json"));
    let repo_assoc = repo_path.map_or_else(Vec::new, |rp| {
        vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from(rp),
            branch: branch.map(std::string::ToString::to_string),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }]
    });
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Gate test item".into(),
        description: None,
        status,
        status_derived: false,
        repo_associations: repo_assoc,
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);
    // Pre-populate the per-work-item harness choice with Claude so
    // gate-spawn tests reach their actual pre-conditions rather
    // than short-circuiting at the "no harness chosen" abort.
    // Tests that exercise the abort itself use explicit setup
    // instead of this fixture. See
    // `harness_choice_applied_to_review_gate_spawn` for the
    // abort-path test.
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);
    (app, wi_id)
}
