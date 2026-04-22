//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    ActivityEntry, App, Arc, BackendError, Config, ConfigurableWorktreeService, CreateWorkItem,
    DisplayEntry, FixedListBackend, PathBuf, RepoAssociationRecord, WorkItemBackend, WorkItemId,
    WorkItemStatus, drain_delete_cleanup,
};

/// `open_delete_prompt` must NOT call any blocking worktree check on
/// the UI thread. This is enforced structurally by the
/// `WorktreeService` trait, which no longer exposes
/// `is_worktree_dirty`, so any attempt to reintroduce a dirty check
/// through the injected service would fail to compile. This test
/// additionally verifies that opening the prompt does not touch the
/// backend, so a stray 'y' keypress is required before anything is
/// destroyed.
#[test]
fn open_delete_prompt_does_not_touch_backend() {
    use crate::config::InMemoryConfigProvider;

    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(FixedListBackend::one_item(
            "/tmp/prompt-test.json",
            "Prompt test item",
            "/repo",
            "test-branch",
        )),
        Arc::new(ConfigurableWorktreeService::recording()),
        Box::new(InMemoryConfigProvider::new()),
    );

    // Inject a fake worktree path into the assembled work item.
    assert_eq!(app.work_items.len(), 1);
    app.work_items[0].repo_associations[0].worktree_path =
        Some(PathBuf::from("/tmp/fake-worktree"));
    app.build_display_list();

    // Select the work item.
    let wi_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
        .unwrap();
    app.selected_item = Some(wi_idx);
    app.sync_selection_identity();

    app.open_delete_prompt();
    assert!(app.delete_prompt_visible, "delete prompt should be visible");
    assert_eq!(app.delete_target_title.as_deref(), Some("Prompt test item"),);

    // Opening the prompt must not touch the backend.
    assert_eq!(
        app.work_items.len(),
        1,
        "work item should still exist after opening the prompt"
    );
}

/// Verify that deleting a work item calls `remove_worktree` and
/// `delete_branch` on the worktree service with the correct arguments.
#[test]
fn delete_calls_remove_worktree_and_delete_branch() {
    use crate::config::InMemoryConfigProvider;

    let recording_ws = Arc::new(ConfigurableWorktreeService::recording());

    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(FixedListBackend::one_item(
            "/tmp/recording-test.json",
            "Recording test item",
            "/my/repo",
            "feature-branch",
        )),
        recording_ws.clone(),
        Box::new(InMemoryConfigProvider::new()),
    );

    // Inject a fake RepoFetchResult so delete_work_item_by_id can
    // find the worktree path via repo_data.
    assert_eq!(app.work_items.len(), 1);
    app.repo_data.insert(
        PathBuf::from("/my/repo"),
        crate::work_item::RepoFetchResult {
            repo_path: PathBuf::from("/my/repo"),
            github_remote: None,
            worktrees: Ok(vec![crate::worktree_service::WorktreeInfo {
                path: PathBuf::from("/my/repo/.worktrees/feature-branch"),
                branch: Some("feature-branch".into()),
                is_main: false,
                ..crate::worktree_service::WorktreeInfo::default()
            }]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    );
    app.build_display_list();

    // Select the work item.
    let wi_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
        .unwrap();
    app.selected_item = Some(wi_idx);
    app.sync_selection_identity();

    // Open the prompt, confirm, then drain the background cleanup
    // thread via poll_delete_cleanup (matches how the real event
    // loop consumes results).
    app.open_delete_prompt();
    app.confirm_delete_from_prompt();
    drain_delete_cleanup(&mut app);

    // Verify remove_worktree was called with correct arguments.
    let rw_calls = recording_ws.remove_worktree_calls.lock().unwrap();
    assert_eq!(rw_calls.len(), 1, "remove_worktree should be called once");
    assert_eq!(
        rw_calls[0].0,
        PathBuf::from("/my/repo"),
        "remove_worktree repo_path"
    );
    assert_eq!(
        rw_calls[0].1,
        PathBuf::from("/my/repo/.worktrees/feature-branch"),
        "remove_worktree worktree_path"
    );
    assert!(
        !rw_calls[0].2,
        "remove_worktree delete_branch should be false (handled separately)"
    );
    assert!(
        rw_calls[0].3,
        "remove_worktree force should be true: modal always passes \
         --force to avoid blocking the UI thread on is_worktree_dirty"
    );
    drop(rw_calls);

    // Verify delete_branch was called with correct arguments.
    let db_calls = recording_ws.delete_branch_calls.lock().unwrap();
    assert_eq!(db_calls.len(), 1, "delete_branch should be called once");
    assert_eq!(
        db_calls[0].0,
        PathBuf::from("/my/repo"),
        "delete_branch repo_path"
    );
    assert_eq!(db_calls[0].1, "feature-branch", "delete_branch branch name");
    assert!(
        db_calls[0].2,
        "delete_branch force should be true (user chose to destroy the item)"
    );
}

/// When `gh pr close` fails for an association with an open PR, the
/// delete flow must PRESERVE the local worktree and branch for that
/// association so the user can recover unpushed commits. If it
/// instead force-deleted local resources and then only noticed the
/// PR close failure afterward, the user would be left with an open
/// PR and no local branch to recover from - which is exactly the
/// data-loss path `spawn_unlinked_cleanup` already guards against.
#[test]
fn delete_preserves_local_resources_when_pr_close_fails() {
    use crate::config::InMemoryConfigProvider;
    use crate::pr_service::PullRequestCloser;

    /// Records calls and always fails. Mirrors the shape of the
    /// `RecordingWorktreeService` stub already used in this test
    /// module.
    struct FailingCloser {
        calls: std::sync::Mutex<Vec<(String, String, u64)>>,
    }

    impl PullRequestCloser for FailingCloser {
        fn close_pr(&self, owner: &str, repo: &str, pr_number: u64) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((owner.into(), repo.into(), pr_number));
            Err("simulated gh auth error".into())
        }
    }

    let recording_ws = Arc::new(ConfigurableWorktreeService::recording());
    let failing_closer = Arc::new(FailingCloser {
        calls: std::sync::Mutex::new(Vec::new()),
    });

    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(FixedListBackend::one_item(
            "/tmp/pr-close-fail-test.json",
            "PR close fail item",
            "/my/repo",
            "feature-branch",
        )),
        recording_ws.clone(),
        Box::new(InMemoryConfigProvider::new()),
    );
    // Replace the production gh-based closer with the failing stub
    // before driving the delete. The delete path reads `app.services.pr_closer`
    // once inside `spawn_delete_cleanup` and Arc::clones it into the
    // background thread, so this assignment must happen before
    // `confirm_delete_from_prompt`.
    app.services.pr_closer = failing_closer.clone();

    // Inject cached RepoFetchResult so gather_delete_cleanup_infos
    // finds both the worktree path AND an open PR. The combination
    // of `github_remote: Some(...)` and an OPEN pr with
    // `head_branch == "feature-branch"` is what populates
    // DeleteCleanupInfo.open_pr_number and drives the PR-close path.
    assert_eq!(app.work_items.len(), 1);
    app.repo_data.insert(
        PathBuf::from("/my/repo"),
        crate::work_item::RepoFetchResult {
            repo_path: PathBuf::from("/my/repo"),
            github_remote: Some(("my-org".into(), "my-repo".into())),
            worktrees: Ok(vec![crate::worktree_service::WorktreeInfo {
                path: PathBuf::from("/my/repo/.worktrees/feature-branch"),
                branch: Some("feature-branch".into()),
                is_main: false,
                ..crate::worktree_service::WorktreeInfo::default()
            }]),
            prs: Ok(vec![crate::github_client::GithubPr {
                number: 42,
                title: "Test PR".into(),
                state: "OPEN".into(),
                is_draft: false,
                head_branch: "feature-branch".into(),
                url: "https://example.com/pr/42".into(),
                review_decision: String::new(),
                status_check_rollup: String::new(),
                head_repo_owner: None,
                author: None,
                mergeable: String::new(),
                requested_reviewer_logins: Vec::new(),
                requested_team_slugs: Vec::new(),
            }]),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    );
    app.build_display_list();

    // Select the work item and drive the delete flow.
    let wi_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
        .unwrap();
    app.selected_item = Some(wi_idx);
    app.sync_selection_identity();

    app.open_delete_prompt();
    app.confirm_delete_from_prompt();
    drain_delete_cleanup(&mut app);

    // The closer must have been invoked with the correct arguments
    // so we know we actually exercised the PR-close-first branch,
    // not some unrelated short-circuit.
    let close_calls = failing_closer.calls.lock().unwrap();
    assert_eq!(
        close_calls.len(),
        1,
        "close_pr should have been called exactly once"
    );
    assert_eq!(
        close_calls[0],
        ("my-org".into(), "my-repo".into(), 42u64),
        "close_pr arguments"
    );
    drop(close_calls);

    // Data-loss guard: destructive local cleanup MUST NOT have run
    // after the PR close failed. This is the whole point of the
    // ordering - failure here means an unpushed branch was still
    // force-deleted while the PR stayed open upstream.
    let rw_calls = recording_ws.remove_worktree_calls.lock().unwrap();
    assert!(
        rw_calls.is_empty(),
        "remove_worktree must NOT be called when PR close fails, got: {rw_calls:?}"
    );
    drop(rw_calls);

    let db_calls = recording_ws.delete_branch_calls.lock().unwrap();
    assert!(
        db_calls.is_empty(),
        "delete_branch must NOT be called when PR close fails, got: {db_calls:?}"
    );
    drop(db_calls);

    // The user's only breadcrumb to the preserved paths is the
    // alert dialog - verify it points at both the worktree and
    // branch so the user can find them manually.
    let alert = app
        .alert_message
        .as_deref()
        .expect("alert_message must surface the PR-close failure");
    assert!(
        alert.contains("preserved local worktree"),
        "alert should mention preserved worktree, got: {alert}"
    );
    assert!(
        alert.contains("preserved local branch"),
        "alert should mention preserved branch, got: {alert}"
    );
    assert!(
        alert.contains("feature-branch"),
        "alert should include the branch name, got: {alert}"
    );
}

// -- Auto-archival tests --

/// Backend that tracks records in memory and supports `set_done_at`.
/// Used by auto-archive tests that need functional `delete/update_status`.
pub struct ArchiveTestBackend {
    pub records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
}

impl WorkItemBackend for ArchiveTestBackend {
    fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
        Ok(crate::work_item_backend::ListResult {
            records: self.records.lock().unwrap().clone(),
            corrupt: Vec::new(),
        })
    }
    fn read(
        &self,
        id: &WorkItemId,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::NotFound(id.clone()))
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
    fn update_status(&self, id: &WorkItemId, status: WorkItemStatus) -> Result<(), BackendError> {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|r| r.id == *id) {
            record.status = status;
            Ok(())
        } else {
            Err(BackendError::NotFound(id.clone()))
        }
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
    fn set_done_at(&self, id: &WorkItemId, done_at: Option<u64>) -> Result<(), BackendError> {
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|r| r.id == *id) {
            record.done_at = done_at;
            Ok(())
        } else {
            Err(BackendError::NotFound(id.clone()))
        }
    }
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
}

pub fn make_archive_record(
    name: &str,
    status: WorkItemStatus,
    done_at: Option<u64>,
) -> crate::work_item_backend::WorkItemRecord {
    crate::work_item_backend::WorkItemRecord {
        display_id: None,
        id: WorkItemId::LocalFile(PathBuf::from(format!("/tmp/{name}.json"))),
        title: name.into(),
        description: None,
        status,
        kind: crate::work_item::WorkItemKind::Own,
        repo_associations: vec![RepoAssociationRecord {
            repo_path: PathBuf::from("/repo"),
            branch: None,
            pr_identity: None,
        }],
        plan: None,
        done_at,
    }
}

#[test]
fn auto_archive_deletes_expired_done_items() {
    let now = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // done_at 8 days ago (exceeds default 7-day period).
    let eight_days_ago = now - (8 * 86400);
    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![
            make_archive_record("expired", WorkItemStatus::Done, Some(eight_days_ago)),
            make_archive_record("active", WorkItemStatus::Implementing, None),
        ]),
    };

    let mut cfg = Config::for_test();
    cfg.defaults.archive_after_days = 7;
    let mut app = App::with_config(cfg, Arc::new(backend));
    app.reassemble_work_items();

    // Only the active item should remain.
    assert_eq!(app.work_items.len(), 1);
    assert_eq!(app.work_items[0].title, "active");
}

#[test]
fn auto_archive_skips_when_disabled() {
    let now = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let old = now - (30 * 86400);
    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![make_archive_record(
            "old-done",
            WorkItemStatus::Done,
            Some(old),
        )]),
    };

    let mut cfg = Config::for_test();
    cfg.defaults.archive_after_days = 0; // disabled
    let mut app = App::with_config(cfg, Arc::new(backend));
    app.reassemble_work_items();

    assert_eq!(app.work_items.len(), 1, "should not archive when disabled");
}

#[test]
fn auto_archive_skips_done_without_done_at() {
    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![make_archive_record(
            "done-no-ts",
            WorkItemStatus::Done,
            None, // no done_at timestamp
        )]),
    };

    let mut cfg = Config::for_test();
    cfg.defaults.archive_after_days = 7;
    let mut app = App::with_config(cfg, Arc::new(backend));
    app.reassemble_work_items();

    assert_eq!(
        app.work_items.len(),
        1,
        "should not archive Done items without done_at"
    );
}

#[test]
fn auto_archive_keeps_recent_done_items() {
    let now = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // done_at 3 days ago (within 7-day period).
    let three_days_ago = now - (3 * 86400);
    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![make_archive_record(
            "recent-done",
            WorkItemStatus::Done,
            Some(three_days_ago),
        )]),
    };

    let mut cfg = Config::for_test();
    cfg.defaults.archive_after_days = 7;
    let mut app = App::with_config(cfg, Arc::new(backend));
    app.reassemble_work_items();

    assert_eq!(app.work_items.len(), 1, "recent Done items should be kept");
}

#[test]
fn auto_archive_works_for_derived_done_items() {
    let now = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // done_at 8 days ago, but backend status is Review (derived-Done via merged PR).
    let eight_days_ago = now - (8 * 86400);
    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![make_archive_record(
            "derived-done",
            WorkItemStatus::Review,
            Some(eight_days_ago),
        )]),
    };

    let mut cfg = Config::for_test();
    cfg.defaults.archive_after_days = 7;
    let mut app = App::with_config(cfg, Arc::new(backend));
    app.reassemble_work_items();

    assert_eq!(
        app.work_items.len(),
        0,
        "derived-Done items with expired done_at should be archived"
    );
}

#[test]
fn apply_stage_change_sets_done_at() {
    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![make_archive_record(
            "review-item",
            WorkItemStatus::Review,
            None,
        )]),
    };

    let mut cfg = Config::for_test();
    cfg.defaults.archive_after_days = 7;
    let mut app = App::with_config(cfg, Arc::new(backend));
    app.reassemble_work_items();
    app.build_display_list();

    let wi_id = app.work_items[0].id.clone();
    app.apply_stage_change(
        &wi_id,
        WorkItemStatus::Review,
        WorkItemStatus::Done,
        "pr_merge",
    );

    // Verify done_at was set on the backend record.
    let records = app.services.backend.list().unwrap().records;
    assert_eq!(records.len(), 1);
    assert!(
        records[0].done_at.is_some(),
        "done_at should be set when entering Done"
    );
}
