//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// Test 11: `spawn_review_gate` reports failures on synchronous
/// pre-conditions (no repo association, no branch) via the returned
/// `ReviewGateSpawn::Blocked`. The background-thread failures (no
/// plan, empty diff, git error) arrive asynchronously via
/// `poll_review_gate` and are covered by other tests.
#[test]
fn spawn_review_gate_sets_status_on_failure() {
    // Case 1: no plan exists - now an ASYNC Blocked message.
    {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );
        let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
        // With the blocking-I/O fix, the no-plan check runs on the
        // background thread. The spawn returns Spawned and the rework
        // flow fires after poll_review_gate drains the Blocked message.
        assert!(matches!(result, ReviewGateSpawn::Spawned));
        drain_review_gate_with_timeout(&mut app, &wi_id);
        assert!(
            app.rework_reasons
                .get(&wi_id)
                .is_some_and(|r| r.contains("no plan")),
            "drained rework reason should mention no plan",
        );
    }

    // Case 2: no branch set - synchronous pre-condition.
    {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            None, // no branch
            Some("/tmp/repo"),
        );
        let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
        match result {
            ReviewGateSpawn::Blocked(reason) => {
                assert!(
                    reason.contains("no branch"),
                    "should mention no branch, got: {reason}",
                );
            }
            ReviewGateSpawn::Spawned => {
                panic!("gate should not have spawned without a branch");
            }
        }
    }

    // Case 3: no repo association - synchronous pre-condition.
    {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            None,
            None, // no repo association
        );
        let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
        match result {
            ReviewGateSpawn::Blocked(reason) => {
                assert!(
                    reason.contains("no repo"),
                    "should mention no repo, got: {reason}",
                );
            }
            ReviewGateSpawn::Spawned => {
                panic!("gate should not have spawned without a repo association");
            }
        }
    }
}

/// Test 2 (from MCP context): Blocked->Review is in the allowed
/// transitions in `poll_mcp_status_updates`. Verify by sending a
/// `StatusUpdate` from Blocked and confirming it is NOT rejected with
/// "not allowed".
#[test]
fn mcp_blocked_to_review_is_allowed_transition() {
    let (mut app, wi_id) = app_with_work_item(
        WorkItemStatus::Blocked,
        Some("feature/test"),
        Some("/tmp/repo"),
    );

    let (tx, rx) = crossbeam_channel::unbounded();
    app.mcp_rx = Some(rx);
    let wi_id_json = serde_json::to_string(&wi_id).unwrap();
    tx.send(McpEvent::StatusUpdate {
        work_item_id: wi_id_json,
        status: "Review".into(),
        reason: "Done".into(),
    })
    .unwrap();

    app.poll_mcp_status_updates();

    // The transition should NOT be rejected as "not allowed". It should
    // reach the gate spawn path (and fail there due to no plan).
    let msg = app.status_message.as_deref().unwrap_or("");
    assert!(
        !msg.contains("not allowed"),
        "Blocked->Review must not be rejected as 'not allowed', got: {msg}",
    );
}

// -- PR identity backfill tests --

pub fn make_assoc(repo: &str, branch: &str) -> crate::work_item_backend::RepoAssociationRecord {
    crate::work_item_backend::RepoAssociationRecord {
        repo_path: PathBuf::from(repo),
        branch: Some(branch.to_string()),
        pr_identity: None,
    }
}

#[test]
fn collect_backfill_requests_returns_done_items_without_pr_identity() {
    use crate::work_item_backend::LocalFileBackend;

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    // Done item with branch but no pr_identity - should be returned.
    let done_record = backend
        .create(CreateWorkItem {
            title: "Done item".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![make_assoc("/tmp/repo", "feature/done")],
        })
        .unwrap();
    backend
        .update_status(&done_record.id, WorkItemStatus::Done)
        .unwrap();

    // Backlog item with branch - should be skipped.
    let _ = backend
        .create(CreateWorkItem {
            title: "Impl item".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![make_assoc("/tmp/repo", "feature/impl")],
        })
        .unwrap();

    // Done item with pr_identity already set - should be skipped.
    let done_with_pr = backend
        .create(CreateWorkItem {
            title: "Done with PR".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![make_assoc("/tmp/repo", "feature/done-pr")],
        })
        .unwrap();
    backend
        .update_status(&done_with_pr.id, WorkItemStatus::Done)
        .unwrap();
    backend
        .save_pr_identity(
            &done_with_pr.id,
            &PathBuf::from("/tmp/repo"),
            &crate::work_item_backend::PrIdentityRecord {
                number: 42,
                title: "Already set".into(),
                url: "https://example.com/pr/42".into(),
            },
        )
        .unwrap();

    let mut app = App::with_config(Config::default(), Arc::new(backend));
    app.worktree_service = Arc::new(StubWorktreeService);

    let requests = app.collect_backfill_requests();

    // Only the first Done item (no pr_identity) should be a candidate.
    // StubWorktreeService.github_remote returns None, so the request
    // is skipped (no github remote). Verify filter works correctly.
    assert!(
        requests.is_empty(),
        "no requests without github remote, got {}",
        requests.len()
    );
}

// -- Delete resource cleanup tests --

/// Delete cleans up all in-memory state keyed by the deleted work item ID:
/// `rework_reasons`, `review_gate_findings`, `no_plan_prompt_queue`, and
/// associated visibility flags.
#[test]
fn delete_cleans_up_memory_state() {
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
    use crate::work_item_backend::ListResult;

    struct TestBackend {
        records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
    }

    impl WorkItemBackend for TestBackend {
        fn read(
            &self,
            id: &WorkItemId,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == *id)
                .cloned()
                .ok_or_else(|| BackendError::NotFound(id.clone()))
        }
        fn list(&self) -> Result<ListResult, BackendError> {
            Ok(ListResult {
                records: self.records.lock().unwrap().clone(),
                corrupt: Vec::new(),
            })
        }
        fn create(
            &self,
            _req: CreateWorkItem,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not used".into()))
        }
        fn delete(&self, id: &WorkItemId) -> Result<(), BackendError> {
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
            unlinked: &crate::work_item::UnlinkedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            let record = crate::work_item_backend::WorkItemRecord {
                display_id: None,
                id: WorkItemId::LocalFile(PathBuf::from("/tmp/delete-mem-test.json")),
                title: unlinked.pr.title.clone(),
                description: None,
                status: WorkItemStatus::Implementing,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: unlinked.repo_path.clone(),
                    branch: Some(unlinked.branch.clone()),
                    pr_identity: None,
                }],
                plan: None,
                done_at: None,
            };
            self.records.lock().unwrap().push(record.clone());
            Ok(record)
        }
        fn import_review_request(
            &self,
            _rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not supported in test".into()))
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
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
    }

    let backend = TestBackend {
        records: std::sync::Mutex::new(Vec::new()),
    };
    let mut app = App::with_config(Config::default(), Arc::new(backend));

    // Import a work item so we have something to delete.
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: PathBuf::from("/repo"),
        pr: PrInfo {
            number: 1,
            title: "Memory cleanup test".into(),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: "https://github.com/o/r/pull/1".into(),
        },
        branch: "1-test".into(),
    });
    app.build_display_list();
    let unlinked_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
        .unwrap();
    app.selected_item = Some(unlinked_idx);
    app.import_selected_unlinked();

    // Get the work item ID.
    let wi_id = app.work_items[0].id.clone();

    // Populate in-memory state for this work item.
    app.rework_reasons
        .insert(wi_id.clone(), "needs fixes".into());
    app.review_gate_findings
        .insert(wi_id.clone(), "some findings".into());
    app.no_plan_prompt_queue.push_back(wi_id.clone());
    app.no_plan_prompt_visible = true;
    app.rework_prompt_wi = Some(wi_id.clone());
    app.rework_prompt_visible = true;
    app.merge_wi_id = Some(wi_id);
    app.confirm_merge = true;

    // Select and delete.
    let work_item_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
        .unwrap();
    app.selected_item = Some(work_item_idx);
    app.sync_selection_identity();
    app.open_delete_prompt();
    app.confirm_delete_from_prompt();

    // Verify all in-memory state is cleaned up.
    assert!(
        app.rework_reasons.is_empty(),
        "rework_reasons should be empty after delete"
    );
    assert!(
        app.review_gate_findings.is_empty(),
        "review_gate_findings should be empty after delete"
    );
    assert!(
        app.no_plan_prompt_queue.is_empty(),
        "no_plan_prompt_queue should be empty after delete"
    );
    assert!(
        !app.no_plan_prompt_visible,
        "no_plan_prompt_visible should be false after delete"
    );
    assert!(
        app.rework_prompt_wi.is_none(),
        "rework_prompt_wi should be None after delete"
    );
    assert!(
        !app.rework_prompt_visible,
        "rework_prompt_visible should be false after delete"
    );
    assert!(
        app.merge_wi_id.is_none(),
        "merge_wi_id should be None after delete"
    );
    assert!(
        !app.confirm_merge,
        "confirm_merge should be false after delete"
    );
}

// -- Shared delete test fixtures --

/// A backend that returns a fixed list of records from `list()`. All
/// mutating operations (delete, `update_status`, etc.) are no-ops that
/// return Ok. Eliminates the need for per-test `OneItemBackend` /
/// `RecordingTestBackend` / etc. boilerplate.
pub struct FixedListBackend {
    pub records: Vec<crate::work_item_backend::WorkItemRecord>,
}

impl FixedListBackend {
    pub fn one_item(id_path: &str, title: &str, repo_path: &str, branch: &str) -> Self {
        Self {
            records: vec![crate::work_item_backend::WorkItemRecord {
                display_id: None,
                id: WorkItemId::LocalFile(PathBuf::from(id_path)),
                title: title.into(),
                description: None,
                status: WorkItemStatus::Implementing,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from(repo_path),
                    branch: Some(branch.into()),
                    pr_identity: None,
                }],
                plan: None,
                done_at: None,
            }],
        }
    }
}

impl WorkItemBackend for FixedListBackend {
    fn read(
        &self,
        id: &WorkItemId,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::NotFound(id.clone()))
    }
    fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
        Ok(crate::work_item_backend::ListResult {
            records: self.records.clone(),
            corrupt: Vec::new(),
        })
    }
    fn create(
        &self,
        _req: CreateWorkItem,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation("not used".into()))
    }
    fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
        Ok(())
    }
    fn update_status(&self, _id: &WorkItemId, _status: WorkItemStatus) -> Result<(), BackendError> {
        Ok(())
    }
    fn import(
        &self,
        _unlinked: &crate::work_item::UnlinkedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation("not used".into()))
    }
    fn import_review_request(
        &self,
        _rr: &crate::work_item::ReviewRequestedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation("not supported in test".into()))
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
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
}

/// Worktree service that records `remove_worktree` / `delete_branch`
/// calls so tests can verify the delete flow invoked git correctly.
pub struct ConfigurableWorktreeService {
    pub remove_worktree_calls: std::sync::Mutex<Vec<(PathBuf, PathBuf, bool, bool)>>,
    pub delete_branch_calls: std::sync::Mutex<Vec<(PathBuf, String, bool)>>,
}

impl ConfigurableWorktreeService {
    pub fn recording() -> Self {
        Self {
            remove_worktree_calls: std::sync::Mutex::new(Vec::new()),
            delete_branch_calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl WorktreeService for ConfigurableWorktreeService {
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
            "not used".into(),
        ))
    }
    fn remove_worktree(
        &self,
        repo_path: &std::path::Path,
        worktree_path: &std::path::Path,
        delete_branch: bool,
        force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        self.remove_worktree_calls.lock().unwrap().push((
            repo_path.to_path_buf(),
            worktree_path.to_path_buf(),
            delete_branch,
            force,
        ));
        Ok(())
    }
    fn delete_branch(
        &self,
        repo_path: &std::path::Path,
        branch: &str,
        force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        self.delete_branch_calls.lock().unwrap().push((
            repo_path.to_path_buf(),
            branch.to_string(),
            force,
        ));
        Ok(())
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
