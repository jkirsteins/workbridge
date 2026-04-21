use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::{fmt, fs};

use serde::{Deserialize, Serialize};

use crate::work_item::{
    ReviewRequestedPr, UnlinkedPr, WorkItemId, WorkItemKind, WorkItemStatus, repo_slug_from_path,
};

/// Errors from backend operations.
///
/// A `Parse` variant for parseable-but-invalid records was removed
/// when Phase 3 of the hygiene campaign eliminated dead
/// `#[allow(dead_code)]` attributes - `LocalFileBackend` skips corrupt
/// files rather than surfacing them, and no other backend exists yet.
/// Re-add the variant (and the matching `Display` arm) in the same
/// commit as the first backend that produces it.
#[derive(Clone, Debug)]
pub enum BackendError {
    /// I/O error reading or writing backend storage.
    Io(String),
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
            Self::Io(msg) => write!(f, "backend I/O error: {msg}"),
            Self::Serialize(msg) => {
                write!(f, "backend serialization error: {msg}")
            }
            Self::NotFound(id) => {
                write!(f, "work item not found: {id:?}")
            }
            Self::Validation(msg) => {
                write!(f, "validation error: {msg}")
            }
            Self::UnsupportedId(id) => {
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
    /// Internal key. For `LocalFileBackend` this is always
    /// `LocalFile(<path of the file on disk>)`; the JSON value is never
    /// trusted because the path IS the id. Both `list()` and
    /// `read_record()` overwrite this field with the actual path
    /// immediately after deserialization, so the deserialized value -
    /// including the placeholder that `#[serde(default)]` produces for
    /// legacy records written before this field existed - is discarded
    /// on every load. Records with a *present-but-malformed* `id`
    /// value (e.g. a string instead of a tagged enum) still fail
    /// strict deserialization and surface as `CorruptRecord`.
    #[serde(default = "placeholder_work_item_id")]
    pub id: WorkItemId,
    pub title: String,
    /// Optional description providing context for planning/refinement.
    /// Defaults to None for migration compatibility with existing records.
    #[serde(default)]
    pub description: Option<String>,
    pub status: WorkItemStatus,
    /// Distinguishes the user's own work from review requests.
    /// Defaults to Own for migration compatibility with existing records.
    #[serde(default)]
    pub kind: WorkItemKind,
    /// Backend-provided, human-readable stable identifier for the work
    /// item (e.g. `"workbridge-42"`). Distinct from `id`, which is the
    /// internal key. `LocalFileBackend` generates IDs as `<repo-slug>-<N>`
    /// at create time, with N persisted in `id-counters.json` so
    /// numbers are never reused - deletion leaves permanent gaps. This
    /// is a post-v1 addition: records created before this feature
    /// landed deserialize with `display_id: None` and are not
    /// backfilled.
    #[serde(default)]
    pub display_id: Option<String>,
    pub repo_associations: Vec<RepoAssociationRecord>,
    /// Implementation plan text. None means no plan has been set yet.
    /// Defaults to None for migration compatibility with existing records.
    #[serde(default)]
    pub plan: Option<String>,
    /// Epoch seconds (UTC) when this item entered the Done state.
    /// Used by auto-archival to determine when to delete completed items.
    /// Defaults to None for migration compatibility with existing records.
    #[serde(default)]
    pub done_at: Option<u64>,
}

/// Placeholder `WorkItemId` used only by `#[serde(default)]` on
/// `WorkItemRecord::id`. Every caller that deserializes a record
/// immediately overwrites `record.id` with the real on-disk path
/// (see `LocalFileBackend::list()` and `LocalFileBackend::read_record()`),
/// so this value must never escape the backend layer. It exists solely
/// so that records written before the `id` field was added still
/// deserialize cleanly instead of surfacing as `CorruptRecord` with a
/// "missing field `id`" reason.
fn placeholder_work_item_id() -> WorkItemId {
    WorkItemId::LocalFile(PathBuf::new())
}

/// An entry in a work item's append-only activity log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// Timestamp in ISO 8601 format (or epoch seconds with Z suffix).
    pub timestamp: String,
    /// Type of event (e.g., "`stage_change`", "note", "`review_gate`").
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
/// Backends must return `BackendError::Validation` if `repo_associations`
/// is empty.
#[derive(Clone, Debug)]
pub struct CreateWorkItem {
    pub title: String,
    pub description: Option<String>,
    pub status: WorkItemStatus,
    pub kind: WorkItemKind,
    pub repo_associations: Vec<RepoAssociationRecord>,
}

/// Trait for work item storage backends. The backend is responsible for
/// persisting work item records. All derived metadata (PR status, CI
/// checks, git state) is handled by the assembly layer, not the backend.
///
/// v1 uses `LocalFileBackend`. Future backends include `GithubIssueBackend`
/// and `GithubProjectBackend`.
pub trait WorkItemBackend: Send + Sync {
    /// List all work item records from this backend.
    ///
    /// Returns a `ListResult` containing valid records and any corrupt
    /// entries that could not be parsed. Callers should surface corrupt
    /// entries to the user rather than silently ignoring them.
    fn list(&self) -> Result<ListResult, BackendError>;

    /// Read a single work item record by ID.
    fn read(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError>;

    /// Create a new work item and return the created record.
    /// Must return `BackendError::Validation` if `request.repo_associations`
    /// is empty (Invariant 1: at least one repo required).
    fn create(&self, request: CreateWorkItem) -> Result<WorkItemRecord, BackendError>;

    /// Pre-delete cleanup hook. Called before `delete()` to allow
    /// backend-specific cleanup (e.g., closing a backing GitHub issue,
    /// archiving a project item). Errors are non-fatal - callers should
    /// log warnings but continue with the delete.
    ///
    /// Default implementation is a no-op. Override in future backends
    /// (`GithubIssueBackend`, `GithubProjectBackend`) as needed.
    fn pre_delete_cleanup(&self, _id: &WorkItemId) -> Result<(), BackendError> {
        Ok(())
    }

    /// Delete a work item by id.
    fn delete(&self, id: &WorkItemId) -> Result<(), BackendError>;

    /// Update a work item's status.
    fn update_status(&self, id: &WorkItemId, status: WorkItemStatus) -> Result<(), BackendError>;

    /// Import an unlinked PR as a new work item.
    fn import(&self, unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError>;

    /// Import a review-requested PR as a new work item.
    fn import_review_request(&self, rr: &ReviewRequestedPr)
    -> Result<WorkItemRecord, BackendError>;

    /// Append an activity entry to a work item's activity log.
    fn append_activity(&self, id: &WorkItemId, entry: &ActivityEntry) -> Result<(), BackendError>;

    /// Append an activity entry **only if the active activity log
    /// already exists**. Returns `Ok(true)` if the entry was written,
    /// `Ok(false)` if the active log was missing (e.g. the work item
    /// was deleted and its log was archived while the caller was
    /// preparing the entry). The invariant the caller cares about is
    /// "do not resurrect the active log file for a deleted item" -
    /// implementations MUST NOT create the active log file if it
    /// does not already exist.
    ///
    /// This is the load-bearing primitive for background threads that
    /// write to the activity log AFTER the main thread may have
    /// already called `delete` on the same work item (today, the
    /// rebase gate: `App::spawn_rebase_gate` -> `append_activity`
    /// runs on a dedicated background thread and can race a main-
    /// thread `backend.delete` + `archive_activity_log`). If the
    /// background thread used `append_activity` instead, the
    /// `LocalFileBackend` implementation's `OpenOptions::create(true)`
    /// would silently recreate the active log file for an already-
    /// deleted item, leaving an orphan `activity-*.jsonl` in the
    /// active directory that the metrics aggregator would then count
    /// as a phantom work item.
    ///
    /// POSIX semantics make this race-free without additional
    /// locking: if the caller opens the file (existing) just before
    /// the main thread's `fs::rename(active -> archive/...)`, the
    /// open file descriptor still points at the renamed inode, so
    /// the caller's write lands in the archived file rather than an
    /// orphan. If the rename happens first, the caller's open fails
    /// with `ENOENT` and this method returns `Ok(false)`.
    ///
    /// The default implementation returns
    /// `Err(BackendError::Validation(...))` so that any future
    /// backend impl that forgets to override this method fails
    /// loudly at the first call site instead of silently
    /// delegating to `append_activity` (which is the orphan-
    /// creating call this primitive exists to replace). The
    /// reference `LocalFileBackend` overrides it with the actual
    /// `create(false)` open. Test stubs that genuinely have no
    /// create-on-append hazard (every in-memory backend in the
    /// test suite) MUST opt in explicitly by either overriding
    /// the method themselves or never being driven through a
    /// code path that calls it. See the "cancellation must
    /// precede destruction" architectural rule in
    /// `docs/harness-contract.md` C10 for the full context, and
    /// the discussion in the round 2 review log entry for the
    /// PR #104 rebase gate cleanup for why this is "default Err"
    /// rather than "default delegate".
    fn append_activity_existing_only(
        &self,
        id: &WorkItemId,
        _entry: &ActivityEntry,
    ) -> Result<bool, BackendError> {
        Err(BackendError::Validation(format!(
            "append_activity_existing_only is not implemented for this \
             backend (work item id: {id:?}); see WorkItemBackend trait \
             docs and docs/harness-contract.md C10 for the orphan-log \
             contract this primitive enforces"
        )))
    }

    /// Update the title of a work item.
    ///
    /// Default implementation returns an unsupported error. Override in
    /// backends that support title mutation (`LocalFileBackend`).
    fn update_title(&self, id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
        Err(BackendError::UnsupportedId(id.clone()))
    }

    /// Set or replace the branch name on the repo association matching
    /// `repo_path`. Used by the "Set branch" recovery dialog and any
    /// future branch-rename flow. Default impl is a no-op so in-process
    /// test mocks do not need to implement it unless they exercise the
    /// recovery path.
    fn update_branch(
        &self,
        _id: &WorkItemId,
        _repo_path: &Path,
        _branch: &str,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// Update the implementation plan for a work item.
    fn update_plan(&self, id: &WorkItemId, plan: &str) -> Result<(), BackendError>;

    /// Read the implementation plan for a work item.
    fn read_plan(&self, id: &WorkItemId) -> Result<Option<String>, BackendError>;

    /// Set or clear the `done_at` timestamp for a work item.
    /// Called when a work item enters or leaves the Done state.
    fn set_done_at(&self, id: &WorkItemId, done_at: Option<u64>) -> Result<(), BackendError>;

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
}

/// Local filesystem backend that stores each work item as a JSON file.
///
/// Files are stored in a platform-specific data directory (reached
/// through `crate::side_effects::paths::project_dirs`) under a
/// `work-items/` subdirectory. Each file is named with a UUID v4 and
/// a `.json` extension.
pub struct LocalFileBackend {
    data_dir: PathBuf,
    /// Serializes read-modify-write access to `id-counters.json` so
    /// concurrent `create()` calls (e.g. from a background thread) can
    /// never race on the counter file and hand out duplicate or
    /// out-of-order display IDs. Held only for the duration of a single
    /// load/save cycle inside `allocate_id`.
    counter_lock: Mutex<()>,
}

impl LocalFileBackend {
    /// Create a new `LocalFileBackend` using the platform-specific data directory.
    ///
    /// macOS: ~/Library/Application Support/workbridge/work-items/
    /// Linux: ~/.local/share/workbridge/work-items/
    ///
    /// Creates the directory if it does not exist.
    pub fn new() -> Result<Self, BackendError> {
        let proj = crate::side_effects::paths::project_dirs()
            .ok_or_else(|| BackendError::Io("could not determine data directory".into()))?;
        let data_dir = proj.data_dir().join("work-items");
        fs::create_dir_all(&data_dir).map_err(|e| {
            BackendError::Io(format!(
                "failed to create data dir {}: {e}",
                data_dir.display()
            ))
        })?;
        Ok(Self {
            data_dir,
            counter_lock: Mutex::new(()),
        })
    }

    /// Create a `LocalFileBackend` with a custom directory (for tests).
    #[cfg(test)]
    pub fn with_dir(dir: PathBuf) -> Result<Self, BackendError> {
        fs::create_dir_all(&dir).map_err(|e| {
            BackendError::Io(format!("failed to create dir {}: {e}", dir.display()))
        })?;
        Ok(Self {
            data_dir: dir,
            counter_lock: Mutex::new(()),
        })
    }

    /// Path to the persistent ID-counter file.
    ///
    /// The file stores a JSON object `{ "<slug>": <highest_ever_n> }`
    /// where `highest_ever_n` is the largest `N` ever assigned for the
    /// given repo slug. Storing the high-water mark (rather than "next")
    /// makes the invariant trivial to read off the file: the next ID
    /// for a slug is always `highest + 1`, and deleting items never
    /// touches the counter, so numbers are never reused even after
    /// deletion leaves gaps.
    fn counter_path(&self) -> PathBuf {
        self.data_dir.join("id-counters.json")
    }

    /// Load the persistent counter map from disk.
    ///
    /// Corruption tolerance: a missing file, an unreadable file, or
    /// garbled JSON all return an empty map and log a warning to
    /// stderr. The invariant "never reuse an ID" is best-effort against
    /// manual file tampering; under normal operation the next
    /// `save_counters` call rewrites the file from scratch and the
    /// counters resume from whatever state they were in pre-corruption.
    fn load_counters(&self) -> HashMap<String, u64> {
        let path = self.counter_path();
        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
            Err(e) => {
                eprintln!(
                    "workbridge: failed to read id-counters file {}: {e}; \
                     starting fresh counters (existing work items keep their IDs)",
                    path.display()
                );
                return HashMap::new();
            }
        };
        match serde_json::from_str::<HashMap<String, u64>>(&contents) {
            Ok(map) => map,
            Err(e) => {
                eprintln!(
                    "workbridge: id-counters file {} is corrupt ({e}); \
                     starting fresh counters (existing work items keep their IDs)",
                    path.display()
                );
                HashMap::new()
            }
        }
    }

    /// Atomically persist the counter map back to disk.
    fn save_counters(&self, counters: &HashMap<String, u64>) -> Result<(), BackendError> {
        let path = self.counter_path();
        let json = serde_json::to_string_pretty(counters)
            .map_err(|e| BackendError::Serialize(format!("{e}")))?;
        atomic_write(&path, json.as_bytes())
            .map_err(|e| BackendError::Io(format!("failed to write {}: {e}", path.display())))?;
        Ok(())
    }

    /// Allocate the next display ID for `slug`, persist the updated
    /// counter, and return the formatted `"{slug}-{N}"` string.
    ///
    /// Locking: acquires `counter_lock` for the entire load/modify/save
    /// cycle so parallel `create()` calls cannot race. The lock is
    /// dropped as soon as the counter file is saved, before the caller
    /// writes the work item JSON, so it does not serialize the rest of
    /// `create()`.
    fn allocate_id(&self, slug: &str) -> Result<String, BackendError> {
        // Recover from a poisoned lock by taking the inner guard. A
        // poisoned mutex just means a previous holder panicked; the
        // counter state is still valid because we load/save on every
        // call rather than caching across calls.
        let _guard = self
            .counter_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut counters = self.load_counters();
        let next = counters.get(slug).copied().unwrap_or(0) + 1;
        counters.insert(slug.to_string(), next);
        self.save_counters(&counters)?;
        Ok(format!("{slug}-{next}"))
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

    /// Directory under `data_dir` where activity logs are moved when their
    /// work item is deleted. Enables the metrics dashboard to read historical
    /// flow events after the owning work item is gone.
    fn archive_dir(&self) -> PathBuf {
        self.data_dir.join("archive")
    }

    /// Path where a work item's activity log lives after deletion. The file
    /// name matches the active path so the format on both sides is identical
    /// and readers can use the same deserialization.
    fn archived_activity_path(&self, id: &WorkItemId) -> Result<PathBuf, BackendError> {
        let active = self.activity_path(id)?;
        let file_name = active
            .file_name()
            .ok_or_else(|| {
                BackendError::Io(format!("invalid activity path: {}", active.display()))
            })?
            .to_owned();
        Ok(self.archive_dir().join(file_name))
    }

    /// Move a work item's activity log into the archive directory so its
    /// flow history survives `delete()`. A no-op if the log does not exist.
    fn archive_activity_log(&self, id: &WorkItemId) -> Result<(), BackendError> {
        let active_path = self.activity_path(id)?;
        if !active_path.exists() {
            return Ok(());
        }
        let archive_dir = self.archive_dir();
        fs::create_dir_all(&archive_dir).map_err(|e| {
            BackendError::Io(format!(
                "failed to create archive dir {}: {e}",
                archive_dir.display()
            ))
        })?;
        let dest = self.archived_activity_path(id)?;
        fs::rename(&active_path, &dest).map_err(|e| {
            BackendError::Io(format!(
                "failed to archive {} -> {}: {e}",
                active_path.display(),
                dest.display()
            ))
        })
    }

    /// Read all activity entries for a work item.
    ///
    /// Test-only helper. Production code does not read activity logs
    /// through the backend: the metrics aggregator reads the raw
    /// `activity-*.jsonl` files from disk (see
    /// `metrics::aggregate_from_activity_logs`) and the MCP activity
    /// query reads through the path returned by `activity_path_for`.
    /// Keeping this as an inherent `#[cfg(test)]` method lets the
    /// append/read round-trip tests exercise `append_activity` without
    /// bloating the public backend trait with a never-called method.
    #[cfg(test)]
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

    /// Read and deserialize a work item record from disk.
    ///
    /// The deserialized `record.id` is always overwritten with
    /// `WorkItemId::LocalFile(path)` so records written before the `id`
    /// field existed (which deserialize with a placeholder via
    /// `#[serde(default)]`) and records whose file was moved after
    /// write both end up with the correct on-disk path as the id.
    fn read_record(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError> {
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
                record.id = WorkItemId::LocalFile(path.clone());
                Ok(record)
            }
            other => Err(BackendError::UnsupportedId(other.clone())),
        }
    }

    /// Read-modify-write helper for a work item record.
    /// Reads the record from disk, applies the mutation, serializes, and
    /// writes back atomically. Deduplicates the boilerplate shared by
    /// `update_status` and `update_plan`.
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
            .map_or_else(|| "workitem".into(), |n| n.to_string_lossy().into_owned())
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
            // Skip sidecar files that share the data directory but do
            // not hold work item records. Adding a new sidecar? Extend
            // this list rather than moving files into subdirectories,
            // so existing deployments keep working.
            if path.file_name().and_then(|n| n.to_str()) == Some("id-counters.json") {
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
                    // Ensure the id reflects the actual file path on
                    // disk. This always runs: for modern records it
                    // corrects a stale path (e.g. if the file was
                    // moved after write); for legacy records written
                    // before the `id` field existed it replaces the
                    // `#[serde(default)]` placeholder with the real
                    // path. See the comment on `WorkItemRecord::id`.
                    record.id = WorkItemId::LocalFile(path.clone());
                    records.push(record);
                }
                Err(e) => {
                    corrupt.push(CorruptRecord {
                        path: path.clone(),
                        reason: format!("corrupt JSON: {e}"),
                    });
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

        // Derive the display-ID slug from the first repo association,
        // using the same rule as the work item list's group header so
        // `#workbridge-42` and `ACTIVE (workbridge)` can never drift.
        // Safe to index: we already returned above if the list is empty.
        let slug = repo_slug_from_path(&request.repo_associations[0].repo_path);
        let display_id = self.allocate_id(&slug)?;

        let filename = format!("{}.json", uuid::Uuid::new_v4());
        let path = self.data_dir.join(&filename);

        let record = WorkItemRecord {
            id: WorkItemId::LocalFile(path.clone()),
            title: request.title,
            description: request.description,
            status: request.status,
            kind: request.kind,
            display_id: Some(display_id),
            repo_associations: request.repo_associations,
            plan: None,
            done_at: None,
        };

        let json = serde_json::to_string_pretty(&record)
            .map_err(|e| BackendError::Serialize(format!("{e}")))?;

        atomic_write(&path, json.as_bytes())
            .map_err(|e| BackendError::Io(format!("failed to write {}: {e}", path.display())))?;

        // Seed the activity log with a `created` event so the metrics
        // aggregator can see freshly created items in `created_per_day`
        // and (when initial status is Backlog) in the current-backlog
        // trailing edge without waiting for the first stage_change.
        // Without this, an item that is created and left untouched is
        // invisible to the Dashboard until some later event appends
        // the first log line. An append failure is non-fatal: the JSON
        // record is authoritative and the item still works; only
        // historical metrics lose that one entry.
        let secs = crate::side_effects::clock::system_now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let created_entry = ActivityEntry {
            timestamp: format!("{secs}Z"),
            event_type: "created".to_string(),
            payload: serde_json::json!({ "initial_status": record.status }),
        };
        if let Err(e) = self.append_activity(&record.id, &created_entry) {
            eprintln!(
                "workbridge: failed to append initial activity entry for {}: {e}; \
                 dashboard will omit this item until another event is logged",
                path.display()
            );
        }

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
                // Move the activity log to the archive directory so the
                // metrics dashboard can read historical flow events after
                // the work item is gone. If archival fails (cross-device
                // rename, permission error, ...) we deliberately leave
                // the log in place in the active directory rather than
                // deleting it: the aggregator reads both the active dir
                // and `archive/`, so an orphan log still contributes to
                // historical metrics and nothing is silently destroyed.
                // The user may need to clean it up by hand later, but
                // history is preserved - which is the whole point of
                // archival. stderr is swallowed by the alternate screen
                // under the TUI, so this warning is best-effort for the
                // non-TUI case only.
                if let Err(e) = self.archive_activity_log(id) {
                    eprintln!(
                        "workbridge: failed to archive activity log for deleted work item: {e}; \
                         leaving log in place so history is preserved"
                    );
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

    fn update_title(&self, id: &WorkItemId, title: &str) -> Result<(), BackendError> {
        let title = title.to_string();
        self.modify_record(id, |record| {
            record.title = title;
        })
    }

    fn update_branch(
        &self,
        id: &WorkItemId,
        repo_path: &Path,
        branch: &str,
    ) -> Result<(), BackendError> {
        let branch = branch.to_string();
        let repo_path = repo_path.to_path_buf();
        self.modify_record(id, |record| {
            for assoc in &mut record.repo_associations {
                if assoc.repo_path == repo_path {
                    assoc.branch = Some(branch.clone());
                }
            }
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

    /// Override that uses `OpenOptions::create(false)` so a delete
    /// racing the append cannot recreate an orphan active log for a
    /// deleted item. `ErrorKind::NotFound` maps to `Ok(false)`; any
    /// other open error propagates as `BackendError::Io`. This is the
    /// structural fix for the orphan-active-log race in the rebase
    /// gate's background thread - see the trait-level docstring for
    /// `append_activity_existing_only` and C10 in
    /// `docs/harness-contract.md`.
    fn append_activity_existing_only(
        &self,
        id: &WorkItemId,
        entry: &ActivityEntry,
    ) -> Result<bool, BackendError> {
        let activity_path = self.activity_path(id)?;
        let mut line =
            serde_json::to_string(entry).map_err(|e| BackendError::Serialize(format!("{e}")))?;
        line.push('\n');

        use std::io::Write;
        let mut file = match std::fs::OpenOptions::new()
            .create(false)
            .append(true)
            .open(&activity_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(BackendError::Io(format!(
                    "failed to open activity log {}: {e}",
                    activity_path.display()
                )));
            }
        };
        // POSIX: a concurrent `fs::rename(active -> archive/...)` on
        // the main thread leaves this fd pointing at the same inode,
        // so the write below lands in the archived file rather than
        // an orphan active log.
        file.write_all(line.as_bytes()).map_err(|e| {
            BackendError::Io(format!(
                "failed to write activity log {}: {e}",
                activity_path.display()
            ))
        })?;
        Ok(true)
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

    fn set_done_at(&self, id: &WorkItemId, done_at: Option<u64>) -> Result<(), BackendError> {
        self.modify_record(id, |record| {
            record.done_at = done_at;
        })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};

    /// Allocate a fresh tempdir for a test. Returns both the `TempDir`
    /// guard (which removes the directory on drop) and a concrete
    /// `PathBuf` for ergonomic use. The `_name` argument is retained for
    /// call-site self-documentation (the suffix used to encode the test
    /// name into the fixed `/tmp/workbridge-test-backend-<name>` path)
    /// even though `tempfile::tempdir()` already produces a collision-
    /// free name.
    fn temp_dir(_name: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        (tmp, dir)
    }

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

        let result =
            backend.update_title(&WorkItemId::LocalFile(dir.join("nonexistent.json")), "Hi");
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
        assert_eq!(record.done_at, Some(1712345678));
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
        backend.set_done_at(&record.id, Some(1000000)).unwrap();
        let result = backend.list().unwrap();
        assert_eq!(result.records[0].done_at, Some(1000000));

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

    // -----------------------------------------------------------------
    // display_id tests
    //
    // Every work item created through LocalFileBackend gets a stable,
    // human-readable `display_id` of the form `<slug>-<N>`, where the
    // slug is the final path component of the first repo association
    // and N is a monotonic per-slug counter persisted in
    // `id-counters.json`. Numbers are never reused even after delete;
    // counters survive process restart; corrupt counter files are
    // tolerated. These tests pin those invariants.
    // -----------------------------------------------------------------

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
    fn create_assigns_display_id() {
        let (_tmp, dir) = temp_dir("display-id-first");
        let backend = LocalFileBackend::with_dir(dir).unwrap();

        let record = backend
            .create(make_request("/tmp/foo/workbridge", "first"))
            .unwrap();

        assert_eq!(
            record.display_id.as_deref(),
            Some("workbridge-1"),
            "first item in `workbridge` repo should be workbridge-1"
        );
    }

    #[test]
    fn display_id_counts_per_repo() {
        let (_tmp, dir) = temp_dir("display-id-per-repo");
        let backend = LocalFileBackend::with_dir(dir).unwrap();

        // Three items in `foo`, interleaved with two in `bar`. The
        // per-slug counter must be independent: `foo` advances 1->2->3
        // while `bar` stays at 1 until its first item is created, then
        // advances 1->2 while `foo` stays at wherever it was.
        let f1 = backend.create(make_request("/repos/foo", "f1")).unwrap();
        let b1 = backend.create(make_request("/repos/bar", "b1")).unwrap();
        let f2 = backend.create(make_request("/repos/foo", "f2")).unwrap();
        let f3 = backend.create(make_request("/repos/foo", "f3")).unwrap();
        let b2 = backend.create(make_request("/repos/bar", "b2")).unwrap();

        assert_eq!(f1.display_id.as_deref(), Some("foo-1"));
        assert_eq!(f2.display_id.as_deref(), Some("foo-2"));
        assert_eq!(f3.display_id.as_deref(), Some("foo-3"));
        assert_eq!(b1.display_id.as_deref(), Some("bar-1"));
        assert_eq!(b2.display_id.as_deref(), Some("bar-2"));
    }

    #[test]
    fn display_id_never_reuses_on_delete() {
        let (_tmp, dir) = temp_dir("display-id-no-reuse");
        let backend = LocalFileBackend::with_dir(dir).unwrap();

        let r1 = backend.create(make_request("/repos/foo", "one")).unwrap();
        let r2 = backend.create(make_request("/repos/foo", "two")).unwrap();
        let r3 = backend.create(make_request("/repos/foo", "three")).unwrap();
        assert_eq!(r1.display_id.as_deref(), Some("foo-1"));
        assert_eq!(r2.display_id.as_deref(), Some("foo-2"));
        assert_eq!(r3.display_id.as_deref(), Some("foo-3"));

        // Delete the middle item. Its number (2) must never be reused.
        backend.delete(&r2.id).unwrap();

        let r4 = backend.create(make_request("/repos/foo", "four")).unwrap();
        assert_eq!(
            r4.display_id.as_deref(),
            Some("foo-4"),
            "deleted IDs leave permanent gaps; the counter always advances"
        );
    }

    #[test]
    fn counter_persists_across_backend_instances() {
        let (_tmp, dir) = temp_dir("display-id-persist");

        // Instance 1: allocate foo-1.
        {
            let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();
            let r = backend.create(make_request("/repos/foo", "one")).unwrap();
            assert_eq!(r.display_id.as_deref(), Some("foo-1"));
        }

        // Instance 2: same dir, fresh backend. The counter file on
        // disk is the only shared state; if it is read on startup the
        // next ID must be foo-2, not foo-1.
        {
            let backend = LocalFileBackend::with_dir(dir).unwrap();
            let r = backend.create(make_request("/repos/foo", "two")).unwrap();
            assert_eq!(
                r.display_id.as_deref(),
                Some("foo-2"),
                "counter must survive backend drop/recreate via id-counters.json"
            );
        }
    }

    #[test]
    fn legacy_record_without_display_id_deserializes() {
        // Migration-compat: an on-disk JSON written before the
        // `display_id` field existed must still load cleanly with
        // `display_id: None`.
        let (_tmp, dir) = temp_dir("display-id-legacy");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let legacy_path = dir.join("legacy.json");
        let legacy_json = r#"{
            "id": {"LocalFile": "__SELF__"},
            "title": "Pre-feature item",
            "status": "Backlog",
            "kind": "Own",
            "repo_associations": [
                {"repo_path": "/repos/foo", "branch": null}
            ]
        }"#
        .replace("__SELF__", legacy_path.to_str().unwrap());
        fs::write(&legacy_path, legacy_json).unwrap();

        let result = backend.list().unwrap();
        assert!(
            result.corrupt.is_empty(),
            "legacy record must not surface as corrupt: {:?}",
            result.corrupt
        );
        assert_eq!(result.records.len(), 1);
        assert_eq!(result.records[0].display_id, None);
        assert_eq!(result.records[0].title, "Pre-feature item");
    }

    #[test]
    fn corrupt_counter_file_does_not_panic() {
        // A manually corrupted `id-counters.json` must be tolerated:
        // the backend logs a warning, starts the counter from zero,
        // and the next save rewrites a valid file from scratch.
        let (_tmp, dir) = temp_dir("display-id-corrupt-counter");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        fs::write(dir.join("id-counters.json"), "{bad json").unwrap();

        let r = backend
            .create(make_request("/repos/foo", "after corruption"))
            .unwrap();
        assert_eq!(
            r.display_id.as_deref(),
            Some("foo-1"),
            "after corruption the counter starts fresh"
        );

        // The save path must have rewritten the file as valid JSON.
        let contents = fs::read_to_string(dir.join("id-counters.json")).unwrap();
        let parsed: HashMap<String, u64> =
            serde_json::from_str(&contents).expect("counter file should be valid JSON after save");
        assert_eq!(parsed.get("foo").copied(), Some(1));
    }

    #[test]
    fn counter_file_is_not_treated_as_work_item() {
        // The counter file lives next to work item JSONs. list()
        // must skip it rather than reporting it as corrupt. Without
        // the skip, every normal startup would surface a fake
        // "corrupt JSON" entry in the UI.
        let (_tmp, dir) = temp_dir("display-id-counter-skip");
        let backend = LocalFileBackend::with_dir(dir).unwrap();

        backend.create(make_request("/repos/foo", "one")).unwrap();

        let result = backend.list().unwrap();
        assert!(
            result.corrupt.is_empty(),
            "id-counters.json should not be reported as corrupt: {:?}",
            result.corrupt
        );
        assert_eq!(result.records.len(), 1);
    }

    // -----------------------------------------------------------------
    // Missing-`id` backward-compatibility tests
    //
    // Work item files written before the `id` field was added must
    // still load cleanly. `WorkItemRecord::id` carries
    // `#[serde(default = "placeholder_work_item_id")]`, and both
    // `list()` and `read_record()` overwrite the deserialized value
    // with `LocalFile(<file path>)` immediately after parsing, so the
    // placeholder never escapes the backend layer. Records with a
    // *present-but-malformed* `id` value still fail strict
    // deserialization and surface as `CorruptRecord`, as does
    // genuinely malformed JSON.
    // -----------------------------------------------------------------

    /// Legacy v1 JSON without the `id` field.
    const LEGACY_WITHOUT_ID: &str = r#"{
        "title": "Legacy item",
        "status": "Implementing",
        "kind": "Own",
        "repo_associations": [
            {"repo_path": "/repos/foo", "branch": "feature/x"}
        ]
    }"#;

    #[test]
    fn legacy_record_missing_id_loads_cleanly_via_list() {
        let (_tmp, dir) = temp_dir("missing-id-list");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let legacy_path = dir.join("legacy-no-id.json");
        fs::write(&legacy_path, LEGACY_WITHOUT_ID).unwrap();

        // Precondition: the file really does not contain an `id` key.
        let raw_before = fs::read_to_string(&legacy_path).unwrap();
        assert!(
            !raw_before.contains("\"id\""),
            "precondition: legacy file must not contain an id field"
        );

        let result = backend.list().unwrap();
        assert!(
            result.corrupt.is_empty(),
            "legacy record without id must not surface as corrupt: {:?}",
            result.corrupt
        );
        assert_eq!(result.records.len(), 1);
        let record = &result.records[0];
        assert_eq!(record.title, "Legacy item");
        assert_eq!(record.status, WorkItemStatus::Implementing);
        // The id must be `LocalFile(<path of the file>)` - the
        // placeholder from `#[serde(default)]` has been overwritten
        // by `list()` with the real on-disk path.
        assert_eq!(record.id, WorkItemId::LocalFile(legacy_path));
    }

    #[test]
    fn legacy_record_missing_id_loads_cleanly_via_read() {
        let (_tmp, dir) = temp_dir("missing-id-read");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        let legacy_path = dir.join("legacy-read.json");
        fs::write(&legacy_path, LEGACY_WITHOUT_ID).unwrap();

        // `read()` must apply the same placeholder overwrite as
        // `list()` so callers that bypass `list()` (direct
        // `backend.read(&id)` after the id is known) also recover
        // legacy records transparently.
        let record = backend
            .read(&WorkItemId::LocalFile(legacy_path.clone()))
            .expect("legacy record without id must read cleanly");
        assert_eq!(record.title, "Legacy item");
        assert_eq!(record.status, WorkItemStatus::Implementing);
        assert_eq!(record.id, WorkItemId::LocalFile(legacy_path));
    }

    #[test]
    fn corrupt_json_still_surfaces_in_corrupt_list() {
        // Genuine corruption (malformed JSON) must NOT be swept up by
        // the missing-id serde default. It has to keep surfacing as a
        // CorruptRecord with a "corrupt JSON" reason.
        let (_tmp, dir) = temp_dir("missing-id-still-corrupt");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        fs::write(dir.join("broken.json"), "{ this is not json").unwrap();

        let result = backend.list().unwrap();
        assert_eq!(result.records.len(), 0);
        assert_eq!(result.corrupt.len(), 1);
        assert!(
            result.corrupt[0].reason.contains("corrupt JSON"),
            "reason should still mention corrupt JSON, got: {}",
            result.corrupt[0].reason
        );
    }

    #[test]
    fn malformed_id_value_still_surfaces_as_corrupt() {
        // Boundary case: the `id` key is present but its value is not
        // a valid `WorkItemId` (here, a bare string instead of a
        // tagged enum). Strict deserialization must fail - the
        // `#[serde(default)]` fallback only kicks in when the field
        // is absent, not when it's present-but-wrong - and the record
        // must surface as `CorruptRecord`. Guards against a future
        // refactor that "helps" by synthesizing a placeholder for any
        // parse error on the id field.
        let (_tmp, dir) = temp_dir("malformed-id-still-corrupt");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        fs::write(
            dir.join("bad-id.json"),
            r#"{
                "id": "not a valid WorkItemId",
                "title": "Broken item",
                "status": "Implementing",
                "kind": "Own",
                "repo_associations": []
            }"#,
        )
        .unwrap();

        let result = backend.list().unwrap();
        assert_eq!(result.records.len(), 0);
        assert_eq!(result.corrupt.len(), 1);
        assert!(
            result.corrupt[0].reason.contains("corrupt JSON"),
            "reason should mention corrupt JSON, got: {}",
            result.corrupt[0].reason
        );
    }
}
