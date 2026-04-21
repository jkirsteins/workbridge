//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

// The wall-clock-free bounded-receive helper lives in
// `crate::side_effects::clock::bounded_recv` and works with both
// `mpsc::Receiver` and `crossbeam_channel::Receiver`. It replaces
// three earlier per-module copies that had drifted on iteration
// budget and panic wording.

// -- F-1 regression test --

#[test]
fn manage_unmanage_sets_fetcher_repos_changed() {
    // Setup: create a config with a base_dir containing a discovered repo.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(dir.join("repo-a/.git")).unwrap();

    let mut cfg = Config::default();
    cfg.add_base_dir(dir.to_str().unwrap()).unwrap();
    // Discovered repo starts unmanaged - include it.
    let all = cfg.all_repos();
    assert!(!all.is_empty(), "should discover at least one repo");
    let _repo_display = all[0].path.display().to_string();

    let mut app = App::with_config(cfg, Arc::new(StubBackend));

    // Initially false.
    assert!(!app.fetcher_repos_changed);

    // Manage a repo from the available list.
    app.settings_list_focus = SettingsListFocus::Available;
    app.settings_available_selected = 0;
    app.manage_selected_repo();
    assert!(
        app.fetcher_repos_changed,
        "fetcher_repos_changed should be true after manage"
    );

    // Reset and test unmanage.
    app.fetcher_repos_changed = false;
    app.settings_list_focus = SettingsListFocus::Managed;
    // The managed repo that is discovered (not explicit) can be unmanaged.
    // Find the discovered repo in the managed list.
    let discovered_idx = app
        .active_repo_cache
        .iter()
        .position(|e| e.source == RepoSource::Discovered)
        .expect("should have a discovered managed repo");
    app.settings_repo_selected = discovered_idx;
    app.unmanage_selected_repo();
    assert!(
        app.fetcher_repos_changed,
        "fetcher_repos_changed should be true after unmanage"
    );
}

// -- F-3 regression test --

#[test]
fn is_inside_managed_repo_positive() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    // Create the subdirectory on disk so canonicalize succeeds.
    std::fs::create_dir_all(dir.join("src")).unwrap();

    let mut cfg = Config::default();
    cfg.add_repo(dir.to_str().unwrap()).unwrap();

    let app = App::with_config(cfg, Arc::new(StubBackend));

    // The repo root itself should be inside.
    assert!(app.is_inside_managed_repo(&dir));
    // A subdirectory should also be inside.
    let subdir = dir.join("src");
    assert!(app.is_inside_managed_repo(&subdir));
    // An unrelated path should not be inside.
    assert!(!app.is_inside_managed_repo(&PathBuf::from("/tmp/unrelated")));
}

// -- Round 3 regression tests --

/// F-1: `managed_repo_root` returns repo root, not subdirectory path.
/// Work item creation must store the repo root, not CWD when CWD is
/// a subdirectory of a managed repo.
#[test]
fn managed_repo_root_returns_root_not_subdir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("src/deeply/nested")).unwrap();

    let mut cfg = Config::default();
    cfg.add_repo(dir.to_str().unwrap()).unwrap();

    let app = App::with_config(cfg, Arc::new(StubBackend));

    // From a subdirectory, managed_repo_root should return the repo root.
    let subdir = dir.join("src/deeply/nested");
    let root = app.managed_repo_root(&subdir);
    assert!(root.is_some(), "subdir should be inside a managed repo");
    let root = root.unwrap();
    let canonical_dir = crate::config::canonicalize_path(&dir).unwrap();
    assert_eq!(
        root,
        canonical_dir,
        "managed_repo_root should return the repo root {}, not the subdir {}",
        canonical_dir.display(),
        subdir.display(),
    );
}

/// F-2: `fetcher_repos_changed` is set after import and delete.
/// Import and delete change backend records, so the fetcher must
/// be restarted to pick up new/removed extra branches.
#[test]
fn import_and_delete_set_fetcher_repos_changed() {
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
    use crate::work_item_backend::ListResult;

    /// Test backend that supports import and delete.
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
                id: WorkItemId::LocalFile(PathBuf::from("/tmp/fake.json")),
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
                id: WorkItemId::LocalFile(PathBuf::from("/tmp/fake-rr.json")),
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

    let backend = TestBackend {
        records: std::sync::Mutex::new(Vec::new()),
    };
    let mut app = App::with_config(Config::default(), Arc::new(backend));

    // Set up an unlinked PR to import.
    app.unlinked_prs.push(crate::work_item::UnlinkedPr {
        repo_path: PathBuf::from("/repo"),
        pr: PrInfo {
            number: 1,
            title: "Test PR".into(),
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
    // Select the unlinked item.
    let unlinked_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
        .expect("should have an unlinked item in display list");
    app.selected_item = Some(unlinked_idx);

    assert!(!app.fetcher_repos_changed);
    app.import_selected_unlinked();
    assert!(
        app.fetcher_repos_changed,
        "fetcher_repos_changed should be true after import",
    );

    // Reset and test delete.
    app.fetcher_repos_changed = false;
    // Select the now-imported work item.
    let work_item_idx = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
        .expect("should have a work item in display list after import");
    app.selected_item = Some(work_item_idx);
    app.sync_selection_identity();
    app.open_delete_prompt();
    app.confirm_delete_from_prompt();
    assert!(
        app.fetcher_repos_changed,
        "fetcher_repos_changed should be true after delete",
    );
}

/// F-1: `fetcher_repos_changed` is set after creating a work item with a
/// branch. Without this, the fetcher never picks up the new branch for
/// issue metadata.
#[test]
fn create_with_branch_sets_fetcher_repos_changed() {
    use crate::work_item_backend::ListResult;

    struct CreateBackend;

    impl WorkItemBackend for CreateBackend {
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
            Ok(crate::work_item_backend::WorkItemRecord {
                display_id: None,
                id: WorkItemId::LocalFile(PathBuf::from("/tmp/new.json")),
                title: req.title.clone(),
                description: None,
                status: req.status,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: req.repo_associations,
                plan: None,
                done_at: None,
            })
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

    let mut app = App::with_config(Config::default(), Arc::new(CreateBackend));
    app.active_repo_cache = vec![RepoEntry {
        path: PathBuf::from("/repo"),
        source: RepoSource::Explicit,
        git_dir_present: true,
    }];

    // Create with a branch - flag should be set.
    assert!(!app.fetcher_repos_changed);
    let result = app.create_work_item_with(
        "With branch".into(),
        None,
        vec![PathBuf::from("/repo")],
        "feature/test".into(),
    );
    assert!(result.is_ok());
    assert!(
        app.fetcher_repos_changed,
        "fetcher_repos_changed should be true after creating with a branch",
    );
}

/// F-3: PR list limit is 500, not the original 100.
/// This is a documentation test - the actual limit is a string in
/// the gh CLI command. We verify the constant through the source.
#[test]
fn pr_list_limit_is_500() {
    // Read the source to verify the limit. This is a safeguard
    // against regressions back to 100.
    let source = include_str!("../../github_client/real.rs");
    assert!(
        source.contains(r#""500""#) && source.contains(r#""--limit""#),
        "PR list limit should be 500 to avoid silent truncation in busy repos",
    );
}

/// PR list calls must include --author @me to filter to the
/// authenticated user's PRs. Without this, repos with 5000+ open PRs
/// return foreign PRs and may not include the user's own.
#[test]
fn pr_list_uses_author_me() {
    let source = include_str!("../../github_client/real.rs");
    assert!(
        source.contains(r#""--author""#) && source.contains(r#""@me""#),
        "PR list calls should include --author @me to filter to user's PRs",
    );
}

/// `FetchStarted` message triggers a status bar activity, cleared on
/// `RepoData` arrival.
#[test]
fn fetch_started_shows_activity() {
    let mut app = App::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.fetch_rx = Some(rx);

    tx.send(FetchMessage::FetchStarted).unwrap();

    app.drain_fetch_results();
    assert!(app.structural_fetch_activity.is_some());
    assert!(app.current_activity().is_some());

    // Sending RepoData should clear the activity.
    tx.send(FetchMessage::RepoData(Box::new(RepoFetchResult {
        repo_path: PathBuf::from("/repo"),
        github_remote: None,
        worktrees: Ok(vec![]),
        prs: Ok(vec![]),
        review_requested_prs: Ok(vec![]),
        current_user_login: None,
        issues: vec![],
    })))
    .unwrap();

    app.drain_fetch_results();
    assert!(app.structural_fetch_activity.is_none());
}

/// `FetcherError` also clears the fetch activity.
#[test]
fn fetch_started_cleared_on_error() {
    let mut app = App::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.fetch_rx = Some(rx);

    tx.send(FetchMessage::FetchStarted).unwrap();
    app.drain_fetch_results();
    assert!(app.structural_fetch_activity.is_some());

    tx.send(FetchMessage::FetcherError {
        repo_path: PathBuf::from("/repo"),
        error: "test error".into(),
    })
    .unwrap();

    app.drain_fetch_results();
    assert!(app.structural_fetch_activity.is_none());
}

/// Multiple `FetchStarted` messages should not create duplicate activities.
#[test]
fn fetch_started_deduplicates() {
    let mut app = App::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.fetch_rx = Some(rx);

    tx.send(FetchMessage::FetchStarted).unwrap();
    tx.send(FetchMessage::FetchStarted).unwrap();

    app.drain_fetch_results();
    assert_eq!(app.activities.len(), 1);
}

/// Spinner persists until all in-flight repos finish, not just the first.
#[test]
fn fetch_activity_persists_until_all_repos_finish() {
    let mut app = App::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.fetch_rx = Some(rx);

    // Two repos start fetching.
    tx.send(FetchMessage::FetchStarted).unwrap();
    tx.send(FetchMessage::FetchStarted).unwrap();
    app.drain_fetch_results();
    assert!(app.structural_fetch_activity.is_some());

    // First repo finishes - spinner should persist.
    tx.send(FetchMessage::RepoData(Box::new(RepoFetchResult {
        repo_path: PathBuf::from("/repo-a"),
        github_remote: None,
        worktrees: Ok(vec![]),
        prs: Ok(vec![]),
        review_requested_prs: Ok(vec![]),
        current_user_login: None,
        issues: vec![],
    })))
    .unwrap();
    app.drain_fetch_results();
    assert!(
        app.structural_fetch_activity.is_some(),
        "spinner should persist while second repo is still fetching",
    );

    // Second repo finishes - now spinner should clear.
    tx.send(FetchMessage::RepoData(Box::new(RepoFetchResult {
        repo_path: PathBuf::from("/repo-b"),
        github_remote: None,
        worktrees: Ok(vec![]),
        prs: Ok(vec![]),
        review_requested_prs: Ok(vec![]),
        current_user_login: None,
        issues: vec![],
    })))
    .unwrap();
    app.drain_fetch_results();
    assert!(app.structural_fetch_activity.is_none());
}

// -- Round 4 regression tests --

/// F-1: Canonicalized repo paths in `active_repo_cache` match fetcher
/// cache keys. A symlinked repo path in config should resolve to its
/// canonical form so that `repo_data` lookups by the assembly layer
/// succeed.
#[test]
fn active_repo_cache_uses_canonical_paths() {
    // Create a real directory and a symlink to it.
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let real_path = dir.join("real-repo");
    let link_path = dir.join("link-repo");
    std::fs::create_dir_all(real_path.join(".git")).unwrap();

    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_path, &link_path).unwrap();
    #[cfg(not(unix))]
    {
        // On non-Unix, skip the symlink test.
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    // Add the symlink path as an explicit repo.
    let mut cfg = Config::default();
    cfg.add_repo(link_path.to_str().unwrap()).unwrap();

    let app = App::with_config(cfg, Arc::new(StubBackend));

    // The active_repo_cache should contain the canonical (real) path,
    // not the symlink path.
    assert_eq!(app.active_repo_cache.len(), 1);
    let cached_path = &app.active_repo_cache[0].path;
    let canonical_real = crate::config::canonicalize_path(&real_path).unwrap();
    assert_eq!(
        *cached_path,
        canonical_real,
        "active_repo_cache should contain canonical path {}, got {}",
        canonical_real.display(),
        cached_path.display(),
    );

    // Verify that repo_data keyed by the canonical path would be found.
    // Simulate: fetcher sends data keyed by cached_path, assembly looks
    // up by the same path.
    let mut repo_data = std::collections::HashMap::new();
    repo_data.insert(
        cached_path.clone(),
        crate::work_item::RepoFetchResult {
            repo_path: cached_path.clone(),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    );
    assert!(
        repo_data.contains_key(cached_path),
        "repo_data lookup by canonical path should succeed",
    );
}
