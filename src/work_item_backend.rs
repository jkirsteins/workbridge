use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::work_item::{BackendType, UnlinkedPr, WorkItemId, WorkItemStatus};

/// Errors from backend operations.
#[derive(Clone, Debug)]
pub enum BackendError {
    /// I/O error reading or writing backend storage.
    Io(String),
    /// Failed to parse a backend record.
    /// Not constructed by LocalFileBackend; reserved for future backends.
    #[allow(dead_code)]
    Parse { path: String, reason: String },
    /// The requested work item was not found.
    NotFound(WorkItemId),
    /// Failed to serialize a record for storage.
    Serialize(String),
    /// Validation error (e.g., no repo associations).
    Validation(String),
    /// The work item ID is not managed by this backend.
    UnsupportedId(WorkItemId),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Io(msg) => write!(f, "backend I/O error: {msg}"),
            BackendError::Parse { path, reason } => {
                write!(f, "backend parse error in {path}: {reason}")
            }
            BackendError::Serialize(msg) => {
                write!(f, "backend serialization error: {msg}")
            }
            BackendError::NotFound(id) => {
                write!(f, "work item not found: {id:?}")
            }
            BackendError::Validation(msg) => {
                write!(f, "validation error: {msg}")
            }
            BackendError::UnsupportedId(id) => {
                write!(f, "work item {id:?} is not managed by this backend")
            }
        }
    }
}

/// A work item record as stored by a backend. Contains only identity and
/// structural data - transient metadata (PR status, git state) is derived
/// by the assembly layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkItemRecord {
    pub id: WorkItemId,
    pub title: String,
    pub status: WorkItemStatus,
    pub repo_associations: Vec<RepoAssociationRecord>,
}

/// A repo association as stored by a backend. Minimal: just the repo path
/// and optional branch name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoAssociationRecord {
    pub repo_path: PathBuf,
    pub branch: Option<String>,
}

/// A corrupt backend record that could not be deserialized.
#[derive(Clone, Debug)]
pub struct CorruptRecord {
    /// Path to the corrupt file on disk.
    pub path: PathBuf,
    /// Human-readable reason the record is corrupt.
    pub reason: String,
}

/// Result of listing backend records, including both valid records and
/// any corrupt entries that could not be parsed.
#[derive(Clone, Debug)]
pub struct ListResult {
    pub records: Vec<WorkItemRecord>,
    pub corrupt: Vec<CorruptRecord>,
}

/// Request to create a new work item. Must have at least one repo
/// association (Invariant 1: work does not happen outside of repos).
/// Backends must return BackendError::Validation if repo_associations
/// is empty.
#[derive(Clone, Debug)]
pub struct CreateWorkItem {
    pub title: String,
    pub status: WorkItemStatus,
    pub repo_associations: Vec<RepoAssociationRecord>,
}

/// Trait for work item storage backends. The backend is responsible for
/// persisting work item records. All derived metadata (PR status, CI
/// checks, git state) is handled by the assembly layer, not the backend.
///
/// v1 uses LocalFileBackend. Future backends include GithubIssueBackend
/// and GithubProjectBackend.
pub trait WorkItemBackend: Send + Sync {
    /// List all work item records from this backend.
    ///
    /// Returns a ListResult containing valid records and any corrupt
    /// entries that could not be parsed. Callers should surface corrupt
    /// entries to the user rather than silently ignoring them.
    fn list(&self) -> Result<ListResult, BackendError>;

    /// Create a new work item and return the created record.
    /// Must return BackendError::Validation if request.repo_associations
    /// is empty (Invariant 1: at least one repo required).
    fn create(&self, request: CreateWorkItem) -> Result<WorkItemRecord, BackendError>;

    /// Delete a work item by id.
    fn delete(&self, id: &WorkItemId) -> Result<(), BackendError>;

    /// Update a work item's status.
    fn update_status(&self, id: &WorkItemId, status: WorkItemStatus) -> Result<(), BackendError>;

    /// Import an unlinked PR as a new work item.
    fn import(&self, unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError>;

    /// Which backend type this implementation represents.
    /// Not called in v1 (assembly derives it from WorkItemId); kept for
    /// multi-backend scenarios where the caller doesn't have the id.
    #[allow(dead_code)]
    fn backend_type(&self) -> BackendType;
}

/// Local filesystem backend that stores each work item as a JSON file.
///
/// Files are stored in a platform-specific data directory (via
/// `directories::ProjectDirs`) under a `work-items/` subdirectory.
/// Each file is named with a UUID v4 and a `.json` extension.
pub struct LocalFileBackend {
    data_dir: PathBuf,
}

impl LocalFileBackend {
    /// Create a new LocalFileBackend using the platform-specific data directory.
    ///
    /// macOS: ~/Library/Application Support/workbridge/work-items/
    /// Linux: ~/.local/share/workbridge/work-items/
    ///
    /// Creates the directory if it does not exist.
    pub fn new() -> Result<Self, BackendError> {
        let proj = directories::ProjectDirs::from("", "", "workbridge")
            .ok_or_else(|| BackendError::Io("could not determine data directory".into()))?;
        let data_dir = proj.data_dir().join("work-items");
        fs::create_dir_all(&data_dir).map_err(|e| {
            BackendError::Io(format!(
                "failed to create data dir {}: {e}",
                data_dir.display()
            ))
        })?;
        Ok(Self { data_dir })
    }

    /// Create a LocalFileBackend with a custom directory (for tests).
    #[cfg(test)]
    pub fn with_dir(dir: PathBuf) -> Result<Self, BackendError> {
        fs::create_dir_all(&dir).map_err(|e| {
            BackendError::Io(format!("failed to create dir {}: {e}", dir.display()))
        })?;
        Ok(Self { data_dir: dir })
    }
}

/// Write data to a file atomically by writing to a temp file in the same
/// directory and then renaming. On POSIX, rename within the same filesystem
/// is atomic, so a crash mid-write leaves the original file intact.
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workitem".into())
    ));
    fs::write(&tmp_path, data)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

impl WorkItemBackend for LocalFileBackend {
    fn list(&self) -> Result<ListResult, BackendError> {
        let entries = fs::read_dir(&self.data_dir).map_err(|e| {
            BackendError::Io(format!(
                "failed to read dir {}: {e}",
                self.data_dir.display()
            ))
        })?;

        let mut records = Vec::new();
        let mut corrupt = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    corrupt.push(CorruptRecord {
                        path: self.data_dir.clone(),
                        reason: format!("failed to read dir entry: {e}"),
                    });
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let contents = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    corrupt.push(CorruptRecord {
                        path: path.clone(),
                        reason: format!("failed to read file: {e}"),
                    });
                    continue;
                }
            };
            match serde_json::from_str::<WorkItemRecord>(&contents) {
                Ok(mut record) => {
                    // Ensure the id reflects the actual file path on disk.
                    record.id = WorkItemId::LocalFile(path);
                    records.push(record);
                }
                Err(e) => {
                    corrupt.push(CorruptRecord {
                        path: path.clone(),
                        reason: format!("corrupt JSON: {e}"),
                    });
                    continue;
                }
            }
        }
        // Sort records by path for deterministic enumeration. read_dir
        // does not guarantee order, so without sorting the display index
        // could point to a different work item after reassembly.
        records.sort_by(|a, b| {
            let path_a = match &a.id {
                WorkItemId::LocalFile(p) => p.as_path(),
                _ => std::path::Path::new(""),
            };
            let path_b = match &b.id {
                WorkItemId::LocalFile(p) => p.as_path(),
                _ => std::path::Path::new(""),
            };
            path_a.cmp(path_b)
        });
        Ok(ListResult { records, corrupt })
    }

    fn create(&self, request: CreateWorkItem) -> Result<WorkItemRecord, BackendError> {
        if request.repo_associations.is_empty() {
            return Err(BackendError::Validation(
                "work item must have at least one repo association".into(),
            ));
        }

        let filename = format!("{}.json", uuid::Uuid::new_v4());
        let path = self.data_dir.join(&filename);

        let record = WorkItemRecord {
            id: WorkItemId::LocalFile(path.clone()),
            title: request.title,
            status: request.status,
            repo_associations: request.repo_associations,
        };

        let json = serde_json::to_string_pretty(&record)
            .map_err(|e| BackendError::Serialize(format!("{e}")))?;

        atomic_write(&path, json.as_bytes())
            .map_err(|e| BackendError::Io(format!("failed to write {}: {e}", path.display())))?;

        Ok(record)
    }

    fn delete(&self, id: &WorkItemId) -> Result<(), BackendError> {
        match id {
            WorkItemId::LocalFile(path) => {
                if !path.exists() {
                    return Err(BackendError::NotFound(id.clone()));
                }
                fs::remove_file(path).map_err(|e| {
                    BackendError::Io(format!("failed to delete {}: {e}", path.display()))
                })?;
                Ok(())
            }
            other => Err(BackendError::UnsupportedId(other.clone())),
        }
    }

    fn update_status(&self, id: &WorkItemId, status: WorkItemStatus) -> Result<(), BackendError> {
        match id {
            WorkItemId::LocalFile(path) => {
                if !path.exists() {
                    return Err(BackendError::NotFound(id.clone()));
                }
                let contents = fs::read_to_string(path).map_err(|e| {
                    BackendError::Io(format!("failed to read {}: {e}", path.display()))
                })?;
                let mut record: WorkItemRecord = serde_json::from_str(&contents).map_err(|e| {
                    BackendError::Io(format!("failed to parse {}: {e}", path.display()))
                })?;
                record.status = status;
                let json = serde_json::to_string_pretty(&record)
                    .map_err(|e| BackendError::Serialize(format!("{e}")))?;
                atomic_write(path, json.as_bytes()).map_err(|e| {
                    BackendError::Io(format!("failed to write {}: {e}", path.display()))
                })?;
                Ok(())
            }
            other => Err(BackendError::UnsupportedId(other.clone())),
        }
    }

    fn import(&self, unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError> {
        let request = CreateWorkItem {
            title: unlinked.pr.title.clone(),
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: unlinked.repo_path.clone(),
                branch: Some(unlinked.branch.clone()),
            }],
        };
        self.create(request)
    }

    fn backend_type(&self) -> BackendType {
        BackendType::LocalFile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("workbridge-test-backend-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn create_and_list_roundtrip() {
        let dir = temp_dir("roundtrip");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let record = backend
            .create(CreateWorkItem {
                title: "Fix auth bug".into(),
                status: WorkItemStatus::Backlog,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/path/to/repo"),
                    branch: Some("42-fix-auth".into()),
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_validates_non_empty_repos() {
        let dir = temp_dir("validate-repos");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let result = backend.create(CreateWorkItem {
            title: "No repos".into(),
            status: WorkItemStatus::Backlog,
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_file() {
        let dir = temp_dir("delete");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let record = backend
            .create(CreateWorkItem {
                title: "To delete".into(),
                status: WorkItemStatus::Implementing,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: None,
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_not_found() {
        let dir = temp_dir("delete-notfound");
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_creates_from_pr() {
        let dir = temp_dir("import");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let unlinked = UnlinkedPr {
            repo_path: PathBuf::from("/my/repo"),
            pr: PrInfo {
                number: 42,
                title: "Fix the widget".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::Passing,
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_surfaces_corrupt_files() {
        let dir = temp_dir("corrupt");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        // Write a corrupt JSON file directly.
        fs::write(dir.join("corrupt.json"), "not valid json {{{").unwrap();

        // Create a valid work item through the backend.
        backend
            .create(CreateWorkItem {
                title: "Valid item".into(),
                status: WorkItemStatus::Backlog,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: Some("main".into()),
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_empty_dir() {
        let dir = temp_dir("empty");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let result = backend.list().unwrap();
        assert!(result.records.is_empty());
        assert!(result.corrupt.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_status_persists() {
        let dir = temp_dir("update-status");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let record = backend
            .create(CreateWorkItem {
                title: "Planning item".into(),
                status: WorkItemStatus::Backlog,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: Some("42-feature".into()),
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_status_not_found() {
        let dir = temp_dir("update-notfound");
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

        let _ = fs::remove_dir_all(&dir);
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
}
