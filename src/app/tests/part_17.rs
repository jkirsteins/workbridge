//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

impl WorkItemBackend for DoneRecordBackend {
    fn read(
        &self,
        id: &WorkItemId,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        if id == &self.record.id {
            Ok(self.record.clone())
        } else {
            Err(BackendError::NotFound(id.clone()))
        }
    }
    fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
        Ok(crate::work_item_backend::ListResult {
            records: vec![self.record.clone()],
            corrupt: Vec::new(),
        })
    }
    fn create(
        &self,
        _request: CreateWorkItem,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "done-record backend does not support create".into(),
        ))
    }
    fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
        Ok(())
    }
    fn update_status(&self, _id: &WorkItemId, _status: WorkItemStatus) -> Result<(), BackendError> {
        Ok(())
    }
    fn import(
        &self,
        _unlinked: &UnlinkedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "done-record backend does not support import".into(),
        ))
    }
    fn import_review_request(
        &self,
        _rr: &crate::work_item::ReviewRequestedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "done-record backend does not support import_review_request".into(),
        ))
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
    fn update_title(&self, _id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
        Ok(())
    }
    fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
        Ok(self.record.plan.clone())
    }
    fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
        Ok(())
    }
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
}

#[test]
fn collect_backfill_requests_reads_github_remote_from_cache() {
    // Drive `collect_backfill_requests` through a backend that
    // actually returns a Done record with a branch and no
    // `pr_identity`. The cached `github_remote` in `repo_data`
    // supplies owner/repo. The previous version of this test used
    // `StubBackend` (empty list), so the loop body never executed
    // and the counter-zero assertion was vacuously satisfied -
    // the test would have passed on master unchanged, providing
    // zero coverage of the UI-thread blocking-I/O guard.
    let repo = PathBuf::from("/tmp/p0-backfill-repo");
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-backfill.json"));
    let backend = Arc::new(DoneRecordBackend {
        record: crate::work_item_backend::WorkItemRecord {
            display_id: None,
            id: wi_id.clone(),
            title: "backfill-test".into(),
            description: None,
            status: WorkItemStatus::Done,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![crate::work_item_backend::RepoAssociationRecord {
                repo_path: repo.clone(),
                branch: Some("feature/bf".into()),
                pr_identity: None,
            }],
            plan: None,
            done_at: Some(0),
        },
    });

    let ws = CountingWorktreeService::new();
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        backend,
        Arc::clone(&ws) as Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    install_cached_repo(&mut app, &repo, Some("feature/bf"), Some(false));

    let requests = app.collect_backfill_requests();

    assert_eq!(
        requests.len(),
        1,
        "backend returned a Done record with branch and no pr_identity - \
         collect_backfill_requests must produce exactly one request using \
         the cached github_remote",
    );
    let (req_wi_id, req_repo, req_branch, req_owner, req_repo_name) = &requests[0];
    assert_eq!(req_wi_id, &wi_id);
    assert_eq!(req_repo, &repo);
    assert_eq!(req_branch, "feature/bf");
    assert_eq!(req_owner, "owner");
    assert_eq!(req_repo_name, "repo");
    assert_eq!(
        ws.load(),
        0,
        "collect_backfill_requests must never call worktree_service on \
         the UI thread - owner/repo must come from repo_data cache",
    );
}

#[test]
fn branch_has_commits_reads_from_cache_and_never_shells_out() {
    let (mut app, ws) = app_with_counting_ws();
    let repo = PathBuf::from("/tmp/p0-branch-commits-repo");
    // Cache populated with has_commits_ahead=Some(true).
    install_cached_repo(&mut app, &repo, Some("feature/bhc"), Some(true));
    assert!(app.branch_has_commits(&repo, "feature/bhc"));

    // Missing branch / missing cache entry returns the safe default.
    assert!(!app.branch_has_commits(&repo, "unknown-branch"));
    let unknown_repo = PathBuf::from("/tmp/never-fetched");
    assert!(!app.branch_has_commits(&unknown_repo, "anything"));

    // Cache populated with has_commits_ahead=None must also default
    // to false rather than retrying a shell-out.
    let repo2 = PathBuf::from("/tmp/p0-branch-commits-repo-2");
    install_cached_repo(&mut app, &repo2, Some("feature/null"), None);
    assert!(!app.branch_has_commits(&repo2, "feature/null"));

    assert_eq!(
        ws.load(),
        0,
        "branch_has_commits must read from repo_data and never call \
         worktree_service",
    );
}

/// `WorkItemBackend` probe that counts `read_plan` calls through
/// an `AtomicUsize`. Used to assert that `begin_session_open`
/// defers the plan read to a background thread.
///
/// The backend holds a `Mutex` "gate" that the background thread
/// must acquire before it is allowed to call `read_plan`. Tests
/// lock the gate before calling `begin_session_open`, then
/// atomically snapshot the counter (which MUST still be zero)
/// BEFORE releasing the gate. Without the gate the background
/// thread can race the UI thread and the counter may already be
/// `1` by the time the test reads it - a race that would
/// wrongly report a regression.
#[derive(Default)]
pub struct CountingPlanBackend {
    pub read_plan_calls: std::sync::atomic::AtomicUsize,
    pub gate: std::sync::Mutex<()>,
}

impl CountingPlanBackend {
    pub fn load(&self) -> usize {
        self.read_plan_calls
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl WorkItemBackend for CountingPlanBackend {
    fn read(
        &self,
        id: &WorkItemId,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::NotFound(id.clone()))
    }
    fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
        Ok(crate::work_item_backend::ListResult {
            records: Vec::new(),
            corrupt: Vec::new(),
        })
    }
    fn create(
        &self,
        _request: CreateWorkItem,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "counting-plan backend does not support create".into(),
        ))
    }
    fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
        Ok(())
    }
    fn update_status(&self, _id: &WorkItemId, _status: WorkItemStatus) -> Result<(), BackendError> {
        Ok(())
    }
    fn import(
        &self,
        _unlinked: &UnlinkedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "counting-plan backend does not support import".into(),
        ))
    }
    fn import_review_request(
        &self,
        _rr: &crate::work_item::ReviewRequestedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "counting-plan backend does not support import_review_request".into(),
        ))
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
    fn update_title(&self, _id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
        Ok(())
    }
    fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
        // Block until the test releases the gate. This proves the
        // call runs on the background thread - a UI-thread caller
        // would deadlock against the already-held mutex.
        let _guard = self.gate.lock().unwrap();
        self.read_plan_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Some("plan-text from counting backend".into()))
    }
    fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
        Ok(())
    }
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
}

#[test]
fn stage_system_prompt_never_reads_plan_on_ui_thread() {
    // Proof: after the refactor, `stage_system_prompt` takes the
    // plan text as a parameter and MUST NOT call
    // `backend.read_plan(...)` itself. Against the pre-fix code
    // this assertion would fail: the UI-thread call of
    // `stage_system_prompt` unconditionally invoked
    // `self.services.backend.read_plan(work_item_id)` before building the
    // prompt, bumping the counter to 1.
    let backend = Arc::new(CountingPlanBackend::default());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-stage-prompt.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "stage-prompt-test".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/p0-stage-prompt-repo"),
            branch: Some("feature/sp".into()),
            worktree_path: Some(PathBuf::from("/tmp/p0-stage-prompt-worktree")),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    let cwd = PathBuf::from("/tmp/p0-stage-prompt-worktree");
    // The caller passes the plan text as a parameter - the
    // function itself must NEVER consult the backend.
    let _ = app.stage_system_prompt(&wi_id, &cwd, "pre-read plan body".into());
    assert_eq!(
        backend.load(),
        0,
        "stage_system_prompt must use the plan_text parameter and \
         never call backend.read_plan on the UI thread",
    );
}

#[test]
fn begin_session_open_defers_backend_read_plan_to_background_thread() {
    // Proof: `begin_session_open` must NOT call
    // `backend.read_plan` on the UI thread. Under the pre-fix
    // `stage_system_prompt` path, `complete_session_open`
    // -> `stage_system_prompt` would read the plan synchronously
    // before returning to the event loop, freezing the UI while
    // the filesystem read ran. This regression guard ensures the
    // read moves to the background thread driven by
    // `poll_session_opens` / `finish_session_open`.
    let backend = Arc::new(CountingPlanBackend::default());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-session-open.json"));
    // Work item needs a status that allows sessions.
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "session-open-test".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/p0-session-open-repo"),
            branch: Some("feature/so".into()),
            worktree_path: Some(PathBuf::from("/tmp/p0-session-open-worktree")),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Hold the gate so the background thread cannot call
    // `read_plan` until the test releases it. Any synchronous
    // caller of `backend.read_plan` would deadlock here (on the
    // test thread) and `begin_session_open` would never return.
    let gate = backend.gate.lock().unwrap();

    // Record a harness choice so `begin_session_open` does not
    // short-circuit on the "no harness chosen" abort (the same
    // abort path exercised by other tests; mirrors the setup in
    // `harness_choice_applied_to_review_gate_spawn`).
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);

    let cwd = PathBuf::from("/tmp/p0-session-open-worktree");
    app.begin_session_open(&wi_id, &cwd);

    // Immediately after the UI-thread call: the backend MUST NOT
    // have been touched. The background thread is parked waiting
    // on the gate mutex held by this test; the counter is zero.
    let reads_immediately_after = backend.load();
    assert_eq!(
        reads_immediately_after, 0,
        "begin_session_open must defer backend.read_plan to the \
         background thread - see docs/UI.md 'Blocking I/O Prohibition'",
    );
    assert!(
        app.session_open_rx.contains_key(&wi_id),
        "begin_session_open must register a pending receiver for the \
         background plan read",
    );

    // Release the gate so the background thread may proceed, then
    // drain it via the channel. After that the counter must be 1
    // (the background thread actually ran the read).
    drop(gate);
    let entry = app.session_open_rx.remove(&wi_id).unwrap();
    let result = crate::side_effects::clock::bounded_recv(
        &entry.rx,
        "background plan-read thread must deliver a result",
    );
    assert_eq!(result.plan_text, "plan-text from counting backend");
    assert!(result.read_error.is_none());
    assert_eq!(
        backend.load(),
        1,
        "background thread must have performed exactly one read_plan call",
    );
    // End the spinner activity since `poll_session_opens` was
    // bypassed by the manual drain above.
    app.activities.end(entry.activity);
}
