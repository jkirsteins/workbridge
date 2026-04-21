//! CRUD, delete, import, list-corruption, and update tests for
//! `LocalFileBackend`.

use std::fs;
use std::path::{Path, PathBuf};

use super::LocalFileBackend;
use super::test_helpers::temp_dir;
use crate::work_item::{
    CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision, UnlinkedPr, WorkItemId,
    WorkItemKind, WorkItemStatus,
};
use crate::work_item_backend::{
    BackendError, CreateWorkItem, RepoAssociationRecord, WorkItemBackend, WorkItemRecord,
};

#[test]
fn create_and_list_roundtrip() {
    let (_tmp, dir) = temp_dir("roundtrip");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Fix auth bug".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/path/to/repo"),
                branch: Some("42-fix-auth".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    assert_eq!(record.title, "Fix auth bug");
    assert_eq!(record.status, WorkItemStatus::Backlog);
    assert_eq!(record.repo_associations.len(), 1);
    assert_eq!(
        record.repo_associations[0].repo_path,
        PathBuf::from("/path/to/repo")
    );
    assert_eq!(
        record.repo_associations[0].branch,
        Some("42-fix-auth".into())
    );

    let result = backend.list().unwrap();
    assert!(result.corrupt.is_empty());
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].title, "Fix auth bug");
    assert_eq!(result.records[0].status, WorkItemStatus::Backlog);
    assert_eq!(result.records[0].repo_associations.len(), 1);
}

#[test]
fn create_validates_non_empty_repos() {
    let (_tmp, dir) = temp_dir("validate-repos");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let result = backend.create(CreateWorkItem {
        title: "No repos".into(),
        description: None,
        status: WorkItemStatus::Backlog,
        kind: WorkItemKind::Own,
        repo_associations: vec![],
    });

    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        BackendError::Validation(msg) => {
            assert!(
                msg.contains("at least one repo association"),
                "unexpected message: {msg}"
            );
        }
        other => panic!("expected Validation error, got: {other}"),
    }
}

#[test]
fn delete_removes_file() {
    let (_tmp, dir) = temp_dir("delete");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "To delete".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                pr_identity: None,
            }],
        })
        .unwrap();

    // Verify the file exists on disk.
    if let WorkItemId::LocalFile(ref path) = record.id {
        assert!(path.exists(), "file should exist after create");
    } else {
        panic!("expected LocalFile id");
    }

    backend.delete(&record.id).unwrap();

    // Verify the file is gone.
    if let WorkItemId::LocalFile(ref path) = record.id {
        assert!(!path.exists(), "file should be gone after delete");
    }

    let result = backend.list().unwrap();
    assert!(result.records.is_empty());
}

#[test]
fn delete_not_found() {
    let (_tmp, dir) = temp_dir("delete-notfound");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let result = backend.delete(&WorkItemId::LocalFile(dir.join("nonexistent.json")));
    assert!(result.is_err());
    match result.unwrap_err() {
        BackendError::NotFound(_) => {}
        other => panic!("expected NotFound, got: {other}"),
    }

    // Non-LocalFile ids should return UnsupportedId.
    let result = backend.delete(&WorkItemId::GithubIssue {
        owner: "foo".into(),
        repo: "bar".into(),
        number: 1,
    });
    assert!(result.is_err());
    match result.unwrap_err() {
        BackendError::UnsupportedId(_) => {}
        other => panic!("expected UnsupportedId, got: {other}"),
    }
}

#[test]
fn import_creates_from_pr() {
    let (_tmp, dir) = temp_dir("import");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let unlinked = UnlinkedPr {
        repo_path: PathBuf::from("/my/repo"),
        pr: PrInfo {
            number: 42,
            title: "Fix the widget".into(),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::Passing,
            mergeable: MergeableState::Unknown,
            url: "https://github.com/org/repo/pull/42".into(),
        },
        branch: "42-fix-widget".into(),
    };

    let record = backend.import(&unlinked).unwrap();
    assert_eq!(record.title, "Fix the widget");
    assert_eq!(record.status, WorkItemStatus::Implementing);
    assert_eq!(record.repo_associations.len(), 1);
    assert_eq!(
        record.repo_associations[0].repo_path,
        PathBuf::from("/my/repo")
    );
    assert_eq!(
        record.repo_associations[0].branch,
        Some("42-fix-widget".into())
    );

    // Verify it persisted.
    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].title, "Fix the widget");
}

#[test]
fn list_surfaces_corrupt_files() {
    let (_tmp, dir) = temp_dir("corrupt");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    // Write a corrupt JSON file directly.
    fs::write(dir.join("corrupt.json"), "not valid json {{{").unwrap();

    // Create a valid work item through the backend.
    backend
        .create(CreateWorkItem {
            title: "Valid item".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: Some("main".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].title, "Valid item");

    // Corrupt file should be surfaced, not silently dropped.
    assert_eq!(result.corrupt.len(), 1);
    assert!(
        result.corrupt[0].path.ends_with("corrupt.json"),
        "corrupt path should reference the corrupt file, got: {}",
        result.corrupt[0].path.display(),
    );
    assert!(
        result.corrupt[0].reason.contains("corrupt JSON"),
        "reason should mention corrupt JSON, got: {}",
        result.corrupt[0].reason,
    );
}

#[test]
fn list_empty_dir() {
    let (_tmp, dir) = temp_dir("empty");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let result = backend.list().unwrap();
    assert!(result.records.is_empty());
    assert!(result.corrupt.is_empty());
}

#[test]
fn update_status_persists() {
    let (_tmp, dir) = temp_dir("update-status");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Planning item".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: Some("42-feature".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    assert_eq!(record.status, WorkItemStatus::Backlog);

    backend
        .update_status(&record.id, WorkItemStatus::Planning)
        .unwrap();

    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].status, WorkItemStatus::Planning);

    // Advance further.
    backend
        .update_status(&record.id, WorkItemStatus::Implementing)
        .unwrap();

    let result = backend.list().unwrap();
    assert_eq!(result.records[0].status, WorkItemStatus::Implementing);
}

#[test]
fn update_status_not_found() {
    let (_tmp, dir) = temp_dir("update-notfound");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let result = backend.update_status(
        &WorkItemId::LocalFile(dir.join("nonexistent.json")),
        WorkItemStatus::Planning,
    );
    assert!(result.is_err());
    match result.unwrap_err() {
        BackendError::NotFound(_) => {}
        other => panic!("expected NotFound, got: {other}"),
    }
}

#[test]
fn update_title_persists() {
    let (_tmp, dir) = temp_dir("update-title");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Quick start".into(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: Some("user/quickstart-ab12".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    backend
        .update_title(&record.id, "Implement dark mode toggle")
        .unwrap();

    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].title, "Implement dark mode toggle");
}

#[test]
fn update_title_not_found() {
    let (_tmp, dir) = temp_dir("update-title-notfound");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let result = backend.update_title(&WorkItemId::LocalFile(dir.join("nonexistent.json")), "Hi");
    assert!(result.is_err());
    match result.unwrap_err() {
        BackendError::NotFound(_) => {}
        other => panic!("expected NotFound, got: {other}"),
    }
}

#[test]
fn update_branch_persists() {
    let (_tmp, dir) = temp_dir("update-branch");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Needs a branch".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                pr_identity: None,
            }],
        })
        .unwrap();

    backend
        .update_branch(&record.id, Path::new("/repo"), "feature/x")
        .unwrap();

    // Re-read from disk (via list) to confirm persistence.
    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(
        result.records[0].repo_associations[0].branch.as_deref(),
        Some("feature/x")
    );
}

#[test]
fn update_branch_not_found() {
    let (_tmp, dir) = temp_dir("update-branch-notfound");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let result = backend.update_branch(
        &WorkItemId::LocalFile(dir.join("nonexistent.json")),
        Path::new("/repo"),
        "feature/x",
    );
    assert!(result.is_err());
    match result.unwrap_err() {
        BackendError::NotFound(_) => {}
        other => panic!("expected NotFound, got: {other}"),
    }
}

#[test]
fn update_branch_only_touches_matching_repo() {
    let (_tmp, dir) = temp_dir("update-branch-scoped");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Multi-repo".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![
                RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo-a"),
                    branch: None,
                    pr_identity: None,
                },
                RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo-b"),
                    branch: Some("existing/b".into()),
                    pr_identity: None,
                },
            ],
        })
        .unwrap();

    backend
        .update_branch(&record.id, Path::new("/repo-a"), "new/a")
        .unwrap();

    let result = backend.list().unwrap();
    let assocs = &result.records[0].repo_associations;
    let repo_a = assocs
        .iter()
        .find(|a| a.repo_path == Path::new("/repo-a"))
        .unwrap();
    let repo_b = assocs
        .iter()
        .find(|a| a.repo_path == Path::new("/repo-b"))
        .unwrap();
    assert_eq!(repo_a.branch.as_deref(), Some("new/a"));
    assert_eq!(
        repo_b.branch.as_deref(),
        Some("existing/b"),
        "unrelated repo association must not be mutated"
    );
}

#[test]
fn serde_migration_todo_to_backlog() {
    let json = r#"{"id":{"LocalFile":"/tmp/test.json"},"title":"Test","status":"Todo","repo_associations":[]}"#;
    let record: WorkItemRecord = serde_json::from_str(json).unwrap();
    assert_eq!(record.status, WorkItemStatus::Backlog);
}

#[test]
fn serde_migration_inprogress_to_implementing() {
    let json = r#"{"id":{"LocalFile":"/tmp/test.json"},"title":"Test","status":"InProgress","repo_associations":[]}"#;
    let record: WorkItemRecord = serde_json::from_str(json).unwrap();
    assert_eq!(record.status, WorkItemStatus::Implementing);
}

#[test]
fn serde_migration_plan_defaults_to_none() {
    // Old records without plan field should deserialize with plan: None.
    let json = r#"{"id":{"LocalFile":"/tmp/test.json"},"title":"Test","status":"Backlog","repo_associations":[]}"#;
    let record: WorkItemRecord = serde_json::from_str(json).unwrap();
    assert_eq!(record.plan, None);
}
