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
    /// Optional description providing context for planning/refinement.
    /// Defaults to None for migration compatibility with existing records.
    #[serde(default)]
    pub description: Option<String>,
    pub status: WorkItemStatus,
    pub repo_associations: Vec<RepoAssociationRecord>,
    /// Implementation plan text. None means no plan has been set yet.
    /// Defaults to None for migration compatibility with existing records.
    #[serde(default)]
    pub plan: Option<String>,
}

/// An entry in a work item's append-only activity log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Timestamp in ISO 8601 format (or epoch seconds with Z suffix).
    pub timestamp: String,
    /// Type of event (e.g., "stage_change", "note", "review_gate").
    pub event_type: String,
    /// Arbitrary JSON payload for the event.
    pub payload: serde_json::Value,
}

/// PR identity snapshot persisted at merge time. Allows the detail view
/// to show PR info after the PR leaves the open-PR list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrIdentityRecord {
    pub number: u64,
    pub title: String,
    pub url: String,
}

/// A repo association as stored by a backend. Minimal: just the repo path
/// and optional branch name.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoAssociationRecord {
    pub repo_path: PathBuf,
    pub branch: Option<String>,
    /// Snapshot of PR identity persisted at merge time.
    #[serde(default)]
    pub pr_identity: Option<PrIdentityRecord>,
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
    pub description: Option<String>,
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

    /// Read a single work item record by ID.
    fn read(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError>;

    /// Create a new work item and return the created record.
    /// Must return BackendError::Validation if request.repo_associations
    /// is empty (Invariant 1: at least one repo required).
    fn create(&self, request: CreateWorkItem) -> Result<WorkItemRecord, BackendError>;

    /// Pre-delete cleanup hook. Called before `delete()` to allow
    /// backend-specific cleanup (e.g., closing a backing GitHub issue,
    /// archiving a project item). Errors are non-fatal - callers should
    /// log warnings but continue with the delete.
    ///
    /// Default implementation is a no-op. Override in future backends
    /// (GithubIssueBackend, GithubProjectBackend) as needed.
    fn pre_delete_cleanup(&self, _id: &WorkItemId) -> Result<(), BackendError> {
        Ok(())
    }

    /// Delete a work item by id.
    fn delete(&self, id: &WorkItemId) -> Result<(), BackendError>;

    /// Update a work item's status.
    fn update_status(&self, id: &WorkItemId, status: WorkItemStatus) -> Result<(), BackendError>;

    /// Import an unlinked PR as a new work item.
    fn import(&self, unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError>;

    /// Append an activity entry to a work item's activity log.
    fn append_activity(&self, id: &WorkItemId, entry: &ActivityEntry) -> Result<(), BackendError>;

    /// Read all activity entries for a work item.
    fn read_activity(&self, id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError>;

    /// Update the implementation plan for a work item.
    fn update_plan(&self, id: &WorkItemId, plan: &str) -> Result<(), BackendError>;

    /// Read the implementation plan for a work item.
    fn read_plan(&self, id: &WorkItemId) -> Result<Option<String>, BackendError>;

    /// Get the activity log file path for a work item. Returns None if the
    /// backend does not support file-based activity logs.
    fn activity_path_for(&self, id: &WorkItemId) -> Option<PathBuf>;

    /// Persist a PR identity snapshot on the repo association matching
    /// `repo_path`. Default no-op for backends that don't support it.
    fn save_pr_identity(
        &self,
        _id: &WorkItemId,
        _repo_path: &Path,
        _pr_identity: &PrIdentityRecord,
    ) -> Result<(), BackendError> {
        Ok(())
    }

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

    /// Compute the activity log file path for a work item.
    /// The activity log is stored as a .jsonl file next to the work item's
    /// JSON file, with the same UUID but an "activity-" prefix.
    fn activity_path(&self, id: &WorkItemId) -> Result<PathBuf, BackendError> {
        match id {
            WorkItemId::LocalFile(path) => {
                let file_name = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
                    BackendError::Io(format!("invalid work item path: {}", path.display()))
                })?;
                Ok(self.data_dir.join(format!("activity-{file_name}.jsonl")))
            }
            other => Err(BackendError::UnsupportedId(other.clone())),
        }
    }

    /// Read and deserialize a work item record from disk.
    fn read_record(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError> {
        match id {
            WorkItemId::LocalFile(path) => {
                if !path.exists() {
                    return Err(BackendError::NotFound(id.clone()));
                }
                let contents = fs::read_to_string(path).map_err(|e| {
                    BackendError::Io(format!("failed to read {}: {e}", path.display()))
                })?;
                serde_json::from_str(&contents).map_err(|e| {
                    BackendError::Io(format!("failed to parse {}: {e}", path.display()))
                })
            }
            other => Err(BackendError::UnsupportedId(other.clone())),
        }
    }

    /// Read-modify-write helper for a work item record.
    /// Reads the record from disk, applies the mutation, serializes, and
    /// writes back atomically. Deduplicates the boilerplate shared by
    /// update_status and update_plan.
    fn modify_record(
        &self,
        id: &WorkItemId,
        f: impl FnOnce(&mut WorkItemRecord),
    ) -> Result<(), BackendError> {
        let mut record = self.read_record(id)?;
        f(&mut record);
        match id {
            WorkItemId::LocalFile(path) => {
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
    fn read(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError> {
        self.read_record(id)
    }

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
            description: request.description,
            status: request.status,
            repo_associations: request.repo_associations,
            plan: None,
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
                // Also remove the activity log file if it exists.
                if let Ok(activity_path) = self.activity_path(id) {
                    let _ = fs::remove_file(&activity_path);
                }
                Ok(())
            }
            other => Err(BackendError::UnsupportedId(other.clone())),
        }
    }

    fn update_status(&self, id: &WorkItemId, status: WorkItemStatus) -> Result<(), BackendError> {
        self.modify_record(id, |record| {
            record.status = status;
        })
    }

    fn import(&self, unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError> {
        let request = CreateWorkItem {
            title: unlinked.pr.title.clone(),
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: unlinked.repo_path.clone(),
                branch: Some(unlinked.branch.clone()),
                pr_identity: None,
            }],
        };
        self.create(request)
    }

    fn append_activity(&self, id: &WorkItemId, entry: &ActivityEntry) -> Result<(), BackendError> {
        let activity_path = self.activity_path(id)?;
        let mut line =
            serde_json::to_string(entry).map_err(|e| BackendError::Serialize(format!("{e}")))?;
        line.push('\n');

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&activity_path)
            .map_err(|e| {
                BackendError::Io(format!(
                    "failed to open activity log {}: {e}",
                    activity_path.display()
                ))
            })?;
        file.write_all(line.as_bytes()).map_err(|e| {
            BackendError::Io(format!(
                "failed to write activity log {}: {e}",
                activity_path.display()
            ))
        })?;
        Ok(())
    }

    fn read_activity(&self, id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
        let activity_path = self.activity_path(id)?;
        if !activity_path.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&activity_path).map_err(|e| {
            BackendError::Io(format!(
                "failed to read activity log {}: {e}",
                activity_path.display()
            ))
        })?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ActivityEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    // Skip corrupt lines rather than discarding the entire
                    // log. Partial writes from a crash can leave a truncated
                    // trailing line; the valid entries before it are still
                    // valuable.
                    eprintln!(
                        "workbridge: skipping corrupt activity log line in {}: {e}",
                        activity_path.display()
                    );
                }
            }
        }
        Ok(entries)
    }

    fn update_plan(&self, id: &WorkItemId, plan: &str) -> Result<(), BackendError> {
        let plan = plan.to_string();
        self.modify_record(id, |record| {
            record.plan = Some(plan);
        })
    }

    fn read_plan(&self, id: &WorkItemId) -> Result<Option<String>, BackendError> {
        Ok(self.read_record(id)?.plan)
    }

    fn activity_path_for(&self, id: &WorkItemId) -> Option<PathBuf> {
        self.activity_path(id).ok()
    }

    fn save_pr_identity(
        &self,
        id: &WorkItemId,
        repo_path: &Path,
        pr_identity: &PrIdentityRecord,
    ) -> Result<(), BackendError> {
        let pr_identity = pr_identity.clone();
        self.modify_record(id, |record| {
            for assoc in &mut record.repo_associations {
                if assoc.repo_path == repo_path {
                    assoc.pr_identity = Some(pr_identity.clone());
                }
            }
        })
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
                description: None,
                status: WorkItemStatus::Backlog,
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_validates_non_empty_repos() {
        let dir = temp_dir("validate-repos");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let result = backend.create(CreateWorkItem {
            title: "No repos".into(),
            description: None,
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
                description: None,
                status: WorkItemStatus::Implementing,
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
                description: None,
                status: WorkItemStatus::Backlog,
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
                description: None,
                status: WorkItemStatus::Backlog,
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

    #[test]
    fn serde_migration_plan_defaults_to_none() {
        // Old records without plan field should deserialize with plan: None.
        let json = r#"{"id":{"LocalFile":"/tmp/test.json"},"title":"Test","status":"Backlog","repo_associations":[]}"#;
        let record: WorkItemRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.plan, None);
    }

    #[test]
    fn activity_log_append_and_read_roundtrip() {
        let dir = temp_dir("activity-roundtrip");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let record = backend
            .create(CreateWorkItem {
                title: "Activity test".into(),
                description: None,
                status: WorkItemStatus::Implementing,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: Some("main".into()),
                    pr_identity: None,
                }],
            })
            .unwrap();

        // Append two entries.
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
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].event_type, "stage_change");
        assert_eq!(entries[0].timestamp, "1000Z");
        assert_eq!(entries[1].event_type, "note");
        assert_eq!(entries[1].timestamp, "2000Z");
        assert_eq!(entries[1].payload["message"], "started work");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn activity_log_empty_for_new_item() {
        let dir = temp_dir("activity-empty");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let record = backend
            .create(CreateWorkItem {
                title: "No activity".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: None,
                    pr_identity: None,
                }],
            })
            .unwrap();

        let entries = backend.read_activity(&record.id).unwrap();
        assert!(entries.is_empty(), "new item should have no activity");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_storage_roundtrip() {
        let dir = temp_dir("plan-roundtrip");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let record = backend
            .create(CreateWorkItem {
                title: "Plan test".into(),
                description: None,
                status: WorkItemStatus::Planning,
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn activity_path_for_returns_path() {
        let dir = temp_dir("activity-path");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let record = backend
            .create(CreateWorkItem {
                title: "Path test".into(),
                description: None,
                status: WorkItemStatus::Backlog,
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

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_pr_identity_roundtrip() {
        let dir = temp_dir("pr-identity");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let repo = PathBuf::from("/my/repo");
        let record = backend
            .create(CreateWorkItem {
                title: "PR identity test".into(),
                description: None,
                status: WorkItemStatus::Implementing,
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

        let _ = fs::remove_dir_all(&dir);
    }
}
