//! On-disk JSON backend for work items.
//!
//! Each work item is stored as a standalone JSON file in a platform-
//! specific data directory. See [`LocalFileBackend`] for the public
//! surface.
//!
//! Tests for this module are split across several files under
//! `src/work_item_backend/local_file/` by logical area:
//!
//! - `crud_tests` - create / list / delete / update / import
//! - `activity_tests` - activity-log append, plan / `done_at`, `pr_identity`
//! - `id_tests` - `display_id` allocation and legacy-record migration

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::work_item::{
    ReviewRequestedPr, UnlinkedPr, WorkItemId, WorkItemKind, WorkItemStatus, repo_slug_from_path,
};
use crate::work_item_backend::{
    ActivityEntry, BackendError, CorruptRecord, CreateWorkItem, ListResult, PrIdentityRecord,
    RepoAssociationRecord, WorkItemBackend, WorkItemRecord,
};

#[cfg(test)]
mod activity_tests;
#[cfg(test)]
mod crud_tests;
#[cfg(test)]
mod id_tests;
#[cfg(test)]
mod test_helpers;

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
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
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
        use std::io::Write;

        let activity_path = self.activity_path(id)?;
        let mut line =
            serde_json::to_string(entry).map_err(|e| BackendError::Serialize(format!("{e}")))?;
        line.push('\n');

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
        use std::io::Write;

        let activity_path = self.activity_path(id)?;
        let mut line =
            serde_json::to_string(entry).map_err(|e| BackendError::Serialize(format!("{e}")))?;
        line.push('\n');

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
