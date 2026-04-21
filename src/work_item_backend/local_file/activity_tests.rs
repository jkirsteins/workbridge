//! Activity-log, plan, `done_at`, and `pr_identity` tests for
//! `LocalFileBackend`. Also covers delete-time archival behavior
//! because it is coupled to activity-log files.

use std::fs;
use std::path::PathBuf;

use super::LocalFileBackend;
use super::test_helpers::temp_dir;
use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::{
    ActivityEntry, CreateWorkItem, PrIdentityRecord, RepoAssociationRecord, WorkItemBackend,
    WorkItemRecord,
};

#[test]
fn delete_archives_activity_log() {
    let (_tmp, dir) = temp_dir("delete-archive");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Item with activity".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: Some("main".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    backend
        .append_activity(
            &record.id,
            &ActivityEntry {
                timestamp: "2026-04-14T10:00:00Z".into(),
                event_type: "stage_change".into(),
                payload: serde_json::json!({"from": "Backlog", "to": "Implementing"}),
            },
        )
        .unwrap();
    backend
        .append_activity(
            &record.id,
            &ActivityEntry {
                timestamp: "2026-04-14T11:00:00Z".into(),
                event_type: "stage_change".into(),
                payload: serde_json::json!({"from": "Implementing", "to": "Done"}),
            },
        )
        .unwrap();

    let active_path = backend.activity_path(&record.id).unwrap();
    let original_contents = fs::read_to_string(&active_path).unwrap();

    backend.delete(&record.id).unwrap();

    if let WorkItemId::LocalFile(ref path) = record.id {
        assert!(!path.exists(), "work item JSON should be gone");
    }
    assert!(
        !active_path.exists(),
        "active activity log path should be empty after archival"
    );

    let archive_path = dir.join("archive").join(active_path.file_name().unwrap());
    assert!(
        archive_path.exists(),
        "activity log should have been moved to {}",
        archive_path.display()
    );
    let archived_contents = fs::read_to_string(&archive_path).unwrap();
    assert_eq!(
        archived_contents, original_contents,
        "archived log must preserve the original bytes"
    );
}

#[test]
fn delete_creates_archive_dir_if_missing() {
    let (_tmp, dir) = temp_dir("delete-archive-dir-created");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Trigger archive dir creation".into(),
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

    backend
        .append_activity(
            &record.id,
            &ActivityEntry {
                timestamp: "2026-04-14T12:00:00Z".into(),
                event_type: "note".into(),
                payload: serde_json::json!({}),
            },
        )
        .unwrap();

    let archive_dir = dir.join("archive");
    assert!(
        !archive_dir.exists(),
        "precondition: archive dir should not exist yet"
    );

    backend.delete(&record.id).unwrap();

    assert!(
        archive_dir.exists() && archive_dir.is_dir(),
        "delete should create the archive directory on demand"
    );
}

#[test]
fn delete_without_activity_log_is_ok() {
    // Regression: deletion must tolerate a missing activity log.
    // `create()` normally seeds a log, so this test explicitly
    // removes it to simulate the no-log scenario (e.g. a record
    // imported before the seeding-on-create change, or one whose
    // log was cleaned up out of band).
    let (_tmp, dir) = temp_dir("delete-no-activity");
    let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "No activity".into(),
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

    let active_path = backend.activity_path(&record.id).unwrap();
    fs::remove_file(&active_path).expect("remove seeded log for precondition");
    assert!(
        !active_path.exists(),
        "precondition: no activity log for this item"
    );

    backend.delete(&record.id).unwrap();

    let archive_dir = dir.join("archive");
    assert!(
        !archive_dir.exists(),
        "archive dir should not be created when there is nothing to archive"
    );
}

#[test]
fn activity_log_append_and_read_roundtrip() {
    let (_tmp, dir) = temp_dir("activity-roundtrip");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Activity test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: Some("main".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    // Append two entries. `create()` already seeds a `created`
    // event, so the roundtrip should yield three entries total
    // with the created event in position 0.
    let entry1 = ActivityEntry {
        timestamp: "1000Z".into(),
        event_type: "stage_change".into(),
        payload: serde_json::json!({"from": "Backlog", "to": "Implementing"}),
    };
    let entry2 = ActivityEntry {
        timestamp: "2000Z".into(),
        event_type: "note".into(),
        payload: serde_json::json!({"message": "started work"}),
    };
    backend.append_activity(&record.id, &entry1).unwrap();
    backend.append_activity(&record.id, &entry2).unwrap();

    // Read back and verify.
    let entries = backend.read_activity(&record.id).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].event_type, "created");
    assert_eq!(
        entries[0].payload["initial_status"].as_str(),
        Some("Implementing")
    );
    assert_eq!(entries[1].event_type, "stage_change");
    assert_eq!(entries[1].timestamp, "1000Z");
    assert_eq!(entries[2].event_type, "note");
    assert_eq!(entries[2].timestamp, "2000Z");
    assert_eq!(entries[2].payload["message"], "started work");
}

#[test]
fn create_seeds_activity_log_with_created_event() {
    // `create()` seeds the activity log with a single `created`
    // event capturing the initial status. This is what lets the
    // metrics dashboard count freshly created items in
    // `created_per_day` and in the current-backlog trailing edge
    // before any subsequent stage_change happens.
    let (_tmp, dir) = temp_dir("activity-seeded-on-create");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Seeded".into(),
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

    let entries = backend.read_activity(&record.id).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "new item should have exactly one seeded `created` event"
    );
    assert_eq!(entries[0].event_type, "created");
    assert_eq!(
        entries[0].payload["initial_status"].as_str(),
        Some("Backlog")
    );
    // Timestamp is `{secs}Z`; just verify the suffix and that the
    // numeric portion parses as a plausible epoch second.
    let ts = &entries[0].timestamp;
    assert!(ts.ends_with('Z'), "timestamp should end with Z: {ts}");
    let secs: i64 = ts.trim_end_matches('Z').parse().expect("numeric secs");
    assert!(
        secs > 1_600_000_000,
        "timestamp should be a real epoch: {ts}"
    );
}

#[test]
fn plan_storage_roundtrip() {
    let (_tmp, dir) = temp_dir("plan-roundtrip");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Plan test".into(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: Some("feature/plan".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    // Initially no plan.
    let plan = backend.read_plan(&record.id).unwrap();
    assert_eq!(plan, None, "new item should have no plan");

    // Set a plan.
    backend
        .update_plan(&record.id, "Step 1: implement feature\nStep 2: test")
        .unwrap();

    // Read it back.
    let plan = backend.read_plan(&record.id).unwrap();
    assert_eq!(
        plan,
        Some("Step 1: implement feature\nStep 2: test".to_string()),
    );

    // Update the plan.
    backend.update_plan(&record.id, "Revised plan").unwrap();
    let plan = backend.read_plan(&record.id).unwrap();
    assert_eq!(plan, Some("Revised plan".to_string()));

    // Verify the plan persists through list().
    let result = backend.list().unwrap();
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].plan, Some("Revised plan".to_string()),);
}

#[test]
fn activity_path_for_returns_path() {
    let (_tmp, dir) = temp_dir("activity-path");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Path test".into(),
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

    let path = backend.activity_path_for(&record.id);
    assert!(path.is_some(), "should return an activity log path");
    let path = path.unwrap();
    assert!(
        path.to_string_lossy().contains("activity-"),
        "path should contain 'activity-' prefix, got: {}",
        path.display(),
    );
    assert!(
        path.to_string_lossy().ends_with(".jsonl"),
        "path should end with .jsonl, got: {}",
        path.display(),
    );
}

#[test]
fn serde_migration_done_at_defaults_to_none() {
    let json = r#"{"id":{"LocalFile":"/tmp/test.json"},"title":"Test","status":"Done","repo_associations":[]}"#;
    let record: WorkItemRecord = serde_json::from_str(json).unwrap();
    assert_eq!(record.done_at, None);
}

#[test]
fn serde_done_at_roundtrip() {
    let json = r#"{"id":{"LocalFile":"/tmp/test.json"},"title":"Test","status":"Done","repo_associations":[],"done_at":1712345678}"#;
    let record: WorkItemRecord = serde_json::from_str(json).unwrap();
    assert_eq!(record.done_at, Some(1_712_345_678));
}

#[test]
fn set_done_at_roundtrip() {
    let (_tmp, dir) = temp_dir("set-done-at");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let record = backend
        .create(CreateWorkItem {
            title: "Done test".into(),
            description: None,
            kind: WorkItemKind::Own,
            status: WorkItemStatus::Done,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: Some("main".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    // Initially no done_at.
    let result = backend.list().unwrap();
    assert_eq!(result.records[0].done_at, None);

    // Set done_at.
    backend.set_done_at(&record.id, Some(1_000_000)).unwrap();
    let result = backend.list().unwrap();
    assert_eq!(result.records[0].done_at, Some(1_000_000));

    // Clear done_at.
    backend.set_done_at(&record.id, None).unwrap();
    let result = backend.list().unwrap();
    assert_eq!(result.records[0].done_at, None);
}

#[test]
fn save_pr_identity_roundtrip() {
    let (_tmp, dir) = temp_dir("pr-identity");
    let backend = LocalFileBackend::with_dir(dir).unwrap();

    let repo = PathBuf::from("/my/repo");
    let record = backend
        .create(CreateWorkItem {
            title: "PR identity test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo.clone(),
                branch: Some("feature-x".into()),
                pr_identity: None,
            }],
        })
        .unwrap();

    // Initially no pr_identity.
    let read_back = backend.read(&record.id).unwrap();
    assert!(
        read_back.repo_associations[0].pr_identity.is_none(),
        "new record should have no pr_identity",
    );

    // Save a PR identity.
    let identity = PrIdentityRecord {
        number: 42,
        title: "Ship the feature".into(),
        url: "https://github.com/o/r/pull/42".into(),
    };
    backend
        .save_pr_identity(&record.id, &repo, &identity)
        .unwrap();

    // Read back and verify it persisted.
    let read_back = backend.read(&record.id).unwrap();
    let saved = read_back.repo_associations[0]
        .pr_identity
        .as_ref()
        .expect("pr_identity should be set after save");
    assert_eq!(saved.number, 42);
    assert_eq!(saved.title, "Ship the feature");
    assert_eq!(saved.url, "https://github.com/o/r/pull/42");

    // Verify it survives a list() roundtrip too.
    let result = backend.list().unwrap();
    let listed = result.records[0].repo_associations[0]
        .pr_identity
        .as_ref()
        .expect("pr_identity should survive list roundtrip");
    assert_eq!(listed.number, 42);
}
