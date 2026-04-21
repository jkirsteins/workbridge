//! Synthetic data generator for the metrics Dashboard.
//!
//! Invoked via the `workbridge seed-dashboard <target-dir>` CLI subcommand.
//! Writes ~40 synthetic work items spanning ~400 days into the target
//! directory, using the real serde types so the format on disk is
//! guaranteed to match the production write path.
//!
//! All timestamps are computed relative to "now" so the seed remains valid
//! regardless of wall-clock date. The generated mix exercises every code
//! path the Dashboard renders:
//!
//! - Normal-flow items (Backlog -> Implementing -> Done) inside each window.
//! - Long-cycle items (Backlog -> Done over 20+ days) for the p90 KPI.
//! - Items currently in Review > 3 days (stuck-items list).
//! - Items currently in Blocked > 1 day (stuck-items list).
//! - PR-merged-without-Done drift cases.
//! - Done-without-PR-merged drift cases.
//! - Items still in Backlog (non-zero current backlog size).
//! - **Archived items** (activity log in `archive/` subdirectory, no JSON
//!   record): proves the retention fix - history survives deletion.
//!
//! The seeder is intentionally placed in the main binary rather than a
//! separate `src/bin/` target so it can reuse the project's existing
//! serde types without converting the crate to a hybrid bin+lib.

use std::error::Error;
use std::fs;
use std::path::Path;

use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::{ActivityEntry, RepoAssociationRecord, WorkItemRecord};

/// Entry point invoked by `handle_cli` for the `seed-dashboard` subcommand.
/// Writes a fresh synthetic dataset into `target_dir`.
///
/// **Safety guard**: refuses to run against a directory that already
/// contains any `*.json` work item records or `activity-*.jsonl` logs
/// (either at the top level or inside an existing `archive/`
/// subdirectory). A single typo like
/// `workbridge seed-dashboard ~/.local/share/workbridge/work-items`
/// would otherwise silently pollute the real backend with ~37 fake
/// items pointing at a non-existent repo. Callers must pass a fresh
/// empty directory - e.g. `mktemp -d` in the manual verification
/// flow from `docs/metrics.md`.
pub fn seed_dashboard(target_dir: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(target_dir)?;
    refuse_if_populated(target_dir)?;
    let archive_dir = target_dir.join("archive");
    fs::create_dir_all(&archive_dir)?;
    refuse_if_populated(&archive_dir)?;

    let now = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let day = 86_400_i64;

    let mut written_items = 0usize;
    let mut written_archive = 0usize;

    // ---- Active items: normal flow, Done in 1-5 days, in last 7 days ----
    for i in 0..6 {
        let started = now - (i + 1) * day;
        let done = started + (1 + i % 4) * day;
        write_item(
            target_dir,
            &format!("active-normal-{i:02}"),
            WorkItemStatus::Done,
            &[
                stage_event(
                    started,
                    WorkItemStatus::Backlog,
                    WorkItemStatus::Implementing,
                ),
                stage_event(done, WorkItemStatus::Implementing, WorkItemStatus::Done),
            ],
            true,
        )?;
        written_items += 1;
    }

    // ---- Active items: long cycle (20-30 days) for p90 KPI ----
    for i in 0..3 {
        let started = now - (35 + i * 5) * day;
        let done = started + (20 + i * 5) * day;
        write_item(
            target_dir,
            &format!("active-long-{i:02}"),
            WorkItemStatus::Done,
            &[
                stage_event(
                    started,
                    WorkItemStatus::Backlog,
                    WorkItemStatus::Implementing,
                ),
                stage_event(done, WorkItemStatus::Implementing, WorkItemStatus::Done),
            ],
            true,
        )?;
        written_items += 1;
    }

    // ---- Stuck Review items (>3 days in Review) ----
    for i in 0..2 {
        let started = now - (10 + i) * day;
        let entered_review = now - (5 + i) * day;
        write_item(
            target_dir,
            &format!("stuck-review-{i:02}"),
            WorkItemStatus::Review,
            &[
                stage_event(
                    started,
                    WorkItemStatus::Backlog,
                    WorkItemStatus::Implementing,
                ),
                stage_event(
                    entered_review,
                    WorkItemStatus::Implementing,
                    WorkItemStatus::Review,
                ),
            ],
            true,
        )?;
        written_items += 1;
    }

    // ---- Stuck Blocked items (>1 day in Blocked) ----
    for i in 0..2 {
        let started = now - (8 + i) * day;
        let entered_blocked = now - (2 + i) * day;
        write_item(
            target_dir,
            &format!("stuck-blocked-{i:02}"),
            WorkItemStatus::Blocked,
            &[
                stage_event(
                    started,
                    WorkItemStatus::Backlog,
                    WorkItemStatus::Implementing,
                ),
                stage_event(
                    entered_blocked,
                    WorkItemStatus::Implementing,
                    WorkItemStatus::Blocked,
                ),
            ],
            true,
        )?;
        written_items += 1;
    }

    // ---- Drift: pr_merged event present, but item never reached Done ----
    for i in 0..2 {
        let started = now - (12 + i * 3) * day;
        let merged = started + (3 + i) * day;
        write_item(
            target_dir,
            &format!("drift-merged-not-done-{i:02}"),
            WorkItemStatus::Implementing,
            &[
                stage_event(
                    started,
                    WorkItemStatus::Backlog,
                    WorkItemStatus::Implementing,
                ),
                pr_merged_event(merged),
            ],
            true,
        )?;
        written_items += 1;
    }

    // ---- Drift: Done state reached, but no pr_merged event ----
    for i in 0..2 {
        let started = now - (15 + i * 2) * day;
        let done = started + (4 + i) * day;
        write_item(
            target_dir,
            &format!("drift-done-not-merged-{i:02}"),
            WorkItemStatus::Done,
            &[
                stage_event(
                    started,
                    WorkItemStatus::Backlog,
                    WorkItemStatus::Implementing,
                ),
                stage_event(done, WorkItemStatus::Implementing, WorkItemStatus::Done),
            ],
            true,
        )?;
        written_items += 1;
    }

    // ---- Items still in Backlog (current backlog size > 0) ----
    for i in 0..5 {
        let started = now - (3 + i) * day;
        write_item(
            target_dir,
            &format!("active-backlog-{i:02}"),
            WorkItemStatus::Backlog,
            &[stage_event(
                started,
                WorkItemStatus::Planning,
                WorkItemStatus::Backlog,
            )],
            true,
        )?;
        written_items += 1;
    }

    // ---- Archived items: full lifecycle preserved in archive/ ----
    // These prove the retention fix: their JSON files are gone but the
    // activity logs survive in `archive/`, so the dashboard can still
    // count them in historical windows.
    for i in 0..15 {
        let started = now - (40 + i * 20) * day;
        let done = started + (3 + i % 7) * day;
        let id = format!("archived-{i:02}");
        write_archive_only(
            &archive_dir,
            &id,
            &[
                stage_event(
                    started,
                    WorkItemStatus::Backlog,
                    WorkItemStatus::Implementing,
                ),
                pr_merged_event(done - 60),
                stage_event(done, WorkItemStatus::Implementing, WorkItemStatus::Done),
            ],
        )?;
        written_archive += 1;
    }

    eprintln!(
        "workbridge: seeded {written_items} active items and {written_archive} archived activity logs into {}",
        target_dir.display()
    );
    Ok(())
}

/// Refuse to run the seeder against a directory that already contains
/// any workbridge-shaped files. We check for `*.json` (backend records)
/// and `activity-*.jsonl` (activity logs, active or archived). This is
/// the whole safety guard: it is cheap, syscall-free beyond a single
/// `read_dir`, and catches the one failure mode that matters
/// (pointing the seeder at the real `work-items/` directory by accident).
fn refuse_if_populated(dir: &Path) -> Result<(), Box<dyn Error>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let ext = std::path::Path::new(name_str)
            .extension()
            .and_then(|e| e.to_str());
        let looks_like_record = ext.is_some_and(|e| e.eq_ignore_ascii_case("json"));
        let looks_like_activity = name_str.starts_with("activity-")
            && ext.is_some_and(|e| e.eq_ignore_ascii_case("jsonl"));
        if looks_like_record || looks_like_activity {
            return Err(format!(
                "target directory {} already contains workbridge work-item files \
                 ({name_str:?}); refusing to overwrite. Run `workbridge seed-dashboard` \
                 against a fresh empty directory, e.g. `mktemp -d`.",
                dir.display()
            )
            .into());
        }
    }
    Ok(())
}

/// Compose a `stage_change` `ActivityEntry` with the project's canonical
/// timestamp format (`{secs}Z`) and payload shape (`{from, to}`).
fn stage_event(secs: i64, from: WorkItemStatus, to: WorkItemStatus) -> ActivityEntry {
    ActivityEntry {
        timestamp: format!("{secs}Z"),
        event_type: "stage_change".to_string(),
        payload: serde_json::json!({
            "from": status_label(from),
            "to": status_label(to),
        }),
    }
}

fn pr_merged_event(secs: i64) -> ActivityEntry {
    ActivityEntry {
        timestamp: format!("{secs}Z"),
        event_type: "pr_merged".to_string(),
        payload: serde_json::json!({}),
    }
}

const fn status_label(status: WorkItemStatus) -> &'static str {
    match status {
        WorkItemStatus::Backlog => "Backlog",
        WorkItemStatus::Planning => "Planning",
        WorkItemStatus::Implementing => "Implementing",
        WorkItemStatus::Blocked => "Blocked",
        WorkItemStatus::Review => "Review",
        WorkItemStatus::Mergequeue => "Mergequeue",
        WorkItemStatus::Done => "Done",
    }
}

/// Write an active work item: a `{id}.json` record plus an
/// `activity-{id}.jsonl` log. Used for items the dashboard sees as
/// currently-existing in workbridge.
fn write_item(
    target_dir: &Path,
    id: &str,
    status: WorkItemStatus,
    entries: &[ActivityEntry],
    include_record: bool,
) -> Result<(), Box<dyn Error>> {
    let json_path = target_dir.join(format!("{id}.json"));
    let activity_path = target_dir.join(format!("activity-{id}.jsonl"));

    if include_record {
        let record = WorkItemRecord {
            id: WorkItemId::LocalFile(json_path.clone()),
            title: format!("Seed: {id}"),
            description: None,
            status,
            kind: WorkItemKind::Own,
            display_id: None,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: std::path::PathBuf::from("/seed/repo"),
                branch: Some(format!("seed/{id}")),
                pr_identity: None,
            }],
            plan: None,
            done_at: if status == WorkItemStatus::Done {
                Some(0)
            } else {
                None
            },
        };
        let json = serde_json::to_string_pretty(&record)?;
        fs::write(&json_path, json)?;
    }

    write_activity_log(&activity_path, entries)?;
    Ok(())
}

/// Write only the activity log for an archived item (no JSON record).
/// Mirrors the on-disk shape after `LocalFileBackend::delete()` archives
/// the log: a file in `archive/` with no sibling JSON.
fn write_archive_only(
    archive_dir: &Path,
    id: &str,
    entries: &[ActivityEntry],
) -> Result<(), Box<dyn Error>> {
    let activity_path = archive_dir.join(format!("activity-{id}.jsonl"));
    write_activity_log(&activity_path, entries)
}

fn write_activity_log(path: &Path, entries: &[ActivityEntry]) -> Result<(), Box<dyn Error>> {
    let mut buf = String::new();
    for entry in entries {
        buf.push_str(&serde_json::to_string(entry)?);
        buf.push('\n');
    }
    fs::write(path, buf)?;
    Ok(())
}
