//! Shared test helpers for the `LocalFileBackend` test files.

use std::path::PathBuf;

use crate::work_item::{WorkItemKind, WorkItemStatus};
use crate::work_item_backend::{CreateWorkItem, RepoAssociationRecord};

/// Allocate a fresh tempdir for a test. Returns both the `TempDir`
/// guard (which removes the directory on drop) and a concrete
/// `PathBuf` for ergonomic use. The `_name` argument is retained for
/// call-site self-documentation (the suffix used to encode the test
/// name into the fixed `/tmp/workbridge-test-backend-<name>` path)
/// even though `tempfile::tempdir()` already produces a collision-
/// free name.
pub(super) fn temp_dir(_name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    (tmp, dir)
}

/// Build a minimal `CreateWorkItem` for the display-ID tests: a
/// Backlog item with one repo association and no branch.
pub(super) fn make_request(repo: &str, title: &str) -> CreateWorkItem {
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
