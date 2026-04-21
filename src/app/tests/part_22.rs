//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// Structural fix for the orphan-active-log race: the rebase
/// gate's background thread calls
/// `append_activity_existing_only`, NOT `append_activity`, so a
/// `backend.delete` that already archived the active log cannot
/// be silently reverted by a racing background append. This test
/// exercises the real `LocalFileBackend` code path (no
/// fabricated gate state, no fake backends) to prove that the
/// structural guarantee holds at the filesystem level. The
/// earlier `delete_work_item_cancels_rebase_gate_before_backend_delete`
/// test pins the ordering invariant; this test pins the backend
/// primitive that makes the ordering invariant robust even if a
/// future refactor reorders phases or forgets a cancellation
/// check.
#[test]
fn append_activity_existing_only_does_not_recreate_orphan_after_delete() {
    use crate::work_item_backend::{
        ActivityEntry, CreateWorkItem, LocalFileBackend, RepoAssociationRecord, WorkItemBackend,
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let backend = LocalFileBackend::with_dir(dir).expect("backend must be constructable");

    // Create a work item. `LocalFileBackend::create` seeds an
    // initial `created` activity log entry, so the active log
    // file exists on disk at this point.
    let record = backend
        .create(CreateWorkItem {
            title: "orphan-log-test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/tmp/orphan-repo"),
                branch: Some("feature".into()),
                pr_identity: None,
            }],
        })
        .expect("create must succeed");
    let wi_id = record.id;

    let active_path = backend
        .activity_path_for(&wi_id)
        .expect("activity path must be defined for LocalFileBackend");
    assert!(
        active_path.exists(),
        "sanity check: initial activity log should have been seeded by create",
    );

    // Simulate the main thread's `backend.delete` call that the
    // rebase gate's background thread is racing against.
    backend.delete(&wi_id).expect("delete must succeed");
    assert!(
        !active_path.exists(),
        "backend.delete must archive the active activity log",
    );

    // Now the background thread wakes up to write its
    // `rebase_completed` entry. Under the old
    // `append_activity(create(true))` path this would silently
    // recreate an orphan active log file. The new
    // `append_activity_existing_only` primitive returns
    // `Ok(false)` and leaves the filesystem untouched.
    let entry = ActivityEntry {
        timestamp: "2026-04-16T00:00:00Z".into(),
        event_type: "rebase_completed".into(),
        payload: serde_json::json!({
            "base_branch": "main",
            "conflicts_resolved": false,
            "source": "rebase_gate",
        }),
    };
    let appended = backend
        .append_activity_existing_only(&wi_id, &entry)
        .expect("append_activity_existing_only must not error on ENOENT");
    assert!(
        !appended,
        "append_activity_existing_only must return Ok(false) when the \
         active log was archived by a concurrent delete",
    );
    assert!(
        !active_path.exists(),
        "append_activity_existing_only must NOT recreate the active \
         activity log after delete (that is the orphan bug this \
         primitive exists to prevent)",
    );

    // Witness the bug being fixed: the old `append_activity`
    // path WOULD have created the orphan file. We verify this
    // both to document the hazard in an executable form and to
    // pin the invariant that the two primitives behave
    // differently on ENOENT. The creation is immediately
    // cleaned up so the test leaves the temp dir in a sane
    // shape.
    backend
        .append_activity(&wi_id, &entry)
        .expect("legacy append_activity should succeed (that is the bug)");
    assert!(
        active_path.exists(),
        "legacy append_activity recreates the orphan active log - this \
         assertion documents the hazard append_activity_existing_only \
         exists to avoid",
    );
    let _ = std::fs::remove_file(&active_path);
}

/// Default trait impl: backends that do NOT explicitly override
/// `append_activity_existing_only` MUST get a loud
/// `BackendError::Validation` instead of silently falling
/// through to `append_activity` (which is the orphan-creating
/// call this primitive exists to replace). The reference
/// `LocalFileBackend` overrides it; any future backend that
/// forgets to override it will fail at the first call site
/// instead of silently regressing the orphan-active-log race.
/// This is the round 2 fix for the PR #104 review log entry
/// "Default impl of `append_activity_existing_only` silently
/// re-introduces the bug for any future backend".
#[test]
fn append_activity_existing_only_default_impl_returns_err() {
    use crate::work_item_backend::{
        ActivityEntry, BackendError, CreateWorkItem, ListResult, WorkItemBackend, WorkItemRecord,
    };

    #[derive(Default)]
    struct NonOverridingBackend {
        appended: std::sync::Mutex<Vec<ActivityEntry>>,
    }
    impl WorkItemBackend for NonOverridingBackend {
        fn list(&self) -> Result<ListResult, BackendError> {
            Ok(ListResult {
                records: Vec::new(),
                corrupt: Vec::new(),
            })
        }
        fn read(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::NotFound(id.clone()))
        }
        fn create(&self, _req: CreateWorkItem) -> Result<WorkItemRecord, BackendError> {
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
            entry: &ActivityEntry,
        ) -> Result<(), BackendError> {
            self.appended.lock().unwrap().push(entry.clone());
            Ok(())
        }
        // Deliberately does NOT override append_activity_existing_only;
        // the test asserts that the trait default refuses the call.
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

    let backend = NonOverridingBackend::default();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/non-overriding.json"));
    let entry = ActivityEntry {
        timestamp: "2026-04-16T00:00:00Z".into(),
        event_type: "test".into(),
        payload: serde_json::json!({}),
    };
    let result = backend.append_activity_existing_only(&wi_id, &entry);
    match result {
        Err(BackendError::Validation(msg)) => {
            assert!(
                msg.contains("append_activity_existing_only"),
                "default-impl error message must name the missing \
                 primitive; got: {msg}",
            );
        }
        other => panic!("default impl must return BackendError::Validation; got: {other:?}"),
    }
    assert!(
        backend.appended.lock().unwrap().is_empty(),
        "default impl must NOT forward to append_activity; doing so \
         would silently re-create the orphan-active-log hazard for \
         any future non-LocalFileBackend impl",
    );
}

// -- Feature: ReviewRequest merge auto-close polling --
//
// These tests mirror `poll_mergequeue_*` and
// `reconstruct_mergequeue_watches_*`. They feed poll results into
// `review_request_merge_polls` directly via a locally-owned
// `crossbeam_channel` so nothing ever shells out to real `gh` on
// the test thread. See `docs/UI.md` "Blocking I/O Prohibition".

/// Test backend that records every `save_pr_identity` and
/// `append_activity` call, and applies status / `pr_identity`
/// mutations to its in-memory records so a subsequent
/// `assembly::reassemble` can observe them. Used to verify the
/// merge-gate path end-to-end without touching the filesystem.
#[derive(Default)]
pub struct RrTestBackend {
    pub records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
    pub saved_identities: std::sync::Mutex<Vec<(WorkItemId, PathBuf, PrIdentityRecord)>>,
    pub appended_activities: std::sync::Mutex<Vec<(WorkItemId, ActivityEntry)>>,
}

impl WorkItemBackend for RrTestBackend {
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
        self.records
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == *id)
            .cloned()
            .ok_or_else(|| BackendError::NotFound(id.clone()))
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
    fn append_activity(&self, id: &WorkItemId, entry: &ActivityEntry) -> Result<(), BackendError> {
        self.appended_activities
            .lock()
            .unwrap()
            .push((id.clone(), entry.clone()));
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
    fn save_pr_identity(
        &self,
        id: &WorkItemId,
        repo_path: &std::path::Path,
        pr_identity: &PrIdentityRecord,
    ) -> Result<(), BackendError> {
        self.saved_identities.lock().unwrap().push((
            id.clone(),
            repo_path.to_path_buf(),
            pr_identity.clone(),
        ));
        // Also mutate the in-memory record so the next
        // `assembly::reassemble` can see the persisted identity
        // and fire the `PrInfo` fallback.
        let mut records = self.records.lock().unwrap();
        if let Some(record) = records.iter_mut().find(|r| r.id == *id)
            && let Some(assoc) = record
                .repo_associations
                .iter_mut()
                .find(|a| a.repo_path == repo_path)
        {
            assoc.pr_identity = Some(pr_identity.clone());
        }
        Ok(())
    }
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
}

pub fn make_rr_record(
    name: &str,
    kind: crate::work_item::WorkItemKind,
    status: WorkItemStatus,
    repo_path: &std::path::Path,
    branch: Option<&str>,
) -> crate::work_item_backend::WorkItemRecord {
    crate::work_item_backend::WorkItemRecord {
        id: WorkItemId::LocalFile(PathBuf::from(format!("/tmp/{name}.json"))),
        title: name.into(),
        description: None,
        status,
        kind,
        display_id: None,
        repo_associations: vec![RepoAssociationRecord {
            repo_path: repo_path.to_path_buf(),
            branch: branch.map(String::from),
            pr_identity: None,
        }],
        plan: None,
        done_at: None,
    }
}

pub fn seed_rr_app(
    records: Vec<crate::work_item_backend::WorkItemRecord>,
    repo_path: &std::path::Path,
    seed_remote: bool,
) -> (App, Arc<RrTestBackend>) {
    let backend = Arc::new(RrTestBackend {
        records: std::sync::Mutex::new(records),
        ..Default::default()
    });
    let mut app = App::with_config(Config::for_test(), backend.clone());
    if seed_remote {
        app.repo_data.insert(
            repo_path.to_path_buf(),
            crate::work_item::RepoFetchResult {
                repo_path: repo_path.to_path_buf(),
                github_remote: Some(("owner".into(), "repo".into())),
                worktrees: Ok(Vec::new()),
                prs: Ok(Vec::new()),
                review_requested_prs: Ok(Vec::new()),
                issues: Vec::new(),
                current_user_login: None,
            },
        );
    }
    app.reassemble_work_items();
    // Pin the per-watch cooldown so Phase 2 of
    // `poll_review_request_merges` never spawns a real `gh pr
    // view` subprocess during a unit test. Individual tests that
    // exercise Phase 2 can override this back to None.
    let now = crate::side_effects::clock::instant_now();
    for w in &mut app.review_request_merge_watches {
        w.last_polled = Some(now);
    }
    (app, backend)
}

#[test]
fn reconstruct_review_request_merge_watches_registers_review_request_in_review() {
    let repo_path = PathBuf::from("/tmp/rr-review");
    let rec = make_rr_record(
        "rr1",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr1"),
    );
    let rec_id = rec.id.clone();
    let (app, _backend) = seed_rr_app(vec![rec], &repo_path, true);
    let matches: Vec<_> = app
        .review_request_merge_watches
        .iter()
        .filter(|w| w.wi_id == rec_id)
        .collect();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].owner_repo, "owner/repo");
    assert_eq!(matches[0].branch, "feature/rr1");
    assert_eq!(matches[0].pr_number, None);
}

#[test]
fn reconstruct_review_request_merge_watches_is_idempotent() {
    let repo_path = PathBuf::from("/tmp/rr-idempotent");
    let rec = make_rr_record(
        "rr2",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr2"),
    );
    let (mut app, _backend) = seed_rr_app(vec![rec], &repo_path, true);
    let before = app.review_request_merge_watches.len();
    app.reconstruct_review_request_merge_watches();
    app.reconstruct_review_request_merge_watches();
    assert_eq!(
        app.review_request_merge_watches.len(),
        before,
        "re-running reconstruction must not add duplicate watches"
    );
}

#[test]
fn reconstruct_review_request_merge_watches_skips_when_github_remote_missing() {
    let repo_path = PathBuf::from("/tmp/rr-no-remote");
    let rec = make_rr_record(
        "rr3",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr3"),
    );
    let rec_id = rec.id.clone();
    // seed_remote = false: repo_data is not populated, mirroring
    // the cold-start window before the first fetch completes. The
    // watch must be skipped this cycle and rebuilt on the next
    // reassembly.
    let (app, _backend) = seed_rr_app(vec![rec], &repo_path, false);
    assert!(
        app.review_request_merge_watches
            .iter()
            .all(|w| w.wi_id != rec_id),
        "watch must be skipped when github_remote is missing"
    );
}

#[test]
fn reconstruct_review_request_merge_watches_skips_own_items() {
    let repo_path = PathBuf::from("/tmp/rr-own");
    let rec = make_rr_record(
        "rr4",
        crate::work_item::WorkItemKind::Own,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr4"),
    );
    let rec_id = rec.id.clone();
    let (app, _backend) = seed_rr_app(vec![rec], &repo_path, true);
    assert!(
        app.review_request_merge_watches
            .iter()
            .all(|w| w.wi_id != rec_id),
        "Own-kind items must not be watched (the author-filtered PR path is the source of truth for them)",
    );
}

#[test]
fn reconstruct_review_request_merge_watches_skips_done_items() {
    let repo_path = PathBuf::from("/tmp/rr-done");
    let rec = make_rr_record(
        "rr5",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Done,
        &repo_path,
        Some("feature/rr5"),
    );
    let rec_id = rec.id.clone();
    let (app, _backend) = seed_rr_app(vec![rec], &repo_path, true);
    assert!(
        app.review_request_merge_watches
            .iter()
            .all(|w| w.wi_id != rec_id),
        "ReviewRequest items already in Done must not be re-watched"
    );
}
