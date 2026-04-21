//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// When recovery fails, the recovery flag is cleared and the error is
/// shown via `alert_message` (not back through the stale prompt).
#[test]
fn poll_worktree_creation_shows_alert_on_recovery_failure() {
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );

    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/recovery-fail.json"));
    app.stale_recovery_in_progress = true;
    app.stale_worktree_prompt = Some(StaleWorktreePrompt {
        wi_id: wi_id.clone(),
        error: "original error".into(),
        stale_path: PathBuf::from("/tmp/stale"),
        repo_path: PathBuf::from("/repos/myrepo"),
        branch: "feature/fail".into(),
        open_session: true,
    });

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(WorktreeCreateResult {
        wi_id: wi_id.clone(),
        repo_path: PathBuf::from("/repos/myrepo"),
        branch: Some("feature/fail".into()),
        path: None,
        error: Some("Recovery failed: permission denied".into()),
        open_session: true,
        branch_gone: false,
        reused: false,
        stale_worktree_path: None, // no re-detection on retry failure
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
        "stale_recovery_in_progress must be cleared after failed recovery",
    );
    assert!(
        app.stale_worktree_prompt.is_none(),
        "stale_worktree_prompt must be cleared after failed recovery",
    );
    assert!(
        app.alert_message
            .as_deref()
            .unwrap_or("")
            .contains("Recovery failed"),
        "alert should show the recovery failure error, got: {:?}",
        app.alert_message,
    );
}

/// F-2 regression: `import_selected_unlinked` creates the worktree under
/// `repo_path/worktree_dir/branch`, not `repo_path.parent()`/<repo>-wt-<branch>.
#[test]
fn import_creates_worktree_under_config_worktree_dir() {
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
    use crate::work_item_backend::ListResult;
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    /// Mock worktree service that records `create_worktree` calls.
    struct MockWorktreeService {
        created: std::sync::Mutex<Vec<(PathBuf, String, PathBuf)>>,
    }

    impl WorktreeService for MockWorktreeService {
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

    // Use a custom worktree_dir to verify it is respected.
    let mut config = Config::default();
    config.defaults.worktree_dir = "my-worktrees".to_string();

    let mock_ws = Arc::new(MockWorktreeService {
        created: std::sync::Mutex::new(Vec::new()),
    });
    let backend = TestBackend {
        records: std::sync::Mutex::new(Vec::new()),
    };
    let mut app = App::with_config_and_worktree_service(
        config,
        Arc::new(backend),
        Arc::clone(&mock_ws) as Arc<dyn WorktreeService + Send + Sync>,
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );

    // Set up an unlinked PR with a branch containing /.
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: PathBuf::from("/repos/myrepo"),
        pr: PrInfo {
            number: 42,
            title: "Fix the bug".into(),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: "https://github.com/o/r/pull/42".into(),
        },
        branch: "feature/login-page".into(),
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

    // Verify the worktree target directory uses config.defaults.worktree_dir
    // and sanitizes the branch name.
    let created = mock_ws.created.lock().unwrap();
    assert_eq!(
        created.len(),
        1,
        "import should create exactly one worktree",
    );
    assert_eq!(
        created[0].2,
        PathBuf::from("/repos/myrepo/my-worktrees/feature-login-page"),
        "worktree should be under repo_path/worktree_dir/sanitized-branch",
    );
}

// -- Codex round regression tests --

/// F-3: `create_work_item_with` rejects repos where `git_dir_present` is false.
/// Even if a repo path is passed in the repos list, it should be filtered
/// out when the corresponding `active_repo_cache` entry has `git_dir_present`
/// set to false.
#[test]
fn create_work_item_with_rejects_repos_without_git_dir() {
    use crate::work_item_backend::ListResult;

    /// Backend that records create calls via a shared Arc.
    struct RecordingBackend {
        last_repos: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    }

    impl WorkItemBackend for RecordingBackend {
        fn read(
            &self,
            id: &WorkItemId,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::NotFound(id.clone()))
        }
        fn list(&self) -> Result<ListResult, BackendError> {
            Ok(ListResult {
                records: Vec::new(),
                corrupt: Vec::new(),
            })
        }
        fn create(
            &self,
            req: CreateWorkItem,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            *self.last_repos.lock().unwrap() = req
                .repo_associations
                .iter()
                .map(|r| r.repo_path.clone())
                .collect();
            let record = crate::work_item_backend::WorkItemRecord {
                display_id: None,
                id: WorkItemId::LocalFile(PathBuf::from("/tmp/new.json")),
                title: req.title.clone(),
                description: None,
                status: req.status,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: req.repo_associations,
                plan: None,
                done_at: None,
            };
            Ok(record)
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

    let last_repos = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut app = App::with_config(
        Config::default(),
        Arc::new(RecordingBackend {
            last_repos: Arc::clone(&last_repos),
        }),
    );

    // Populate active_repo_cache with one repo that has git_dir and one
    // that does not.
    app.active_repo_cache = vec![
        RepoEntry {
            path: PathBuf::from("/repos/with-git"),
            source: RepoSource::Explicit,
            git_dir_present: true,
        },
        RepoEntry {
            path: PathBuf::from("/repos/no-git"),
            source: RepoSource::Explicit,
            git_dir_present: false,
        },
    ];

    // Attempt to create with both repos selected.
    let result = app.create_work_item_with(
        "Test item".into(),
        None,
        vec![
            PathBuf::from("/repos/with-git"),
            PathBuf::from("/repos/no-git"),
        ],
        "feature/test".into(),
    );
    assert!(result.is_ok(), "create should succeed for valid repos");

    // The status message should indicate success.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Created"),
        "expected success message, got: {msg}",
    );

    // Verify only the repo with git_dir_present was sent to the backend.
    let repos = last_repos.lock().unwrap();
    assert_eq!(
        repos.len(),
        1,
        "backend should receive exactly one repo, got {}",
        repos.len(),
    );
    assert_eq!(
        repos[0],
        PathBuf::from("/repos/with-git"),
        "only the repo with git_dir_present should be included",
    );
}

pub fn make_work_item(path: &str, title: &str, status: WorkItemStatus) -> WorkItem {
    use crate::work_item::RepoAssociation;
    WorkItem {
        display_id: None,
        id: WorkItemId::LocalFile(PathBuf::from(format!("/data/{title}.json"))),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: title.to_string(),
        description: None,
        status,
        status_derived: false,
        repo_associations: vec![RepoAssociation {
            repo_path: PathBuf::from(path),
            branch: None,
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    }
}

#[test]
fn display_list_groups_by_stage_and_repo() {
    let mut app = App::new();
    app.work_items = vec![
        make_work_item("/repos/alpha", "Backlog item", WorkItemStatus::Backlog),
        make_work_item("/repos/alpha", "Done item", WorkItemStatus::Done),
    ];
    app.build_display_list();

    let work_item_entry_count = app
        .display_list
        .iter()
        .filter(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
        .count();
    assert_eq!(work_item_entry_count, 2, "both items should appear");

    let group_headers: Vec<_> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::GroupHeader { label, count, .. } => Some((label.as_str(), *count)),
            _ => None,
        })
        .collect();
    assert_eq!(group_headers.len(), 2);
    assert_eq!(group_headers[0], ("BACKLOGGED (alpha)", 1));
    assert_eq!(group_headers[1], ("DONE (alpha)", 1));
}

#[test]
fn display_list_all_backlog_only_shows_backlogged_group() {
    let mut app = App::new();
    app.work_items = vec![
        make_work_item("/repos/myrepo", "Item A", WorkItemStatus::Backlog),
        make_work_item("/repos/myrepo", "Item B", WorkItemStatus::Backlog),
    ];
    app.build_display_list();

    let headers: Vec<_> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::GroupHeader { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(headers, vec!["BACKLOGGED (myrepo)"]);
}

#[test]
fn display_list_all_active_only_shows_active_group() {
    let mut app = App::new();
    app.work_items = vec![
        make_work_item(
            "/repos/myrepo",
            "Implementing item",
            WorkItemStatus::Implementing,
        ),
        make_work_item("/repos/myrepo", "Review item", WorkItemStatus::Review),
    ];
    app.build_display_list();

    let headers: Vec<_> = app
        .display_list
        .iter()
        .filter_map(|e| match e {
            DisplayEntry::GroupHeader { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(headers, vec!["ACTIVE (myrepo)"]);
}

#[test]
fn display_list_no_items_no_groups() {
    let mut app = App::new();
    app.build_display_list();
    assert!(app.display_list.is_empty());
}
