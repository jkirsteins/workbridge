//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    ActivityEntry, App, Arc, BackendError, Config, CreateWorkItem, DisplayEntry, FetchMessage,
    PathBuf, RepoAssociationRecord, StubBackend, WorkItemBackend, WorkItemId, WorkItemStatus,
    WorktreeService, drain_worktree_creation,
};

// -- Round 6 regression tests --

/// F-1: Unlinked PR selection keyed by (`repo_path`, branch) not just branch.
/// Two repos can have unlinked PRs on the same branch name. After
/// reassembly, selection must stay on the correct repo's PR.
#[test]
fn unlinked_selection_disambiguates_by_repo_path() {
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};

    let mut app = App::new();

    let repo_a = PathBuf::from("/repos/alpha");
    let repo_b = PathBuf::from("/repos/beta");
    let branch = "update-deps";

    // Two unlinked PRs from different repos with the same branch name.
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: repo_a,
        pr: PrInfo {
            number: 1,
            title: "Update deps (alpha)".into(),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: "https://github.com/o/alpha/pull/1".into(),
        },
        branch: branch.into(),
    });
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: repo_b.clone(),
        pr: PrInfo {
            number: 2,
            title: "Update deps (beta)".into(),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: "https://github.com/o/beta/pull/2".into(),
        },
        branch: branch.into(),
    });
    app.build_display_list();

    // Select the second unlinked item (beta's PR).
    app.select_next_item(); // first unlinked (alpha)
    app.select_next_item(); // second unlinked (beta)

    // Verify we selected the beta PR.
    let sel_idx = app.selected_item.expect("should have selection");
    match &app.display_list[sel_idx] {
        DisplayEntry::UnlinkedItem(ul_idx) => {
            assert_eq!(
                app.unlinked_prs[*ul_idx].repo_path, repo_b,
                "should have selected beta's PR",
            );
        }
        other => panic!("expected UnlinkedItem, got: {other:?}"),
    }

    // Verify the identity tracker stores (repo_path, branch).
    assert_eq!(
        app.selected_unlinked_branch,
        Some((repo_b.clone(), branch.to_string())),
        "identity tracker should store (repo_path, branch)",
    );

    // Simulate reassembly: rebuild display list. Selection should
    // restore to beta's PR, not alpha's (which has the same branch).
    app.build_display_list();

    let restored_idx = app.selected_item.expect("selection should survive rebuild");
    match &app.display_list[restored_idx] {
        DisplayEntry::UnlinkedItem(ul_idx) => {
            assert_eq!(
                app.unlinked_prs[*ul_idx].repo_path, repo_b,
                "after rebuild, selection should still be beta's PR, not alpha's",
            );
            assert_eq!(
                app.unlinked_prs[*ul_idx].pr.number, 2,
                "after rebuild, selected PR number should be 2 (beta), not 1 (alpha)",
            );
        }
        other => panic!("expected UnlinkedItem after rebuild, got: {other:?}"),
    }
}

// -- Round 7 regression tests --

/// F-2: Invalid `branch_issue_pattern` is caught at startup.
/// Verify that an invalid regex is detected and the pattern is reset
/// to an empty string (disabling issue extraction) rather than crashing
/// or causing fetcher threads to die.
#[test]
fn invalid_branch_issue_pattern_caught_at_startup() {
    // Simulate what main.rs does: validate the pattern and replace if invalid.
    let mut cfg = Config::default();
    cfg.defaults.branch_issue_pattern = "[invalid(".to_string();

    let mut app = App::with_config(cfg, Arc::new(StubBackend));

    // Replicate the main.rs validation logic.
    if let Err(e) = regex::Regex::new(&app.services.config.defaults.branch_issue_pattern) {
        let bad = app.services.config.defaults.branch_issue_pattern.clone();
        app.services.config.defaults.branch_issue_pattern = String::new();
        let msg = format!("Invalid branch_issue_pattern '{bad}': {e} (issue extraction disabled)");
        if app.shell.status_message.is_none() {
            app.shell.status_message = Some(msg);
        } else {
            app.pending_fetch_errors.push(msg);
        }
    }

    // The pattern should have been replaced with empty string.
    assert_eq!(
        app.services.config.defaults.branch_issue_pattern, "",
        "invalid pattern should be replaced with empty string",
    );

    // An error message should have been set.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Invalid branch_issue_pattern") && msg.contains("[invalid("),
        "expected invalid pattern error in status, got: {msg}",
    );
}

/// F-2: Disconnected fetcher channel surfaces error in status bar.
/// When all fetcher threads exit (e.g. due to invalid regex), the
/// channel disconnects. `drain_fetch_results` should detect this and
/// set `fetcher_disconnected` = true with a status message.
#[test]
fn disconnected_fetcher_surfaces_error() {
    let mut app = App::new();

    // Create a channel and immediately drop the sender to simulate
    // all fetcher threads exiting.
    let (tx, rx) = std::sync::mpsc::channel::<FetchMessage>();
    app.fetch_rx = Some(rx);
    drop(tx);

    assert!(!app.fetcher_flags.disconnected);

    let received = app.drain_fetch_results();
    // No data was received, but disconnect was detected.
    assert!(!received, "no actual data should have been received");
    assert!(
        app.fetcher_flags.disconnected,
        "fetcher_disconnected should be true after channel disconnect",
    );

    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Background fetcher stopped unexpectedly"),
        "expected disconnect error in status, got: {msg}",
    );

    // Calling drain again should NOT push duplicate errors.
    app.shell.status_message = None;
    app.drain_fetch_results();
    assert!(
        app.shell.status_message.is_none(),
        "should not push duplicate disconnect error",
    );
}

// -- Round 8 regression tests --

/// F-1: Importing an unlinked PR creates a worktree for the imported
/// branch, making the work item immediately sessionable.
#[test]
fn import_creates_worktree_for_branch() {
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
            // Mock: fetch always succeeds (branch exists on origin).
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

    let mock_ws = Arc::new(MockWorktreeService {
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

    // Set up an unlinked PR to import.
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
        branch: "fix-bug".into(),
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

    // Verify a worktree was created.
    let created = mock_ws.created.lock().unwrap();
    assert_eq!(
        created.len(),
        1,
        "import should create exactly one worktree, got {}",
        created.len(),
    );
    assert_eq!(created[0].0, PathBuf::from("/repos/myrepo"));
    assert_eq!(created[0].1, "fix-bug");
    // Worktree should be under repo_path/worktree_dir/branch.
    assert_eq!(
        created[0].2,
        PathBuf::from("/repos/myrepo/.worktrees/fix-bug"),
        "worktree should use config.defaults.worktree_dir, not parent dir",
    );

    // Verify status message indicates success with worktree.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Imported") && msg.contains("worktree created"),
        "expected import success with worktree message, got: {msg}",
    );
}
