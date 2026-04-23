//! Shared test helpers for the `app::tests` submodule tree. These
//! helpers concentrate the boilerplate mock implementations of
//! `WorktreeService` and `WorkItemBackend` used by the multiple
//! `import_*` / `create_*` tests across `part_03`..`part_05` so each
//! individual test file stays under the 700-line ceiling and each
//! individual test function stays under the 100-line `too_many_lines`
//! clippy ceiling.
//!
//! The mocks here are deliberately minimal: the `WorkItemBackend` impl
//! satisfies only the methods exercised by the import-flow tests,
//! and the `WorktreeService` mock is configurable via a single
//! `fetch_should_fail` flag (fork-PR fetch failure is the only axis
//! any of the call sites actually varies).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{
    ActivityEntry, BackendError, CreateWorkItem, RepoAssociationRecord, WorkItemBackend,
    WorkItemId, WorkItemStatus, WorktreeService,
};
use crate::worktree_service::{WorktreeError, WorktreeInfo};

/// Mock worktree service that records every `create_worktree` call.
///
/// `fetch_should_fail` - when `true`, `fetch_branch` returns a synthetic
/// `GitError` simulating a fork PR whose branch is not reachable on the
/// configured `origin` remote. Used by `import_skips_worktree_when_fetch_fails`.
/// When `false`, `fetch_branch` returns `Ok(())`.
pub struct ImportMockWorktreeService {
    pub created: Mutex<Vec<(PathBuf, String, PathBuf)>>,
    pub fetch_should_fail: bool,
}

impl ImportMockWorktreeService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            created: Mutex::new(Vec::new()),
            fetch_should_fail: false,
        })
    }

    pub fn new_failing_fetch() -> Arc<Self> {
        Arc::new(Self {
            created: Mutex::new(Vec::new()),
            fetch_should_fail: true,
        })
    }
}

impl WorktreeService for ImportMockWorktreeService {
    fn list_worktrees(&self, _repo_path: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        Ok(Vec::new())
    }

    fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
        target_dir: &Path,
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
        _repo_path: &Path,
        _worktree_path: &Path,
        _delete_branch: bool,
        _force: bool,
    ) -> Result<(), WorktreeError> {
        Ok(())
    }

    fn delete_branch(
        &self,
        _repo_path: &Path,
        _branch: &str,
        _force: bool,
    ) -> Result<(), WorktreeError> {
        Ok(())
    }

    fn default_branch(&self, _repo_path: &Path) -> Result<String, WorktreeError> {
        Ok("main".to_string())
    }

    fn github_remote(&self, _repo_path: &Path) -> Result<Option<(String, String)>, WorktreeError> {
        Ok(None)
    }

    fn fetch_branch(&self, _repo_path: &Path, _branch: &str) -> Result<(), WorktreeError> {
        if self.fetch_should_fail {
            Err(WorktreeError::GitError(
                "fatal: couldn't find remote ref fork-branch".into(),
            ))
        } else {
            Ok(())
        }
    }

    fn create_branch(&self, _repo_path: &Path, _branch: &str) -> Result<(), WorktreeError> {
        Ok(())
    }

    fn prune_worktrees(&self, _repo_path: &Path) -> Result<(), WorktreeError> {
        Ok(())
    }
}

/// Minimal `WorkItemBackend` that supports the `import_*` flow. Records
/// every imported record via its `records` mutex.
pub struct ImportTestBackend {
    pub records: Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
}

impl ImportTestBackend {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
        }
    }

    pub fn with_records(records: Vec<crate::work_item_backend::WorkItemRecord>) -> Self {
        Self {
            records: Mutex::new(records),
        }
    }
}

impl WorkItemBackend for ImportTestBackend {
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

    fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
        Ok(crate::work_item_backend::ListResult {
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

    fn update_status(&self, _id: &WorkItemId, _status: WorkItemStatus) -> Result<(), BackendError> {
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

    fn activity_path_for(&self, _id: &WorkItemId) -> Option<PathBuf> {
        None
    }
}
