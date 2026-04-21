//! In-memory `MockBackend` for tests.
//!
//! A simple, thread-safe `WorkItemBackend` implementation that keeps
//! records in a `Mutex<Vec<_>>`. Intended as a shared starting point
//! for tests across the crate that need a real-enough backend without
//! touching the filesystem. The ad-hoc test stubs currently scattered
//! through `src/app.rs` can migrate to this in a follow-up.
//!
//! This module is `#[cfg(test)]` so it does not ship in release
//! builds.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::work_item::{ReviewRequestedPr, UnlinkedPr, WorkItemId, WorkItemKind, WorkItemStatus};

use super::{
    ActivityEntry, BackendError, CreateWorkItem, ListResult, PrIdentityRecord,
    RepoAssociationRecord, WorkItemBackend, WorkItemRecord,
};

/// In-memory backend that stores work items in a `Mutex<Vec<_>>`.
///
/// Invariants:
/// - IDs are synthesized as `WorkItemId::LocalFile(PathBuf::from(format!("/mock/{n}.json")))`
///   where `n` is a monotonically increasing counter.
/// - `create` returns `Validation` for an empty `repo_associations`,
///   matching the contract `LocalFileBackend::create` enforces.
pub struct MockBackend {
    inner: Mutex<MockState>,
}

struct MockState {
    records: Vec<WorkItemRecord>,
    next_id: u64,
}

impl MockBackend {
    /// Build a fresh, empty `MockBackend`.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockState {
                records: Vec::new(),
                next_id: 1,
            }),
        }
    }

    fn allocate_id(state: &mut MockState) -> WorkItemId {
        let n = state.next_id;
        state.next_id += 1;
        WorkItemId::LocalFile(PathBuf::from(format!("/mock/{n}.json")))
    }

    fn with_state<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut MockState) -> R,
    {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkItemBackend for MockBackend {
    fn list(&self) -> Result<ListResult, BackendError> {
        self.with_state(|state| {
            Ok(ListResult {
                records: state.records.clone(),
                corrupt: Vec::new(),
            })
        })
    }

    fn read(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError> {
        self.with_state(|state| {
            state
                .records
                .iter()
                .find(|r| r.id == *id)
                .cloned()
                .ok_or_else(|| BackendError::NotFound(id.clone()))
        })
    }

    fn create(&self, request: CreateWorkItem) -> Result<WorkItemRecord, BackendError> {
        if request.repo_associations.is_empty() {
            return Err(BackendError::Validation(
                "work item must have at least one repo association".into(),
            ));
        }
        self.with_state(|state| {
            let id = Self::allocate_id(state);
            let record = WorkItemRecord {
                id,
                title: request.title,
                description: request.description,
                status: request.status,
                kind: request.kind,
                display_id: None,
                repo_associations: request.repo_associations,
                plan: None,
                done_at: None,
            };
            state.records.push(record.clone());
            Ok(record)
        })
    }

    fn delete(&self, id: &WorkItemId) -> Result<(), BackendError> {
        self.with_state(|state| {
            let pos = state.records.iter().position(|r| r.id == *id);
            match pos {
                Some(i) => {
                    state.records.remove(i);
                    Ok(())
                }
                None => Err(BackendError::NotFound(id.clone())),
            }
        })
    }

    fn update_status(&self, id: &WorkItemId, status: WorkItemStatus) -> Result<(), BackendError> {
        self.with_state(|state| {
            let record = state
                .records
                .iter_mut()
                .find(|r| r.id == *id)
                .ok_or_else(|| BackendError::NotFound(id.clone()))?;
            record.status = status;
            Ok(())
        })
    }

    fn import(&self, unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError> {
        let request = CreateWorkItem {
            title: unlinked.pr.title.clone(),
            description: None,
            status: WorkItemStatus::Implementing,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: unlinked.repo_path.clone(),
                branch: Some(unlinked.branch.clone()),
                pr_identity: None,
            }],
        };
        self.create(request)
    }

    fn import_review_request(
        &self,
        rr: &ReviewRequestedPr,
    ) -> Result<WorkItemRecord, BackendError> {
        let request = CreateWorkItem {
            title: rr.pr.title.clone(),
            description: None,
            status: WorkItemStatus::Review,
            kind: WorkItemKind::ReviewRequest,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: rr.repo_path.clone(),
                branch: Some(rr.branch.clone()),
                pr_identity: None,
            }],
        };
        self.create(request)
    }

    fn append_activity(
        &self,
        _id: &WorkItemId,
        _entry: &ActivityEntry,
    ) -> Result<(), BackendError> {
        // In-memory backend has no activity log.
        Ok(())
    }

    fn append_activity_existing_only(
        &self,
        _id: &WorkItemId,
        _entry: &ActivityEntry,
    ) -> Result<bool, BackendError> {
        // No active-log file exists, so the "existing only" contract
        // has nothing to append to. Return `Ok(false)` to mirror the
        // `LocalFileBackend` behavior when the log is missing.
        Ok(false)
    }

    fn update_title(&self, id: &WorkItemId, title: &str) -> Result<(), BackendError> {
        self.with_state(|state| {
            let record = state
                .records
                .iter_mut()
                .find(|r| r.id == *id)
                .ok_or_else(|| BackendError::NotFound(id.clone()))?;
            record.title = title.to_string();
            Ok(())
        })
    }

    fn update_branch(
        &self,
        id: &WorkItemId,
        repo_path: &Path,
        branch: &str,
    ) -> Result<(), BackendError> {
        self.with_state(|state| {
            let record = state
                .records
                .iter_mut()
                .find(|r| r.id == *id)
                .ok_or_else(|| BackendError::NotFound(id.clone()))?;
            for assoc in &mut record.repo_associations {
                if assoc.repo_path == repo_path {
                    assoc.branch = Some(branch.to_string());
                }
            }
            Ok(())
        })
    }

    fn update_plan(&self, id: &WorkItemId, plan: &str) -> Result<(), BackendError> {
        self.with_state(|state| {
            let record = state
                .records
                .iter_mut()
                .find(|r| r.id == *id)
                .ok_or_else(|| BackendError::NotFound(id.clone()))?;
            record.plan = Some(plan.to_string());
            Ok(())
        })
    }

    fn read_plan(&self, id: &WorkItemId) -> Result<Option<String>, BackendError> {
        self.with_state(|state| {
            let record = state
                .records
                .iter()
                .find(|r| r.id == *id)
                .ok_or_else(|| BackendError::NotFound(id.clone()))?;
            Ok(record.plan.clone())
        })
    }

    fn set_done_at(&self, id: &WorkItemId, done_at: Option<u64>) -> Result<(), BackendError> {
        self.with_state(|state| {
            let record = state
                .records
                .iter_mut()
                .find(|r| r.id == *id)
                .ok_or_else(|| BackendError::NotFound(id.clone()))?;
            record.done_at = done_at;
            Ok(())
        })
    }

    fn activity_path_for(&self, _id: &WorkItemId) -> Option<PathBuf> {
        None
    }

    fn save_pr_identity(
        &self,
        id: &WorkItemId,
        repo_path: &Path,
        pr_identity: &PrIdentityRecord,
    ) -> Result<(), BackendError> {
        self.with_state(|state| {
            let record = state
                .records
                .iter_mut()
                .find(|r| r.id == *id)
                .ok_or_else(|| BackendError::NotFound(id.clone()))?;
            for assoc in &mut record.repo_associations {
                if assoc.repo_path == repo_path {
                    assoc.pr_identity = Some(pr_identity.clone());
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::MockBackend;
    use crate::work_item::{WorkItemKind, WorkItemStatus};
    use crate::work_item_backend::{
        BackendError, CreateWorkItem, RepoAssociationRecord, WorkItemBackend,
    };

    fn make_request(repo: &str, title: &str) -> CreateWorkItem {
        CreateWorkItem {
            title: title.into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from(repo),
                branch: None,
                pr_identity: None,
            }],
        }
    }

    #[test]
    fn create_list_roundtrip() {
        let backend = MockBackend::new();
        let record = backend.create(make_request("/repo", "first")).unwrap();
        assert_eq!(record.title, "first");

        let result = backend.list().unwrap();
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].title, "first");
    }

    #[test]
    fn create_rejects_empty_repo_associations() {
        let backend = MockBackend::new();
        let err = backend
            .create(CreateWorkItem {
                title: "empty".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: WorkItemKind::Own,
                repo_associations: vec![],
            })
            .unwrap_err();
        assert!(matches!(err, BackendError::Validation(_)));
    }

    #[test]
    fn delete_removes_record() {
        let backend = MockBackend::new();
        let r = backend.create(make_request("/repo", "kill me")).unwrap();
        backend.delete(&r.id).unwrap();
        assert!(backend.list().unwrap().records.is_empty());
    }

    #[test]
    fn update_status_persists() {
        let backend = MockBackend::new();
        let r = backend.create(make_request("/repo", "advance")).unwrap();
        backend
            .update_status(&r.id, WorkItemStatus::Implementing)
            .unwrap();
        let read = backend.read(&r.id).unwrap();
        assert_eq!(read.status, WorkItemStatus::Implementing);
    }
}
