//! Work item storage backend abstraction.
//!
//! This module defines the [`WorkItemBackend`] trait plus the record
//! types that flow across it. Concrete implementations live in
//! submodules:
//!
//! - [`local_file`] - `LocalFileBackend`, the on-disk JSON backend used
//!   in v1.
//! - [`mock`] (cfg(test)) - `MockBackend`, a shared in-memory stub for
//!   tests across the crate.
//!
//! The public API is re-exported at `crate::work_item_backend::<name>`
//! for every type that was previously defined in the monolithic
//! `src/work_item_backend.rs`, so existing call sites keep working
//! after the decomposition.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::work_item::{ReviewRequestedPr, UnlinkedPr, WorkItemId, WorkItemKind, WorkItemStatus};

pub mod local_file;
#[cfg(test)]
pub mod mock;

pub use local_file::LocalFileBackend;

/// Errors from backend operations.
///
/// A `Parse` variant for parseable-but-invalid records was removed
/// when the hygiene cleanup eliminated dead `#[allow(dead_code)]`
/// attributes - `LocalFileBackend` skips corrupt files rather than
/// surfacing them, and no other backend exists yet. Re-add the
/// variant (and the matching `Display` arm) in the same commit as
/// the first backend that produces it.
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
