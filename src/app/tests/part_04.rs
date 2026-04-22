//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    ActivityEntry, App, Arc, BackendError, BackendType, Config, CreateWorkItem, DisplayEntry,
    Duration, PathBuf, RepoAssociationRecord, StaleWorktreePrompt, StubBackend,
    StubWorktreeService, UserActionKey, UserActionPayload, WorkItemBackend, WorkItemId,
    WorkItemStatus, WorktreeCreateResult, WorktreeService, drain_worktree_creation,
};

/// F-1 regression: importing a PR whose branch cannot be fetched from
/// origin must NOT create a worktree (to avoid creating from wrong
/// revision). The backend record is still created so the work item
/// exists, but the user is told to check out manually.
#[test]
fn import_skips_worktree_when_fetch_fails() {
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
    use crate::work_item_backend::ListResult;
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    /// Mock worktree service where `fetch_branch` always fails
    /// (simulates fork PR or branch not on origin).
    struct FailFetchWorktreeService {
        created: std::sync::Mutex<Vec<(PathBuf, String, PathBuf)>>,
    }

    impl WorktreeService for FailFetchWorktreeService {
        fn list_worktrees(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            Ok(Vec::new())
        }

        fn create_worktree(
            &self,
            repo_path: &std::path::Path,
            branch: &str,
            target_dir: &std::path::Path,
        ) -> Result<WorktreeInfo, WorktreeError> {
            self.created.lock().unwrap().push((
                repo_path.to_path_buf(),
                branch.to_string(),
                target_dir.to_path_buf(),
            ));
            Ok(WorktreeInfo {
                path: target_dir.to_path_buf(),
                branch: Some(branch.to_string()),
                is_main: false,
                has_commits_ahead: Some(false),
                ..WorktreeInfo::default()
            })
        }

        fn remove_worktree(
            &self,
            _repo_path: &std::path::Path,
            _worktree_path: &std::path::Path,
            _delete_branch: bool,
            _force: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }

        fn delete_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _force: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }

        fn default_branch(&self, _repo_path: &std::path::Path) -> Result<String, WorktreeError> {
            Ok("main".to_string())
        }

        fn github_remote(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<Option<(String, String)>, WorktreeError> {
            Ok(None)
        }

        fn fetch_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
        ) -> Result<(), WorktreeError> {
            Err(WorktreeError::GitError(
                "fatal: couldn't find remote ref fork-branch".into(),
            ))
        }

        fn create_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }

        fn prune_worktrees(&self, _repo_path: &std::path::Path) -> Result<(), WorktreeError> {
            Ok(())
        }
    }

    /// Test backend that supports import.
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
        fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
            Ok(())
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
                id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported.json")),
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
            rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            let record = crate::work_item_backend::WorkItemRecord {
                display_id: None,
                id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported-rr.json")),
                title: rr.pr.title.clone(),
                status: WorkItemStatus::Review,
                kind: crate::work_item::WorkItemKind::ReviewRequest,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: rr.repo_path.clone(),
                    branch: Some(rr.branch.clone()),
                    pr_identity: None,
                }],
                plan: None,
                description: None,
                done_at: None,
            };
            self.records.lock().unwrap().push(record.clone());
            Ok(record)
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

    let mock_ws = Arc::new(FailFetchWorktreeService {
        created: std::sync::Mutex::new(Vec::new()),
    });
    let backend = TestBackend {
        records: std::sync::Mutex::new(Vec::new()),
    };
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(backend),
        Arc::clone(&mock_ws) as Arc<dyn WorktreeService + Send + Sync>,
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );

    // Set up an unlinked PR to import (simulates a fork PR).
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: PathBuf::from("/repos/myrepo"),
        pr: PrInfo {
            number: 99,
            title: "Fork contribution".into(),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: "https://github.com/o/r/pull/99".into(),
        },
        branch: "fork-branch".into(),
    });
    app.build_display_list();

    // Select the unlinked item.
    let unlinked_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
        .expect("should have an unlinked item in display list");
    app.selected_item = Some(unlinked_idx);

    // Import it (spawns background worktree creation).
    app.import_selected_unlinked();

    // Wait for the background thread to complete and poll the result.
    drain_worktree_creation(&mut app);

    // Verify NO worktree was created (fetch failed, so we skip).
    let created = mock_ws.created.lock().unwrap();
    assert_eq!(
        created.len(),
        0,
        "import should NOT create a worktree when fetch fails, but {} were created",
        created.len(),
    );

    // Verify the backend record WAS created (import succeeded).
    assert!(
        !app.work_items.is_empty(),
        "backend record should still be created even when fetch fails",
    );

    // Verify status message tells user about manual checkout.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("could not fetch branch") && msg.contains("Manual checkout required"),
        "expected manual checkout message, got: {msg}",
    );
}

// -- Round 10 regression tests --

/// F-2 regression: `worktree_target_path` builds the path under
/// `repo_path/worktree_dir/sanitized_branch`, not
/// `repo_path.parent()`/<repo>-wt-<branch>.
#[test]
fn worktree_target_path_uses_config_worktree_dir() {
    let repo = PathBuf::from("/repos/myrepo");

    // Default worktree_dir is ".worktrees"
    let path = App::worktree_target_path(&repo, "feature/login", ".worktrees");
    assert_eq!(
        path,
        PathBuf::from("/repos/myrepo/.worktrees/feature-login"),
        "worktree should be under repo_path/worktree_dir with / replaced by -",
    );

    // Custom worktree_dir
    let path = App::worktree_target_path(&repo, "fix/auth-bug", "wt");
    assert_eq!(path, PathBuf::from("/repos/myrepo/wt/fix-auth-bug"),);

    // Branch with no slashes
    let path = App::worktree_target_path(&repo, "simple-branch", ".worktrees");
    assert_eq!(
        path,
        PathBuf::from("/repos/myrepo/.worktrees/simple-branch"),
    );
}

/// `find_reusable_worktree` must only accept worktrees that live at the
/// exact expected target path, are not the main worktree, and are on
/// the target branch. Any other match is rejected so the caller falls
/// through to `create_worktree` (which surfaces git's "already checked
/// out" error for truly conflicting cases).
#[test]
fn find_reusable_worktree_enforces_all_guards() {
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    struct ListOnlyMock {
        entries: Vec<WorktreeInfo>,
    }
    impl WorktreeService for ListOnlyMock {
        fn list_worktrees(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            Ok(self.entries.clone())
        }
        fn create_worktree(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _target_dir: &std::path::Path,
        ) -> Result<WorktreeInfo, WorktreeError> {
            Err(WorktreeError::GitError("not used".into()))
        }
        fn remove_worktree(
            &self,
            _repo_path: &std::path::Path,
            _worktree_path: &std::path::Path,
            _delete_branch: bool,
            _force: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn delete_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _force: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn default_branch(&self, _repo_path: &std::path::Path) -> Result<String, WorktreeError> {
            Ok("main".into())
        }
        fn github_remote(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<Option<(String, String)>, WorktreeError> {
            Ok(None)
        }
        fn fetch_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn create_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn prune_worktrees(&self, _repo_path: &std::path::Path) -> Result<(), WorktreeError> {
            Ok(())
        }
    }

    // find_reusable_worktree canonicalizes both paths, so they must
    // exist on disk. Use a temp dir with a fresh subdirectory per case.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let wt_target = repo.join(".worktrees").join("feature-x");
    std::fs::create_dir_all(&wt_target).unwrap();
    let other_target = repo.join(".worktrees").join("feature-x-alt");
    std::fs::create_dir_all(&other_target).unwrap();

    // Case 1: exact match at wt_target, not main, branch matches -> accept.
    let mock = ListOnlyMock {
        entries: vec![WorktreeInfo {
            path: wt_target.clone(),
            branch: Some("feature-x".into()),
            is_main: false,
            ..WorktreeInfo::default()
        }],
    };
    let found = App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target);
    assert!(found.is_some(), "valid reuse should be accepted");

    // Case 2: is_main=true must be rejected even if path and branch match.
    let mock = ListOnlyMock {
        entries: vec![WorktreeInfo {
            path: wt_target.clone(),
            branch: Some("feature-x".into()),
            is_main: true,
            ..WorktreeInfo::default()
        }],
    };
    assert!(
        App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
        "main worktree must never be reused as a work-item worktree",
    );

    // Case 3: branch mismatch must be rejected.
    let mock = ListOnlyMock {
        entries: vec![WorktreeInfo {
            path: wt_target.clone(),
            branch: Some("other-branch".into()),
            is_main: false,
            ..WorktreeInfo::default()
        }],
    };
    assert!(
        App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
        "branch mismatch must not be reused",
    );

    // Case 4: path mismatch (worktree at a different location than the
    // expected .worktrees/<branch>) must be rejected.
    let mock = ListOnlyMock {
        entries: vec![WorktreeInfo {
            path: other_target,
            branch: Some("feature-x".into()),
            is_main: false,
            ..WorktreeInfo::default()
        }],
    };
    assert!(
        App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
        "worktree at unexpected location must not be silently adopted",
    );

    // Case 5: empty list -> None (happy path for fresh creates).
    let mock = ListOnlyMock { entries: vec![] };
    assert!(
        App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
        "empty list should yield None",
    );

    // Case 6: wt_target does not exist on disk -> None (canonicalization
    // fails; the caller will fall through to create_worktree).
    let missing_target = repo.join(".worktrees").join("never-existed");
    let mock = ListOnlyMock {
        entries: vec![WorktreeInfo {
            path: wt_target,
            branch: Some("feature-x".into()),
            is_main: false,
            ..WorktreeInfo::default()
        }],
    };
    assert!(
        App::find_reusable_worktree(&mock, &repo, "feature-x", &missing_target).is_none(),
        "non-existent target path must not match anything",
    );
}

// -----------------------------------------------------------------------
// Stale worktree recovery regression tests
// -----------------------------------------------------------------------

/// When `poll_worktree_creation` receives a result with
/// `stale_worktree_path: Some(...)`, it must populate
/// `stale_worktree_prompt` instead of falling through to the generic
/// alert path.
#[test]
fn poll_worktree_creation_routes_stale_to_prompt() {
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );

    let (tx, rx) = crossbeam_channel::bounded(1);
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/stale-test.json"));

    tx.send(WorktreeCreateResult {
        wi_id: wi_id.clone(),
        repo_path: PathBuf::from("/repos/myrepo"),
        branch: Some("feature/stale".into()),
        path: None,
        error: Some("Branch locked to stale worktree".into()),
        open_session: true,
        branch_gone: false,
        reused: false,
        stale_worktree_path: Some(PathBuf::from("/repos/myrepo/.worktrees/feature-stale")),
    })
    .unwrap();

    app.try_begin_user_action(UserActionKey::WorktreeCreate, Duration::ZERO, "test");
    app.attach_user_action_payload(
        &UserActionKey::WorktreeCreate,
        UserActionPayload::WorktreeCreate {
            rx,
            wi_id: wi_id.clone(),
        },
    );

    app.poll_worktree_creation();

    assert!(
        app.stale_worktree_prompt.is_some(),
        "stale_worktree_path should route to stale_worktree_prompt",
    );
    assert!(
        app.alert_message.is_none(),
        "stale worktree error should NOT fall through to generic alert",
    );
    let prompt = app.stale_worktree_prompt.as_ref().unwrap();
    assert_eq!(prompt.wi_id, wi_id);
    assert_eq!(
        prompt.stale_path,
        PathBuf::from("/repos/myrepo/.worktrees/feature-stale"),
    );
    assert_eq!(prompt.branch, "feature/stale");
}

/// When a recovery result arrives (`stale_recovery_in_progress` = true)
/// and succeeds, both the recovery flag and prompt must be cleared.
#[test]
fn poll_worktree_creation_clears_stale_recovery_on_success() {
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );

    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/recovery-test.json"));
    app.stale_recovery_in_progress = true;
    app.stale_worktree_prompt = Some(StaleWorktreePrompt {
        wi_id: wi_id.clone(),
        error: "test error".into(),
        stale_path: PathBuf::from("/tmp/stale"),
        repo_path: PathBuf::from("/repos/myrepo"),
        branch: "feature/recover".into(),
        open_session: true,
    });

    // Add a work item so the success path doesn't trip the
    // "work item deleted during creation" branch.
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "recovery test".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(WorktreeCreateResult {
        wi_id: wi_id.clone(),
        repo_path: PathBuf::from("/repos/myrepo"),
        branch: Some("feature/recover".into()),
        path: Some(PathBuf::from("/repos/myrepo/.worktrees/feature-recover")),
        error: None,
        open_session: true,
        branch_gone: false,
        reused: false,
        stale_worktree_path: None,
    })
    .unwrap();

    app.try_begin_user_action(UserActionKey::WorktreeCreate, Duration::ZERO, "test");
    app.attach_user_action_payload(
        &UserActionKey::WorktreeCreate,
        UserActionPayload::WorktreeCreate { rx, wi_id },
    );

    app.poll_worktree_creation();

    assert!(
        !app.stale_recovery_in_progress,
        "stale_recovery_in_progress must be cleared after successful recovery",
    );
    assert!(
        app.stale_worktree_prompt.is_none(),
        "stale_worktree_prompt must be cleared after successful recovery",
    );
}
