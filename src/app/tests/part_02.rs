//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// F-2: Unmanaging a repo prunes stale fetch cache entries.
/// After fetcher restart, `repo_data` for removed repos should be
/// cleared so stale data stops rendering.
#[test]
fn unmanage_prunes_stale_repo_data() {
    let mut app = App::new();

    // Simulate fetched data for two repos.
    let repo_a = PathBuf::from("/repos/alpha");
    let repo_b = PathBuf::from("/repos/beta");
    app.repo_data.insert(
        repo_a.clone(),
        crate::work_item::RepoFetchResult {
            repo_path: repo_a,
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    );
    app.repo_data.insert(
        repo_b.clone(),
        crate::work_item::RepoFetchResult {
            repo_path: repo_b,
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    );

    assert_eq!(app.repo_data.len(), 2);

    // Simulate the prune logic from main.rs: only keep repos that
    // are in the new active list (which is empty for a default app).
    let new_repos: Vec<PathBuf> = app
        .active_repo_cache
        .iter()
        .filter(|r| r.git_dir_present)
        .map(|r| r.path.clone())
        .collect();
    app.repo_data.retain(|k, _| new_repos.contains(k));

    assert!(
        app.repo_data.is_empty(),
        "repo_data should be pruned when no active repos remain, got {} entries",
        app.repo_data.len(),
    );
}

/// F-3: Worktree fetch failures are surfaced in the status bar,
/// not silently treated as "no worktrees".
#[test]
fn worktree_fetch_error_surfaces_in_status() {
    use crate::worktree_service::WorktreeError;

    let mut app = App::new();

    // Create a channel and feed it a result with a worktree error.
    let (tx, rx) = std::sync::mpsc::channel();
    app.fetch_rx = Some(rx);

    let repo_path = PathBuf::from("/repos/broken");
    tx.send(FetchMessage::RepoData(Box::new(
        crate::work_item::RepoFetchResult {
            repo_path: repo_path.clone(),
            github_remote: None,
            worktrees: Err(WorktreeError::GitError("not a git repository".into())),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    )))
    .unwrap();

    let received = app.drain_fetch_results();
    assert!(received, "should have received a message");

    // The status message should mention the worktree error.
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Worktree error") && msg.contains("not a git repository"),
        "expected worktree error in status, got: {msg}",
    );

    // The error should be tracked per repo to avoid re-showing.
    assert!(
        app.worktree_errors_shown.contains(&repo_path),
        "repo should be in worktree_errors_shown set",
    );

    // Sending a second error for the same repo should NOT overwrite
    // the status message.
    app.shell.status_message = Some("other message".into());
    tx.send(FetchMessage::RepoData(Box::new(
        crate::work_item::RepoFetchResult {
            repo_path,
            github_remote: None,
            worktrees: Err(WorktreeError::GitError("still broken".into())),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    )))
    .unwrap();
    app.drain_fetch_results();
    assert_eq!(
        app.shell.status_message.as_deref(),
        Some("other message"),
        "second worktree error for same repo should not overwrite status",
    );
}

// -- Round 5 regression tests --

/// F-1: Selection survives reassembly when items reorder.
/// After backend records change order, the same `WorkItemId` should
/// remain selected even if its display index changes.
#[test]
fn selection_survives_reassembly_when_items_reorder() {
    use crate::work_item_backend::ListResult;

    /// Backend that returns records in a controllable order.
    struct OrderableBackend {
        records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
    }

    impl WorkItemBackend for OrderableBackend {
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

    let id_a = WorkItemId::LocalFile(PathBuf::from("/data/aaa.json"));
    let id_b = WorkItemId::LocalFile(PathBuf::from("/data/bbb.json"));

    let record_a = crate::work_item_backend::WorkItemRecord {
        display_id: None,
        id: id_a.clone(),
        title: "Item A".into(),
        description: None,
        status: WorkItemStatus::Backlog,
        kind: crate::work_item::WorkItemKind::Own,
        repo_associations: vec![RepoAssociationRecord {
            repo_path: PathBuf::from("/repo"),
            branch: None,
            pr_identity: None,
        }],
        plan: None,
        done_at: None,
    };
    let record_b = crate::work_item_backend::WorkItemRecord {
        display_id: None,
        id: id_b.clone(),
        title: "Item B".into(),
        description: None,
        status: WorkItemStatus::Backlog,
        kind: crate::work_item::WorkItemKind::Own,
        repo_associations: vec![RepoAssociationRecord {
            repo_path: PathBuf::from("/repo"),
            branch: None,
            pr_identity: None,
        }],
        plan: None,
        done_at: None,
    };

    // Start with order A, B.
    let backend = OrderableBackend {
        records: std::sync::Mutex::new(vec![record_a, record_b]),
    };
    let mut app = App::with_config(Config::default(), Arc::new(backend));

    // Select Item B (the second Todo item).
    app.select_next_item(); // selects first item (A)
    app.select_next_item(); // selects second item (B)

    let selected_id = app.selected_work_item_id();
    assert_eq!(
        selected_id,
        Some(id_b.clone()),
        "should have selected Item B",
    );
    let old_index = app.selected_item;

    // Reverse the order to B, A and reassemble. We simulate this by
    // directly setting work_items in reversed order since we cannot
    // mutate the backend through the trait interface.
    app.work_items = vec![
        crate::work_item::WorkItem {
            display_id: None,
            id: id_b.clone(),
            backend_type: crate::work_item::BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Item B".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            errors: vec![],
        },
        crate::work_item::WorkItem {
            display_id: None,
            id: id_a,
            backend_type: crate::work_item::BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Item A".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            errors: vec![],
        },
    ];
    app.build_display_list();

    // After rebuild, selection should still point to Item B.
    let new_selected_id = app.selected_work_item_id();
    assert_eq!(
        new_selected_id,
        Some(id_b),
        "selection should still be Item B after reorder",
    );

    // The index should have changed since B moved from position 2 to 1.
    let new_index = app.selected_item;
    assert_ne!(
        old_index, new_index,
        "display index should change when items reorder",
    );
}

/// Helper for ACTIVE-group sort tests: build a minimal `WorkItem` for
/// a given repo path and status. Title is purely informational for
/// assertion messages.
pub fn active_sort_test_item(
    title: &str,
    status: WorkItemStatus,
    repo: &str,
) -> crate::work_item::WorkItem {
    crate::work_item::WorkItem {
        id: WorkItemId::LocalFile(PathBuf::from(format!("/tmp/{title}.json"))),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: title.to_string(),
        display_id: None,
        description: None,
        status,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from(repo),
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

/// Inside a single `ACTIVE (<repo>)` sub-group, items must be sorted
/// by reverse workflow stage (MQ -> RV -> IM -> PL) regardless of
/// the backend's insertion order. Within a single stage, the relative
/// order from backend path order is preserved (stable sort).
#[test]
fn build_display_list_sorts_active_group_by_stage() {
    let mut app = App::new();
    // Insert in a deliberately non-workflow order. Two PL items,
    // two IM items, one RV, one MQ - all in the same repo.
    app.work_items = vec![
        active_sort_test_item("a", WorkItemStatus::Implementing, "/repo"),
        active_sort_test_item("b", WorkItemStatus::Planning, "/repo"),
        active_sort_test_item("c", WorkItemStatus::Review, "/repo"),
        active_sort_test_item("d", WorkItemStatus::Planning, "/repo"),
        active_sort_test_item("e", WorkItemStatus::Implementing, "/repo"),
        active_sort_test_item("f", WorkItemStatus::Mergequeue, "/repo"),
    ];
    app.build_display_list();

    // Find the single ACTIVE (repo) group header and collect the
    // work-item indices that follow it until the next header.
    let mut header_idx = None;
    let mut header_count = None;
    for (i, entry) in app.display_list.iter().enumerate() {
        if let DisplayEntry::GroupHeader { label, count, .. } = entry
            && label.starts_with("ACTIVE ")
        {
            header_idx = Some(i);
            header_count = Some(*count);
            break;
        }
    }
    let header_idx = header_idx.expect("expected an ACTIVE group header");
    assert_eq!(
        header_count,
        Some(6),
        "ACTIVE group header count should match item count",
    );

    // Gather ordered titles from the entries that follow the header.
    let mut ordered_titles: Vec<&str> = Vec::new();
    for entry in app.display_list.iter().skip(header_idx + 1) {
        match entry {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                ordered_titles.push(app.work_items[*wi_idx].title.as_str());
            }
            _ => break,
        }
    }

    // Expected: MQ first (f), then RV (c), then IM items (a, e in
    // original order), then PL items (b, d in original order).
    assert_eq!(
        ordered_titles,
        vec!["f", "c", "a", "e", "b", "d"],
        "ACTIVE group items should sort MQ -> RV -> IM -> PL \
         with backend order preserved within each stage",
    );
}

/// Single-stage ACTIVE buckets must preserve the original backend
/// order as the stable-sort tiebreaker. This guards against a future
/// refactor that swaps in an unstable sort.
#[test]
fn push_repo_groups_preserves_single_stage_ordering() {
    let mut app = App::new();
    app.work_items = vec![
        active_sort_test_item("x", WorkItemStatus::Implementing, "/repo"),
        active_sort_test_item("y", WorkItemStatus::Implementing, "/repo"),
        active_sort_test_item("z", WorkItemStatus::Implementing, "/repo"),
    ];
    app.build_display_list();

    let header_idx = app
        .display_list
        .iter()
        .position(|e| {
            matches!(
                e,
                DisplayEntry::GroupHeader { label, .. } if label.starts_with("ACTIVE ")
            )
        })
        .expect("expected an ACTIVE group header");

    let mut ordered_titles: Vec<&str> = Vec::new();
    for entry in app.display_list.iter().skip(header_idx + 1) {
        match entry {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                ordered_titles.push(app.work_items[*wi_idx].title.as_str());
            }
            _ => break,
        }
    }
    assert_eq!(
        ordered_titles,
        vec!["x", "y", "z"],
        "single-stage ACTIVE bucket must preserve original order",
    );
}

/// F-1: `LocalFileBackend::list()` returns records sorted by path for
/// deterministic enumeration. `read_dir` order is filesystem-dependent,
/// so sorting ensures stable display indices.
#[test]
fn backend_list_returns_sorted_records() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let backend = crate::work_item_backend::LocalFileBackend::with_dir(dir.clone()).unwrap();

    // Create items with names that would sort differently than creation order.
    // File names are UUIDs, so we write files directly with known names.
    let names = ["zzz.json", "aaa.json", "mmm.json"];
    for name in &names {
        let record = crate::work_item_backend::WorkItemRecord {
            display_id: None,
            id: WorkItemId::LocalFile(dir.join(name)),
            title: format!("Item {name}"),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                pr_identity: None,
            }],
            plan: None,
            done_at: None,
        };
        let json = serde_json::to_string_pretty(&record).unwrap();
        std::fs::write(dir.join(name), json).unwrap();
    }

    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 3);

    // Records should be sorted by path.
    let paths: Vec<_> = result
        .records
        .iter()
        .map(|r| match &r.id {
            WorkItemId::LocalFile(p) => p.clone(),
            _ => panic!("expected LocalFile"),
        })
        .collect();
    assert_eq!(paths[0], dir.join("aaa.json"));
    assert_eq!(paths[1], dir.join("mmm.json"));
    assert_eq!(paths[2], dir.join("zzz.json"));
}

/// F-3: Fetch errors queued while status bar is occupied eventually
/// surface when the status clears.
#[test]
fn pending_fetch_errors_surface_when_status_clears() {
    let mut app = App::new();

    // Occupy the status bar.
    app.shell.status_message = Some("busy doing something".into());

    // Create a channel and send a FetcherError while status is occupied.
    let (tx, rx) = std::sync::mpsc::channel();
    app.fetch_rx = Some(rx);

    tx.send(FetchMessage::FetcherError {
        repo_path: PathBuf::from("/repo"),
        error: "connection timed out".into(),
    })
    .unwrap();

    // Drain: the error should be queued, not shown.
    app.drain_fetch_results();
    assert_eq!(
        app.shell.status_message.as_deref(),
        Some("busy doing something"),
        "status bar should remain occupied",
    );
    assert_eq!(
        app.pending_fetch_errors.len(),
        1,
        "error should be queued in pending_fetch_errors",
    );

    // Clear the status bar and drain pending errors.
    app.shell.status_message = None;
    app.drain_pending_fetch_errors();

    // The queued error should now be shown.
    assert_eq!(
        app.shell.status_message.as_deref(),
        Some("Fetch error (/repo): connection timed out"),
        "queued error should surface when status clears",
    );
    assert!(
        app.pending_fetch_errors.is_empty(),
        "pending_fetch_errors should be empty after draining",
    );
}

/// F-3: GitHub errors are also queued when status bar is occupied.
#[test]
fn github_errors_queued_when_status_occupied() {
    let mut app = App::new();

    // Occupy the status bar.
    app.shell.status_message = Some("something important".into());

    let (tx, rx) = std::sync::mpsc::channel();
    app.fetch_rx = Some(rx);

    // Send a repo data result with a non-CliNotFound/AuthRequired error.
    tx.send(FetchMessage::RepoData(Box::new(
        crate::work_item::RepoFetchResult {
            repo_path: PathBuf::from("/repo"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Err(crate::github_client::GithubError::ApiError(
                "rate limited".into(),
            )),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues: vec![],
        },
    )))
    .unwrap();

    app.drain_fetch_results();

    // The status should remain unchanged.
    assert_eq!(
        app.shell.status_message.as_deref(),
        Some("something important"),
    );
    // The error should be queued.
    assert_eq!(app.pending_fetch_errors.len(), 1);
    assert!(
        app.pending_fetch_errors[0].contains("rate limited"),
        "queued error should contain the error message, got: {}",
        app.pending_fetch_errors[0],
    );

    // Clear status and drain.
    app.shell.status_message = None;
    app.drain_pending_fetch_errors();
    assert!(
        app.shell
            .status_message
            .as_deref()
            .unwrap_or("")
            .contains("rate limited"),
        "error should surface after status clears",
    );
}
