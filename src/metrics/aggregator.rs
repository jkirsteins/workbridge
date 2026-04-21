//! Internal parsers, per-item loader, and backlog-interval reconstruction
//! logic for the metrics module. All items are `pub(super)` (so mod.rs
//! can re-export `aggregate_from_activity_logs`) or purely private.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use super::{
    DayNumber, MetricsSnapshot, SECS_PER_DAY, STUCK_BLOCKED_SECS, STUCK_REVIEW_SECS, StuckItem,
    secs_to_day,
};
use crate::work_item::WorkItemStatus;

#[derive(Debug, Clone)]
pub(super) struct ParsedEntry {
    pub(super) secs: i64,
    pub(super) kind: ParsedKind,
}

/// Where an item's activity log was loaded from. Historical (flow)
/// metrics include both variants, but point-in-time metrics like
/// `stuck_items` and the trailing edge of `backlog_size_per_day` only
/// consider `Active` items - anything in `archive/` has already been
/// deleted and must not contribute to "currently stuck" or "currently
/// in backlog" counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Provenance {
    Active,
    Archived,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ParsedKind {
    StageChange {
        from: WorkItemStatus,
        to: WorkItemStatus,
    },
    PrMerged,
    Other,
}

/// Parse a timestamp string emitted by `now_iso8601()` (format: `"{secs}Z"`).
/// Returns None for any other format, which causes the line to be skipped.
fn parse_ts(s: &str) -> Option<i64> {
    let trimmed = s.strip_suffix('Z').unwrap_or(s);
    trimmed.parse::<i64>().ok()
}

/// Parse a `WorkItemStatus` name into the enum, honoring the serde aliases
/// so old records using "Todo" / "`InProgress`" still load.
fn parse_status(s: &str) -> Option<WorkItemStatus> {
    match s {
        "Backlog" | "Todo" => Some(WorkItemStatus::Backlog),
        "Planning" => Some(WorkItemStatus::Planning),
        "Implementing" | "InProgress" => Some(WorkItemStatus::Implementing),
        "Blocked" => Some(WorkItemStatus::Blocked),
        "Review" => Some(WorkItemStatus::Review),
        "Mergequeue" => Some(WorkItemStatus::Mergequeue),
        "Done" => Some(WorkItemStatus::Done),
        _ => None,
    }
}

fn parse_line(line: &str) -> Option<ParsedEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let ts_str = value.get("timestamp")?.as_str()?;
    let secs = parse_ts(ts_str)?;
    let event_type = value.get("event_type")?.as_str()?;
    let kind = match event_type {
        "stage_change" => {
            let payload = value.get("payload")?;
            let from = parse_status(payload.get("from")?.as_str()?)?;
            let to = parse_status(payload.get("to")?.as_str()?)?;
            ParsedKind::StageChange { from, to }
        }
        // A `created` event is the first line `LocalFileBackend::create()`
        // appends to a fresh activity log. It seeds the timeline with the
        // item's initial status so the aggregator's stage_change-driven
        // bootstrap (see `backlog_intervals` and the main loop's cycle-
        // time / stuck-item tracking) treats creation as a self-
        // transition `from == to == initial_status`. That encoding
        // opens a Backlog interval for fresh Backlog items, seeds
        // `current_status` / `current_since` for stuck detection, and
        // contributes to `created_per_day` via `entries[0].secs` - all
        // without a dedicated Created variant in `ParsedKind`.
        "created" => {
            let payload = value.get("payload")?;
            let status = parse_status(payload.get("initial_status")?.as_str()?)?;
            ParsedKind::StageChange {
                from: status,
                to: status,
            }
        }
        "pr_merged" => ParsedKind::PrMerged,
        _ => ParsedKind::Other,
    };
    Some(ParsedEntry { secs, kind })
}

/// Extract the work item id from an activity log file name. Matches the
/// pattern `activity-{id}.jsonl`. Returns None for non-matching names so
/// unrelated files in the data dir (work item JSONs, etc.) are skipped.
fn id_from_filename(name: &std::ffi::OsStr) -> Option<String> {
    let s = name.to_str()?;
    let stem = s.strip_suffix(".jsonl")?;
    let id = stem.strip_prefix("activity-")?;
    Some(id.to_string())
}

fn collect_logs_in(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().and_then(id_from_filename).is_some() {
            out.push(path);
        }
    }
    out
}

/// Read every activity log under `data_dir` (active) and `data_dir/archive/`
/// (historical, for deleted work items), parse them, and group by work item
/// id. Each per-item vec is sorted ascending by timestamp. Each entry also
/// carries a `Provenance` flag so downstream metrics can distinguish live
/// from archived items. If the same id exists in both locations (e.g. the
/// user recreated an item with the same id after deletion), the active
/// copy wins and the archived log is ignored for that id.
///
/// A top-level activity log is only classified as `Provenance::Active`
/// when its sibling `{id}.json` work-item record still exists. The JSON
/// is the source of truth for liveness; the log's location is just a
/// retention hint. Without this check, an archival failure in
/// `LocalFileBackend::delete()` (which unlinks the JSON before moving
/// the log) would leave an orphan log at the top level and incorrectly
/// count the deleted item as a currently-stuck or currently-backlogged
/// item in the Dashboard.
fn load_per_item(data_dir: &Path) -> HashMap<String, (Provenance, Vec<ParsedEntry>)> {
    let active_files = collect_logs_in(data_dir);
    let archived_files = collect_logs_in(&data_dir.join("archive"));
    let mut per_item: HashMap<String, (Provenance, Vec<ParsedEntry>)> = HashMap::new();

    // Load top-level logs first so archived entries for the same id get
    // dropped. Classify each by whether the sibling work-item JSON still
    // exists: present = live item, absent = orphan from a failed
    // archival and therefore historical, not currently-stuck.
    for path in active_files {
        let Some(id) = path.file_name().and_then(id_from_filename) else {
            continue;
        };
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let parsed = contents.lines().filter_map(parse_line).collect::<Vec<_>>();
        let provenance = if data_dir.join(format!("{id}.json")).exists() {
            Provenance::Active
        } else {
            Provenance::Archived
        };
        per_item
            .entry(id)
            .or_insert_with(|| (provenance, Vec::new()))
            .1
            .extend(parsed);
    }
    for path in archived_files {
        let Some(id) = path.file_name().and_then(id_from_filename) else {
            continue;
        };
        if per_item.contains_key(&id) {
            // An active log with the same id already won; discard the
            // archived copy to avoid mixing resurrected + deleted history.
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let parsed = contents.lines().filter_map(parse_line).collect::<Vec<_>>();
        per_item
            .entry(id)
            .or_insert_with(|| (Provenance::Archived, Vec::new()))
            .1
            .extend(parsed);
    }
    for (_, entries) in per_item.values_mut() {
        entries.sort_by_key(|e| e.secs);
    }
    per_item
}

/// Half-open Backlog-state interval `[start, end)`. `end = None` means
/// the item is still in Backlog at snapshot time.
#[derive(Debug, Clone, Copy)]
struct BacklogInterval {
    start: i64,
    end: Option<i64>,
}

/// Reconstruct Backlog-state intervals from a sorted event list. The
/// first `stage_change`'s `from` field tells us the initial status, which
/// is assumed to have been held from the item's first recorded event
/// until the first transition. Items with zero `stage_change` events
/// contribute no intervals (their initial state is unknown).
fn backlog_intervals(entries: &[ParsedEntry]) -> Vec<BacklogInterval> {
    if entries.is_empty() {
        return Vec::new();
    }
    let first_ts = entries[0].secs;

    let mut timeline: Vec<(i64, WorkItemStatus)> = Vec::new();
    let mut bootstrapped = false;
    for entry in entries {
        if let ParsedKind::StageChange { from, to } = entry.kind {
            if !bootstrapped {
                timeline.push((first_ts, from));
                bootstrapped = true;
            }
            timeline.push((entry.secs, to));
        }
    }
    if !bootstrapped {
        return Vec::new();
    }

    let mut intervals = Vec::new();
    let mut open: Option<i64> = None;
    for (ts, status) in timeline {
        if status == WorkItemStatus::Backlog {
            if open.is_none() {
                open = Some(ts);
            }
        } else if let Some(start) = open.take() {
            intervals.push(BacklogInterval {
                start,
                end: Some(ts),
            });
        }
    }
    if let Some(start) = open {
        intervals.push(BacklogInterval { start, end: None });
    }
    intervals
}

/// Compute per-day Backlog size for `from_day..=to_day`. Counted at the
/// end of each day UTC: `eod` is the literal last second of `day`
/// (`(day + 1) * SECS_PER_DAY - 1`), not the first second of the
/// following day. Using the last-second-of-day reference keeps the
/// half-open `[start, end)` interval membership test symmetric at
/// boundary timestamps: an item with `iv.start` at midnight of `day + 1`
/// is counted on `day + 1`, not on `day`; an item with `iv.end` at
/// midnight of `day + 1` is counted on `day` (in backlog for all of
/// day `day`). Without the `- 1`, both tests leak by one day for
/// events that fall on a whole-day boundary.
fn reconstruct_backlog_per_day(
    all_intervals: &[Vec<BacklogInterval>],
    from_day: DayNumber,
    to_day: DayNumber,
) -> BTreeMap<DayNumber, u32> {
    let mut result = BTreeMap::new();
    for day in from_day..=to_day {
        let eod = (day + 1) * SECS_PER_DAY - 1;
        let count = all_intervals
            .iter()
            .filter(|intervals| {
                intervals
                    .iter()
                    .any(|iv| iv.start <= eod && iv.end.is_none_or(|e| e > eod))
            })
            .count() as u32;
        result.insert(day, count);
    }
    result
}

/// Walk every activity log under `data_dir` (including `archive/`) and
/// return a fresh `MetricsSnapshot`. Pure, synchronous, safe to call
/// repeatedly; intended to run on a background thread.
pub fn aggregate_from_activity_logs(data_dir: &Path) -> MetricsSnapshot {
    let now_secs = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);

    let per_item = load_per_item(data_dir);

    let mut snapshot = MetricsSnapshot {
        computed_at_secs: now_secs,
        ..MetricsSnapshot::default()
    };

    let mut all_intervals: Vec<Vec<BacklogInterval>> = Vec::with_capacity(per_item.len());

    for (wi_id, (provenance, entries)) in &per_item {
        if entries.is_empty() {
            all_intervals.push(Vec::new());
            continue;
        }

        let created_day = secs_to_day(entries[0].secs);
        *snapshot.created_per_day.entry(created_day).or_insert(0) += 1;

        let mut first_backlog_entry: Option<i64> = None;
        let mut first_done: Option<i64> = None;
        let mut current_status: Option<WorkItemStatus> = None;
        let mut current_since: Option<i64> = None;
        let mut bootstrapped = false;

        for entry in entries {
            match entry.kind {
                ParsedKind::StageChange { from, to } => {
                    // The `from` field of the first stage_change tells us
                    // the item's status before any recorded transition.
                    // We use this only to seed `first_backlog_entry` for
                    // cycle-time calculations; `current_status` is always
                    // overwritten by the `to` side below.
                    if !bootstrapped {
                        bootstrapped = true;
                        if from == WorkItemStatus::Backlog {
                            first_backlog_entry = Some(entries[0].secs);
                        }
                    }
                    if to == WorkItemStatus::Done && first_done.is_none() {
                        first_done = Some(entry.secs);
                        let day = secs_to_day(entry.secs);
                        *snapshot.done_per_day.entry(day).or_insert(0) += 1;
                    }
                    if to == WorkItemStatus::Backlog && first_backlog_entry.is_none() {
                        first_backlog_entry = Some(entry.secs);
                    }
                    current_status = Some(to);
                    current_since = Some(entry.secs);
                }
                ParsedKind::PrMerged => {
                    let day = secs_to_day(entry.secs);
                    *snapshot.prs_merged_per_day.entry(day).or_insert(0) += 1;
                }
                ParsedKind::Other => {}
            }
        }

        if let Some(done) = first_done {
            let start = first_backlog_entry.unwrap_or(entries[0].secs);
            snapshot.cycle_times_secs.push(done - start);
        }

        // Stuck-item detection is a point-in-time "now" metric: only
        // live items can be currently stuck. Archived items were deleted,
        // so their last-known status must not count toward stuck_items.
        if *provenance == Provenance::Active
            && let (Some(status), Some(since)) = (current_status, current_since)
        {
            let age = now_secs - since;
            let threshold = match status {
                WorkItemStatus::Review => Some(STUCK_REVIEW_SECS),
                WorkItemStatus::Blocked => Some(STUCK_BLOCKED_SECS),
                _ => None,
            };
            if let Some(t) = threshold
                && age > t
            {
                snapshot.stuck_items.push(StuckItem {
                    wi_id: wi_id.clone(),
                    status,
                    stuck_for_secs: age,
                });
            }
        }

        // For archived items, close any still-open Backlog interval at
        // the last observed event's timestamp. After that point the item
        // no longer exists on disk and must not inflate the trailing
        // edge of `backlog_size_per_day` forever.
        let mut intervals = backlog_intervals(entries);
        if *provenance == Provenance::Archived
            && let Some(last) = intervals.last_mut()
            && last.end.is_none()
        {
            let last_event_ts = entries.last().map_or(last.start, |e| e.secs);
            last.end = Some(last_event_ts);
        }
        all_intervals.push(intervals);
    }

    let to_day = secs_to_day(now_secs);
    let from_day = to_day - 365;
    snapshot.backlog_size_per_day = reconstruct_backlog_per_day(&all_intervals, from_day, to_day);

    snapshot
        .stuck_items
        .sort_by_key(|item| std::cmp::Reverse(item.stuck_for_secs));

    snapshot
}
