//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

#[test]
fn apply_stage_change_clears_done_at_on_retreat() {
    let now = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![make_archive_record(
            "done-item",
            WorkItemStatus::Done,
            Some(now),
        )]),
    };

    let mut cfg = Config::for_test();
    cfg.defaults.archive_after_days = 7;
    let mut app = App::with_config(cfg, Arc::new(backend));
    app.reassemble_work_items();
    app.build_display_list();

    let wi_id = app.work_items[0].id.clone();
    app.apply_stage_change(&wi_id, WorkItemStatus::Done, WorkItemStatus::Review, "test");

    // Verify done_at was cleared.
    let records = app.backend.list().unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].done_at, None,
        "done_at should be cleared when retreating from Done"
    );
}

// -- P0 Blocking-I/O regression guard (GAN ui-audit-p0-io) --
//
// Ensures the UI-thread entry points touched by the audit never call
// into `WorktreeService` synchronously. We install a worktree service
// that atomically bumps a per-method counter and returns a stub
// result without blocking. Each regression test snapshots the
// counter immediately after the UI-thread entry point returns and
// asserts that NOTHING was called on the main thread. Background
// threads spawned by the entry points are free to increment the
// counter later - the snapshot is taken synchronously before any
// thread progress is observable.
//
// A counting probe is used instead of a panicking probe because the
// PR-create / merge / review-submit entry points intentionally spawn
// background threads that DO call `default_branch` later. A panic on
// the worker thread would pollute `--nocapture` output without
// adding signal.

/// Counting probe that records how many times any method was
/// called on the UI thread. A `Mutex<()>` "gate" establishes a
/// deterministic happens-before edge: tests that spawn a
/// background thread which might call into this service acquire
/// the gate BEFORE invoking the UI-thread entry point, snapshot
/// the counter, then drop the gate so the background thread can
/// proceed. Without the gate the background thread could race the
/// test thread and bump the counter before the assertion runs,
/// flaking the test under CI load. Mirrors the pattern used by
/// `CountingPlanBackend`.
#[derive(Default)]
pub struct CountingWorktreeService {
    pub calls: std::sync::atomic::AtomicUsize,
    pub gate: std::sync::Mutex<()>,
}

impl CountingWorktreeService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn load(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Acquire the gate mutex, block until the test thread
    /// releases it, then atomically bump the counter. Every
    /// trait method routes through here, so any caller - UI
    /// thread or background thread - is forced to serialize
    /// against whichever test is holding the gate.
    pub fn gated_bump(&self) {
        let _guard = self.gate.lock().unwrap();
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl crate::worktree_service::WorktreeService for CountingWorktreeService {
    fn list_worktrees(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<Vec<crate::worktree_service::WorktreeInfo>, crate::worktree_service::WorktreeError>
    {
        self.gated_bump();
        Ok(Vec::new())
    }

    fn create_worktree(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
        target_dir: &std::path::Path,
    ) -> Result<crate::worktree_service::WorktreeInfo, crate::worktree_service::WorktreeError> {
        self.gated_bump();
        Ok(crate::worktree_service::WorktreeInfo {
            path: target_dir.to_path_buf(),
            branch: None,
            is_main: false,
            ..crate::worktree_service::WorktreeInfo::default()
        })
    }

    fn remove_worktree(
        &self,
        _repo_path: &std::path::Path,
        _worktree_path: &std::path::Path,
        _delete_branch: bool,
        _force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        self.gated_bump();
        Ok(())
    }

    fn delete_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
        _force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        self.gated_bump();
        Ok(())
    }

    fn default_branch(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<String, crate::worktree_service::WorktreeError> {
        self.gated_bump();
        Ok("main".into())
    }

    fn github_remote(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<Option<(String, String)>, crate::worktree_service::WorktreeError> {
        self.gated_bump();
        Ok(None)
    }

    fn fetch_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        self.gated_bump();
        Ok(())
    }

    fn create_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        self.gated_bump();
        Ok(())
    }
    fn prune_worktrees(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }
}

/// Build an `App` wired with the counting worktree service and an
/// in-memory config provider. Returns the shared `Arc` so tests can
/// snapshot the call count after each UI-thread entry point.
pub fn app_with_counting_ws() -> (App, Arc<CountingWorktreeService>) {
    let ws = CountingWorktreeService::new();
    let app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::clone(&ws) as Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    (app, ws)
}

/// Install a cached `RepoFetchResult` with the given `github_remote`
/// and optional worktree so UI-thread cache reads (`github_remote`,
/// `has_commits_ahead`) return real data without ever touching the
/// worktree service.
pub fn install_cached_repo(
    app: &mut App,
    repo_path: &std::path::Path,
    branch: Option<&str>,
    has_commits_ahead: Option<bool>,
) {
    let worktrees = branch.map_or_else(Vec::new, |b| {
        vec![crate::worktree_service::WorktreeInfo {
            path: repo_path.join(".worktrees").join(b),
            branch: Some(b.to_string()),
            is_main: false,
            has_commits_ahead,
            ..crate::worktree_service::WorktreeInfo::default()
        }]
    });
    app.repo_data.insert(
        repo_path.to_path_buf(),
        crate::work_item::RepoFetchResult {
            repo_path: repo_path.to_path_buf(),
            github_remote: Some(("owner".into(), "repo".into())),
            worktrees: Ok(worktrees),
            prs: Ok(Vec::new()),
            review_requested_prs: Ok(Vec::new()),
            current_user_login: None,
            issues: Vec::new(),
        },
    );
}

pub fn push_review_work_item(
    app: &mut App,
    id: &WorkItemId,
    repo_path: &std::path::Path,
    branch: &str,
    status: WorkItemStatus,
) {
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "blocking-io-test".into(),
        description: None,
        status,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: repo_path.to_path_buf(),
            branch: Some(branch.to_string()),
            worktree_path: None,
            pr: Some(crate::work_item::PrInfo {
                number: 42,
                url: "https://example.com/pr/42".into(),
                state: crate::work_item::PrState::Open,
                title: "pr".into(),
                is_draft: false,
                checks: crate::work_item::CheckStatus::Passing,
                mergeable: crate::work_item::MergeableState::Unknown,
                review_decision: crate::work_item::ReviewDecision::None,
            }),
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
}

/// Minimal `WorkItemBackend` whose `read_plan` returns a non-empty
/// plan string so `spawn_review_gate` progresses past the plan check
/// on the background thread. All other methods defer to `StubBackend`
/// semantics (no-op / not-found) so the backend stays inert for the
/// regression test's purposes.
pub struct NonEmptyPlanBackend;

impl WorkItemBackend for NonEmptyPlanBackend {
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
            "non-empty-plan backend does not support create".into(),
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
            "non-empty-plan backend does not support import".into(),
        ))
    }
    fn import_review_request(
        &self,
        _rr: &crate::work_item::ReviewRequestedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "non-empty-plan backend does not support import_review_request".into(),
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
        Ok(Some("plan-text for regression test".into()))
    }
    fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
        Ok(())
    }
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
}

#[test]
fn spawn_review_gate_does_not_touch_worktree_service_synchronously() {
    // Exercise the full happy-path pre-conditions (plan exists,
    // branch is set, repo association present) so the background
    // thread is the ONLY place `default_branch` / `github_remote` /
    // `git diff` may run. Against the pre-fix master version this
    // assertion would fail: `spawn_review_gate` called
    // `self.worktree_service.default_branch(&repo_path)` on the UI
    // thread after reading the plan, bumping the counter to 1.
    let ws = CountingWorktreeService::new();
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(NonEmptyPlanBackend),
        Arc::clone(&ws) as Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let repo = PathBuf::from("/tmp/p0-review-gate-repo");
    install_cached_repo(&mut app, &repo, Some("feature/gate"), Some(true));
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-gate.json"));
    push_review_work_item(
        &mut app,
        &wi_id,
        &repo,
        "feature/gate",
        WorkItemStatus::Implementing,
    );
    // Pre-populate the per-work-item harness choice so the gate
    // reaches its real pre-conditions rather than short-circuiting
    // at "no harness chosen" (Milestone 3 rule; see
    // `harness_choice_applied_to_review_gate_spawn` for the
    // abort-path test).
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);

    // Hold the gate mutex so the background thread (which WILL
    // call `default_branch` / `github_remote` as soon as it wakes)
    // cannot increment the counter before we snapshot it. Without
    // this deterministic happens-before edge, CI-load thread
    // scheduling can let the background thread run first and
    // flake the assertion. Mirrors the pattern used by
    // `begin_session_open_defers_backend_read_plan_to_background_thread`.
    let gate = ws.gate.lock().unwrap();

    let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
    let ws_calls_after_spawn = ws.load();

    assert!(
        matches!(result, ReviewGateSpawn::Spawned),
        "gate must spawn when plan, branch and repo are all present",
    );
    assert_eq!(
        ws_calls_after_spawn, 0,
        "spawn_review_gate must not touch worktree_service on the UI thread: \
         read_plan, default_branch, git diff and github_remote must all run \
         inside the std::thread::spawn closure",
    );

    // Spawning the gate must register a status-bar activity per
    // `docs/UI.md` "Activity indicator placement" - assert it is
    // visible BEFORE we drop the gate so the spinner is observable
    // in the live system, not just after teardown.
    assert!(
        app.activities.current().is_some(),
        "spawn_review_gate must register a status-bar activity",
    );

    // Release the gate so the background thread can proceed and
    // drain. Routing through `drop_review_gate` ensures the
    // associated activity is also ended - the same teardown path
    // every drop site uses.
    drop(gate);
    app.drop_review_gate(&wi_id);
    assert!(
        app.activities.current().is_none(),
        "drop_review_gate must end the review gate activity",
    );
}

#[test]
fn spawn_pr_creation_reads_github_remote_from_cache() {
    // Happy path: the cached github_remote is populated, so the main
    // thread never calls into worktree_service. The background thread
    // WILL call `default_branch` later. The gate mutex establishes a
    // deterministic happens-before edge so the counter snapshot runs
    // before the background thread can increment it, eliminating the
    // CI-load race condition that would otherwise flake this test.
    let (mut app, ws) = app_with_counting_ws();
    let repo = PathBuf::from("/tmp/p0-pr-create-repo");
    install_cached_repo(&mut app, &repo, Some("feature/prc"), Some(true));
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-prc.json"));
    push_review_work_item(
        &mut app,
        &wi_id,
        &repo,
        "feature/prc",
        WorkItemStatus::Review,
    );

    // Hold the gate so the background thread is blocked on its
    // first `default_branch` call and cannot race the snapshot.
    let gate = ws.gate.lock().unwrap();

    app.end_user_action(&UserActionKey::PrCreate);
    app.spawn_pr_creation(&wi_id);
    let ws_calls_after_spawn = ws.load();

    assert_eq!(
        app.user_action_work_item(&UserActionKey::PrCreate),
        Some(&wi_id),
    );
    assert_eq!(
        ws_calls_after_spawn, 0,
        "spawn_pr_creation must read github_remote from repo_data, not \
         worktree_service, on the UI thread",
    );

    // Release the gate so the background thread can drain. Dropping
    // the receiver stops any progress being observed.
    drop(gate);
    app.end_user_action(&UserActionKey::PrCreate);
}

#[test]
fn execute_merge_reads_github_remote_from_cache() {
    // Happy path: the cached github_remote is populated, so the UI
    // thread never calls into worktree_service. The background
    // precheck thread (`spawn_merge_precheck`) WILL call
    // `list_worktrees` later. The gate mutex establishes a
    // deterministic happens-before edge so the counter snapshot
    // runs before that background call can increment it - same
    // pattern as `spawn_pr_creation_reads_github_remote_from_cache`.
    let (mut app, ws) = app_with_counting_ws();
    let repo = PathBuf::from("/tmp/p0-merge-repo");
    install_cached_repo(&mut app, &repo, Some("feature/merge"), Some(true));
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-merge.json"));
    push_review_work_item(
        &mut app,
        &wi_id,
        &repo,
        "feature/merge",
        WorkItemStatus::Review,
    );

    // Hold the gate so the background precheck thread is blocked
    // on its first `list_worktrees` call and cannot race the
    // snapshot. Without this, `execute_merge` -> `spawn_merge_precheck`
    // would race against `ws.load()` on slow CI.
    let gate = ws.gate.lock().unwrap();

    app.end_user_action(&UserActionKey::PrMerge);
    app.execute_merge(&wi_id, "squash");
    let ws_calls_after_spawn = ws.load();

    assert!(
        app.is_user_action_in_flight(&UserActionKey::PrMerge) || app.alert_message.is_some(),
        "execute_merge must proceed past the github_remote lookup",
    );
    assert_eq!(
        ws_calls_after_spawn, 0,
        "execute_merge must read github_remote from repo_data, not \
         worktree_service, on the UI thread",
    );

    // Release the gate so the background precheck thread can
    // drain. Dropping the slot stops any progress being observed.
    drop(gate);
    app.end_user_action(&UserActionKey::PrMerge);
}

#[test]
fn spawn_review_submission_reads_github_remote_from_cache() {
    let (mut app, ws) = app_with_counting_ws();
    let repo = PathBuf::from("/tmp/p0-review-submit-repo");
    install_cached_repo(&mut app, &repo, Some("feature/rs"), Some(true));
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-rs.json"));
    push_review_work_item(
        &mut app,
        &wi_id,
        &repo,
        "feature/rs",
        WorkItemStatus::Review,
    );

    app.end_user_action(&UserActionKey::ReviewSubmit);
    app.spawn_review_submission(&wi_id, "approve", "");
    let ws_calls_after_spawn = ws.load();

    assert_eq!(
        app.user_action_work_item(&UserActionKey::ReviewSubmit),
        Some(&wi_id),
    );
    assert_eq!(
        ws_calls_after_spawn, 0,
        "spawn_review_submission must read github_remote from repo_data, \
         not worktree_service, on the UI thread",
    );
    app.end_user_action(&UserActionKey::ReviewSubmit);
}

#[test]
fn enter_mergequeue_reads_github_remote_from_cache() {
    let (mut app, ws) = app_with_counting_ws();
    let repo = PathBuf::from("/tmp/p0-mq-repo");
    install_cached_repo(&mut app, &repo, Some("feature/mq"), Some(true));
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-mq.json"));
    push_review_work_item(
        &mut app,
        &wi_id,
        &repo,
        "feature/mq",
        WorkItemStatus::Review,
    );

    app.enter_mergequeue(&wi_id);
    assert!(
        app.mergequeue_watches.iter().any(|w| w.wi_id == wi_id),
        "enter_mergequeue must proceed past the github_remote lookup using \
         cached data only",
    );
    assert_eq!(
        ws.load(),
        0,
        "enter_mergequeue must never call worktree_service on the UI thread",
    );
}

/// Minimal `WorkItemBackend` whose `list` returns a single Done
/// record with a branch and `pr_identity: None`. Used by the
/// backfill regression test to prove `collect_backfill_requests`
/// actually enters its loop body (the old version used
/// `StubBackend` whose empty list skipped the loop entirely, so
/// the counter-zero assertion was trivially satisfied for the
/// wrong reason).
pub struct DoneRecordBackend {
    pub record: crate::work_item_backend::WorkItemRecord,
}
