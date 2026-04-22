//! Free helper functions extracted from `src/app/mod.rs`.

use std::path::PathBuf;

use super::{DisplayEntry, PrMergePollResult};
use crate::config::RepoEntry;
use crate::work_item::{UnlinkedPr, WorkItemId, WorkItemStatus};
use crate::work_item_backend::{
    ActivityEntry, BackendError, CreateWorkItem, PrIdentityRecord, WorkItemBackend,
};

/// Spawn a background thread that runs
/// `gh pr view <target> --repo <owner/repo> --json state,number,title,url`
/// and sends exactly one `PrMergePollResult` through the returned
/// receiver. Shared by `poll_mergequeue` and `poll_review_request_merges`
/// so the `gh` invocation and JSON parsing live in a single place.
///
/// `target` is the pinned PR number when known (unambiguous), otherwise
/// the branch name. The branch fallback is used on watches reconstructed
/// from a backend record after an app restart, and on all `ReviewRequest`
/// watches where the `--author @me` fetch never populated `assoc.pr`.
/// The poll's caller backfills the resolved number into the watch on
/// the first successful result so subsequent polls target the exact PR.
///
/// Every outcome (success, non-zero exit, spawn failure, JSON parse
/// failure) is delivered as a single send on `tx`. Errors are encoded
/// as `pr_state: "ERROR: ..."` so the caller can handle them uniformly.
pub fn spawn_gh_pr_view_poll(
    wi_id: WorkItemId,
    pr_number: Option<u64>,
    owner_repo: String,
    branch: String,
    repo_path: PathBuf,
) -> crossbeam_channel::Receiver<PrMergePollResult> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let target = pr_number.map_or_else(|| branch.clone(), |n| n.to_string());
        let outcome = match std::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &target,
                "--repo",
                &owner_repo,
                "--json",
                "state,number,title,url",
            ])
            .output()
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let parsed: serde_json::Value = match serde_json::from_str(stdout.trim()) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx.send(PrMergePollResult {
                            wi_id,
                            pr_state: format!("ERROR: JSON parse failed: {e}"),
                            branch,
                            repo_path,
                            pr_identity: None,
                        });
                        return;
                    }
                };
                let state = parsed
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("UNKNOWN")
                    .to_string();
                let pr_identity = parsed
                    .get("number")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|number| {
                        let title = parsed.get("title")?.as_str()?.to_string();
                        let url = parsed.get("url")?.as_str()?.to_string();
                        Some(PrIdentityRecord { number, title, url })
                    });
                PrMergePollResult {
                    wi_id,
                    pr_state: state,
                    branch,
                    repo_path,
                    pr_identity,
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                PrMergePollResult {
                    wi_id,
                    pr_state: format!("ERROR: {}", stderr.trim()),
                    branch,
                    repo_path,
                    pr_identity: None,
                }
            }
            Err(e) => PrMergePollResult {
                wi_id,
                pr_state: format!("ERROR: {e}"),
                branch,
                repo_path,
                pr_identity: None,
            },
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// Canonicalize repo entry paths so that symlinked or non-canonical config
/// paths resolve to the same real filesystem path. This ensures fetcher
/// cache keys (keyed by `repo_path`) match assembly lookups. If canonicalization
/// fails (e.g. path does not exist), the original path is kept.
pub fn canonicalize_repo_entries(entries: Vec<RepoEntry>) -> Vec<RepoEntry> {
    entries
        .into_iter()
        .map(|mut entry| {
            if let Ok(canonical) = crate::config::canonicalize_path(&entry.path) {
                entry.path = canonical;
            }
            entry
        })
        .collect()
}

/// Public crate-level accessor for `now_iso8601`, used by the event module.
pub fn now_iso8601_pub() -> String {
    now_iso8601()
}

/// Return the current time as an ISO 8601 string (UTC).
pub fn now_iso8601() -> String {
    let dur = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Simple UTC timestamp without pulling in chrono.
    // Format: seconds since epoch as a decimal string with "Z" suffix.
    // This is monotonic and machine-parseable.
    format!("{secs}Z")
}

/// Returns true if a display entry can receive selection (is an item, not
/// a header or empty state).
pub const fn is_selectable(entry: &DisplayEntry) -> bool {
    matches!(
        entry,
        DisplayEntry::ReviewRequestItem(_)
            | DisplayEntry::UnlinkedItem(_)
            | DisplayEntry::WorkItemEntry(_)
    )
}

/// A stub backend that stores nothing. Used in tests when no backend
/// persistence is needed. All operations return empty/success.
pub struct StubBackend;

impl WorkItemBackend for StubBackend {
    fn read(
        &self,
        id: &WorkItemId,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::NotFound(id.clone()))
    }

    fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
        Ok(crate::work_item_backend::ListResult {
            records: Vec::new(),
            corrupt: Vec::new(),
        })
    }

    fn create(
        &self,
        _request: CreateWorkItem,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "stub backend does not support create".into(),
        ))
    }

    fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
        Ok(())
    }

    fn update_status(&self, _id: &WorkItemId, _status: WorkItemStatus) -> Result<(), BackendError> {
        Ok(())
    }

    fn import(
        &self,
        _unlinked: &UnlinkedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "stub backend does not support import".into(),
        ))
    }

    fn import_review_request(
        &self,
        _rr: &crate::work_item::ReviewRequestedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "stub backend does not support import_review_request".into(),
        ))
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

    fn update_title(&self, _id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
        Ok(())
    }

    fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
        Ok(None)
    }
    fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
        Ok(())
    }
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
}
