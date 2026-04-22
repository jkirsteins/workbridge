//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    ActivityId, App, Arc, AtomicBool, BackendType, Config, DisplayEntry, Mutex, Ordering,
    OrphanWorktree, PathBuf, RebaseGateMessage, RebaseGateState, SessionEntry, StubWorktreeService,
    WorkItem, WorkItemBackend, WorkItemId, WorkItemStatus,
};

/// Helper: insert a work item with a worktree+branch into `app` and
/// select it, returning the `WorkItemId`. For use by tests that need
/// a rebase-eligible item without actually spawning a gate.
pub fn setup_rebase_eligible_work_item(app: &mut App) -> WorkItemId {
    use crate::work_item::RepoAssociation;
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/session-guard.json"));
    app.work_items.push(WorkItem {
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        display_id: None,
        title: "session guard test".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![RepoAssociation {
            repo_path: PathBuf::from("/repo"),
            branch: Some("feat/x".into()),
            worktree_path: Some(PathBuf::from("/repo/.worktrees/feat-x")),
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
    wi_id
}

#[test]
fn start_rebase_blocked_while_claude_session_alive() {
    // Pressing `m` while the work item has a live Claude session
    // must be rejected: the rebase gate spawns a headless Claude
    // in the same worktree, and two processes racing on the
    // index and working tree produce nondeterministic results.
    let mut app = App::new();
    let wi_id = setup_rebase_eligible_work_item(&mut app);
    // Insert a live session for this work item.
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    let key = (wi_id, WorkItemStatus::Implementing);
    app.sessions.insert(
        key,
        SessionEntry {
            parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );
    app.start_rebase_on_main();
    assert_eq!(
        app.shell.status_message.as_deref(),
        Some("Cannot rebase while a session is active for this item"),
        "rebase must be blocked while a Claude session is alive",
    );
}

#[test]
fn start_rebase_blocked_while_terminal_session_alive() {
    // Same guard but for the Terminal tab: a user's shell in the
    // same worktree can race with `git rebase` the same way an
    // interactive Claude session can.
    let mut app = App::new();
    let wi_id = setup_rebase_eligible_work_item(&mut app);
    // Insert a live terminal session for this work item.
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.terminal_sessions.insert(
        wi_id,
        SessionEntry {
            parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );
    app.start_rebase_on_main();
    assert_eq!(
        app.shell.status_message.as_deref(),
        Some("Cannot rebase while a session is active for this item"),
        "rebase must be blocked while a terminal session is alive",
    );
}

#[test]
fn start_rebase_allowed_with_dead_sessions() {
    // A dead session (alive=false) should NOT block the rebase.
    // This ensures the guard doesn't over-block after a session
    // has exited but before the entry is removed from the map.
    let mut app = App::new();
    let wi_id = setup_rebase_eligible_work_item(&mut app);
    // Insert a dead Claude session.
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    let key = (wi_id.clone(), WorkItemStatus::Implementing);
    app.sessions.insert(
        key,
        SessionEntry {
            parser: Arc::clone(&parser),
            alive: false,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );
    // Insert a dead terminal session.
    app.terminal_sessions.insert(
        wi_id,
        SessionEntry {
            parser,
            alive: false,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );
    app.start_rebase_on_main();
    // The rebase will proceed to spawn_rebase_gate which will
    // try to acquire the user-action slot and fail (no real
    // infrastructure), but the point is that it was NOT blocked
    // by the dead session guard. The status message should NOT
    // be the "Cannot rebase while a session is active" message.
    assert_ne!(
        app.shell.status_message.as_deref(),
        Some("Cannot rebase while a session is active for this item"),
        "dead sessions must NOT block the rebase",
    );
}

// -----------------------------------------------------------------------
// Rebase-gate cancellation contract tests. These are the structural
// guards behind the architectural rule "cancellation must precede
// destruction" that lives in docs/harness-contract.md C10. They
// exercise: (a) the `Drop` impl on `RebaseGateState`, (b) the new
// `all_dead` check including rebase gates, (c) `send_sigterm_all`
// tearing down rebase gates on graceful quit, and (d) the
// `delete_work_item_by_id` ordering rule (cancel the gate BEFORE
// `backend.delete`).
// -----------------------------------------------------------------------

/// Helper: fabricate a `RebaseGateState` with a known cancellation
/// flag and PID slot for assertion. The receiver end of the
/// channel is dropped by the helper; the gate's poll loop will
/// see a disconnected channel on its next tick, which is
/// acceptable for tests that only exercise the cleanup paths.
pub fn make_rebase_gate_state(
    cancelled: Arc<AtomicBool>,
    child_pid: Arc<Mutex<Option<u32>>>,
) -> RebaseGateState {
    let (_tx, rx) = crossbeam_channel::unbounded::<RebaseGateMessage>();
    RebaseGateState {
        rx,
        progress: None,
        activity: ActivityId(0),
        child_pid,
        cancelled,
    }
}

#[test]
fn drop_rebase_gate_state_sets_cancelled_flag() {
    // Direct test of the `Drop for RebaseGateState` insurance:
    // dropping the state by ANY removal path (HashMap::remove,
    // map::clear, App drop on panic, ...) must signal the
    // background thread via the cancelled flag. We deliberately
    // use a `None` PID slot here because `killpg(0, SIGKILL)`
    // would kill the calling process group (the test runner)
    // and any non-zero stand-in would either fail with EPERM
    // (PID 1) or risk hitting an unrelated process. The killpg
    // path itself is exercised end-to-end by the rebase-gate
    // integration in `spawn_rebase_gate`, where the PID is a
    // real freshly-spawned harness child; here we only assert
    // that Drop sets the flag.
    let cancelled = Arc::new(AtomicBool::new(false));
    let child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    {
        let _gate = make_rebase_gate_state(Arc::clone(&cancelled), Arc::clone(&child_pid));
        assert!(!cancelled.load(Ordering::SeqCst));
    } // gate drops here -> Drop impl fires
    assert!(
        cancelled.load(Ordering::SeqCst),
        "Drop impl must set the cancellation flag"
    );
    assert_eq!(
        *child_pid.lock().unwrap(),
        None,
        "Drop impl must leave a None PID slot unchanged"
    );
}

#[test]
fn all_dead_returns_false_while_rebase_gate_is_in_flight() {
    // The shutdown loop in salsa.rs treats `all_dead()` as the
    // "ready to quit" signal. Before the rebase gate's
    // architectural fix, `all_dead` only checked PTY sessions,
    // so a rebase running with no other live session would let
    // the shutdown loop drop through immediately and leave the
    // harness running against the worktree. The rebase_gates
    // entry must keep `all_dead` returning false.
    let mut app = App::new();
    assert!(
        app.all_dead(),
        "fresh App with no sessions and no gates must be considered dead",
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/all-dead.json"));
    app.rebase_gates.insert(
        wi_id.clone(),
        make_rebase_gate_state(Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None))),
    );
    assert!(
        !app.all_dead(),
        "all_dead must return false while a rebase gate is tracked"
    );
    // Removing the gate (which `send_sigterm_all` /
    // `force_kill_all` do) must let `all_dead` return true again.
    app.rebase_gates.remove(&wi_id);
    assert!(
        app.all_dead(),
        "all_dead must return true once the rebase gate is removed",
    );
}

#[test]
fn send_sigterm_all_drains_rebase_gates_for_graceful_quit() {
    // The graceful quit path (Q press / first SIGTERM) calls
    // `send_sigterm_all` followed by `all_dead` checks. Before
    // the architectural fix, `send_sigterm_all` only signalled
    // PTY sessions, so a rebase gate with no PTY session alive
    // would be left untouched and the shutdown loop would let
    // `Control::Quit` fire immediately. After the fix,
    // `send_sigterm_all` empties the rebase_gates map (which
    // SIGKILLs the harness process group via Drop).
    let mut app = App::new();
    let wi_id_a = WorkItemId::LocalFile(PathBuf::from("/tmp/quit-a.json"));
    let wi_id_b = WorkItemId::LocalFile(PathBuf::from("/tmp/quit-b.json"));
    let cancelled_a = Arc::new(AtomicBool::new(false));
    let cancelled_b = Arc::new(AtomicBool::new(false));
    app.rebase_gates.insert(
        wi_id_a,
        make_rebase_gate_state(Arc::clone(&cancelled_a), Arc::new(Mutex::new(None))),
    );
    app.rebase_gates.insert(
        wi_id_b,
        make_rebase_gate_state(Arc::clone(&cancelled_b), Arc::new(Mutex::new(None))),
    );
    assert_eq!(app.rebase_gates.len(), 2);
    assert!(!app.all_dead());

    app.send_sigterm_all();

    assert!(
        app.rebase_gates.is_empty(),
        "send_sigterm_all must drain all rebase gates",
    );
    assert!(
        cancelled_a.load(Ordering::SeqCst),
        "rebase gate A's cancellation flag must be set after graceful quit",
    );
    assert!(
        cancelled_b.load(Ordering::SeqCst),
        "rebase gate B's cancellation flag must be set after graceful quit",
    );
    assert!(
        app.all_dead(),
        "all_dead must return true after send_sigterm_all empties the map",
    );
}

#[test]
fn force_kill_all_still_drains_rebase_gates() {
    // Even though `send_sigterm_all` now empties the map on its
    // own, `force_kill_all` must keep its own loop as defense
    // against future shutdown entrypoints that bypass
    // `send_sigterm_all`. This test pins that behaviour so a
    // refactor that removes the `force_kill_all` rebase loop
    // can't ship silently.
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/force-kill.json"));
    let cancelled = Arc::new(AtomicBool::new(false));
    app.rebase_gates.insert(
        wi_id,
        make_rebase_gate_state(Arc::clone(&cancelled), Arc::new(Mutex::new(None))),
    );

    app.force_kill_all();

    assert!(
        app.rebase_gates.is_empty(),
        "force_kill_all must also drain rebase gates",
    );
    assert!(
        cancelled.load(Ordering::SeqCst),
        "force_kill_all must trigger the cancellation flag via Drop",
    );
}

#[test]
fn abort_background_ops_for_work_item_drops_rebase_gate() {
    // The unified pre-delete helper must drop the rebase gate
    // for the given work item without touching unrelated gates.
    let mut app = App::new();
    let wi_id_a = WorkItemId::LocalFile(PathBuf::from("/tmp/abort-a.json"));
    let wi_id_b = WorkItemId::LocalFile(PathBuf::from("/tmp/abort-b.json"));
    let cancelled_a = Arc::new(AtomicBool::new(false));
    let cancelled_b = Arc::new(AtomicBool::new(false));
    app.rebase_gates.insert(
        wi_id_a.clone(),
        make_rebase_gate_state(Arc::clone(&cancelled_a), Arc::new(Mutex::new(None))),
    );
    app.rebase_gates.insert(
        wi_id_b.clone(),
        make_rebase_gate_state(Arc::clone(&cancelled_b), Arc::new(Mutex::new(None))),
    );

    app.abort_background_ops_for_work_item(&wi_id_a);

    assert!(
        !app.rebase_gates.contains_key(&wi_id_a),
        "rebase gate for the targeted work item must be removed",
    );
    assert!(
        cancelled_a.load(Ordering::SeqCst),
        "cancellation flag for the targeted gate must be set",
    );
    assert!(
        app.rebase_gates.contains_key(&wi_id_b),
        "rebase gate for an unrelated work item must NOT be removed",
    );
    assert!(
        !cancelled_b.load(Ordering::SeqCst),
        "cancellation flag for an unrelated gate must NOT be touched",
    );
}

#[test]
fn delete_work_item_cancels_rebase_gate_before_backend_delete() {
    // The architectural rule "cancellation must precede
    // destruction" exists because the rebase gate's background
    // thread writes its own activity log entry; if
    // `backend.delete` archives the active log BEFORE the gate
    // is told to stop, there is a window where the bg thread
    // can call `append_activity` and recreate an orphan active
    // log via `OpenOptions::create(true)`. This test pins the
    // ordering by having the fake backend's `delete` impl
    // observe the rebase gate's cancellation flag and store
    // what it sees; the assertion below requires the flag to be
    // `true` at the time backend.delete ran.
    use crate::work_item::WorkItemKind;
    use crate::work_item_backend::{
        ActivityEntry, BackendError, CreateWorkItem, ListResult, WorkItemRecord,
    };

    struct OrderingBackend {
        records: std::sync::Mutex<Vec<WorkItemRecord>>,
        cancelled_observer: Arc<AtomicBool>,
        observed_cancelled_at_delete: Arc<AtomicBool>,
        delete_call_count: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl WorkItemBackend for OrderingBackend {
        fn list(&self) -> Result<ListResult, BackendError> {
            Ok(ListResult {
                records: self.records.lock().unwrap().clone(),
                corrupt: Vec::new(),
            })
        }
        fn read(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == *id)
                .cloned()
                .ok_or_else(|| BackendError::NotFound(id.clone()))
        }
        fn create(&self, _req: CreateWorkItem) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not used".into()))
        }
        fn delete(&self, id: &WorkItemId) -> Result<(), BackendError> {
            // Snapshot the cancellation flag at the exact moment
            // `delete` is called. If the architectural fix is in
            // place, `abort_background_ops_for_work_item` ran
            // before this and the flag is `true`.
            let observed = self.cancelled_observer.load(Ordering::SeqCst);
            self.observed_cancelled_at_delete
                .store(observed, Ordering::SeqCst);
            self.delete_call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut records = self.records.lock().unwrap();
            records.iter().position(|r| r.id == *id).map_or_else(
                || Err(BackendError::NotFound(id.clone())),
                |pos| {
                    records.remove(pos);
                    Ok(())
                },
            )
        }
        fn update_status(
            &self,
            _id: &WorkItemId,
            _status: WorkItemStatus,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn import(
            &self,
            _unlinked: &crate::work_item::UnlinkedPr,
        ) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not used".into()))
        }
        fn import_review_request(
            &self,
            _rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not used".into()))
        }
        fn append_activity(
            &self,
            _id: &WorkItemId,
            _entry: &ActivityEntry,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(None)
        }
        fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
            Ok(())
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<PathBuf> {
            None
        }
    }

    let cancelled_observer = Arc::new(AtomicBool::new(false));
    let observed_cancelled_at_delete = Arc::new(AtomicBool::new(false));
    let delete_call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let backend = Arc::new(OrderingBackend {
        records: std::sync::Mutex::new(Vec::new()),
        cancelled_observer: Arc::clone(&cancelled_observer),
        observed_cancelled_at_delete: Arc::clone(&observed_cancelled_at_delete),
        delete_call_count: Arc::clone(&delete_call_count),
    });

    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/ordering.json"));
    let record = WorkItemRecord {
        display_id: None,
        id: wi_id.clone(),
        kind: WorkItemKind::Own,
        title: "ordering".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        done_at: None,
        plan: None,
        repo_associations: Vec::new(),
    };
    backend.records.lock().unwrap().push(record);

    // Insert a rebase gate whose `cancelled` flag is the SAME
    // Arc the backend is observing. The Drop impl on
    // RebaseGateState will set this flag when the gate is
    // removed, so observing `true` inside `backend.delete`
    // proves the helper ran first.
    app.rebase_gates.insert(
        wi_id.clone(),
        make_rebase_gate_state(Arc::clone(&cancelled_observer), Arc::new(Mutex::new(None))),
    );

    let mut warnings: Vec<String> = Vec::new();
    let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
    app.delete_work_item_by_id(&wi_id, &mut warnings, &mut orphan_worktrees)
        .expect("delete must succeed");

    assert_eq!(
        delete_call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "backend.delete must have been invoked exactly once",
    );
    assert!(
        observed_cancelled_at_delete.load(Ordering::SeqCst),
        "rebase gate cancellation flag must be set BEFORE backend.delete - \
         the architectural rule 'cancellation must precede destruction' is broken",
    );
    assert!(
        !app.rebase_gates.contains_key(&wi_id),
        "rebase gate must be removed from the map after delete",
    );
}
