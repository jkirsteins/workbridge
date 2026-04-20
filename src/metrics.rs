//! Metrics aggregator for the global Dashboard view.
//!
//! Reads activity logs from the `LocalFileBackend` data directory, including
//! the `archive/` subdirectory where logs are moved when a work item is
//! deleted, and produces a `MetricsSnapshot` of flow metrics. All logic is
//! pure and synchronous; the background thread that drives refresh lives
//! in `spawn_metrics_aggregator`. The UI thread only ever reads the
//! snapshot via a crossbeam channel - never hits disk directly.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::work_item::WorkItemStatus;

/// How often the background aggregator re-scans the activity log directory.
/// 60s is comfortable: the data only changes when the user is actively
/// completing or stage-changing work items, and the dashboard does not
/// need second-by-second freshness.
const AGGREGATOR_REFRESH: Duration = Duration::from_secs(60);

const SECS_PER_DAY: i64 = 86_400;

/// Day index since the Unix epoch (1970-01-01 UTC). Used as the bucket key
/// for all per-day metric maps; opaque but `Ord`-sortable.
pub type DayNumber = i64;

/// Convert a Unix timestamp in seconds to a day index.
pub fn secs_to_day(secs: i64) -> DayNumber {
    secs.div_euclid(SECS_PER_DAY)
}

/// Stuck-item dwell threshold for Review: items currently in Review whose
/// latest stage_change is older than this count as stuck.
pub const STUCK_REVIEW_SECS: i64 = 3 * SECS_PER_DAY;
/// Stuck-item dwell threshold for Blocked.
pub const STUCK_BLOCKED_SECS: i64 = SECS_PER_DAY;

/// Immutable, self-contained snapshot of all metrics used by the Dashboard.
/// Produced by the background aggregator, consumed by the UI render path.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub created_per_day: BTreeMap<DayNumber, u32>,
    pub done_per_day: BTreeMap<DayNumber, u32>,
    pub prs_merged_per_day: BTreeMap<DayNumber, u32>,
    pub backlog_size_per_day: BTreeMap<DayNumber, u32>,
    pub cycle_times_secs: Vec<i64>,
    pub stuck_items: Vec<StuckItem>,
    pub computed_at_secs: i64,
}

#[derive(Debug, Clone)]
pub struct StuckItem {
    pub wi_id: String,
    pub status: WorkItemStatus,
    pub stuck_for_secs: i64,
}

#[derive(Debug, Clone)]
struct ParsedEntry {
    secs: i64,
    kind: ParsedKind,
}

/// Where an item's activity log was loaded from. Historical (flow)
/// metrics include both variants, but point-in-time metrics like
/// `stuck_items` and the trailing edge of `backlog_size_per_day` only
/// consider `Active` items - anything in `archive/` has already been
/// deleted and must not contribute to "currently stuck" or "currently
/// in backlog" counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provenance {
    Active,
    Archived,
}

#[derive(Debug, Clone, Copy)]
enum ParsedKind {
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

/// Parse a WorkItemStatus name into the enum, honoring the serde aliases
/// so old records using "Todo" / "InProgress" still load.
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
/// first stage_change's `from` field tells us the initial status, which
/// is assumed to have been held from the item's first recorded event
/// until the first transition. Items with zero stage_change events
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
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

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
            let last_event_ts = entries.last().map(|e| e.secs).unwrap_or(last.start);
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

/// Resolve the same workbridge work-items directory that
/// `LocalFileBackend::new()` uses, without going through the backend
/// trait. Returns None on platforms where ProjectDirs cannot determine
/// a home directory. Honors `$HOME` overrides, so tests under a temp
/// `HOME` see an isolated metrics directory.
pub fn default_data_dir() -> Option<PathBuf> {
    let proj = crate::side_effects::paths::project_dirs()?;
    Some(proj.data_dir().join("work-items"))
}

/// Spawn the background metrics aggregator. Returns a receiver that the
/// UI timer tick polls (non-blocking) for fresh snapshots. The producer
/// runs a daemon thread that recomputes every `AGGREGATOR_REFRESH` and
/// sends each result; the channel is unbounded so the consumer can drain
/// to the latest value during a single timer tick. Memory growth is
/// bounded because the UI tick fires every ~200ms - far faster than the
/// 60s production rate.
pub fn spawn_metrics_aggregator(data_dir: PathBuf) -> crossbeam_channel::Receiver<MetricsSnapshot> {
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::spawn(move || {
        loop {
            let snapshot = aggregate_from_activity_logs(&data_dir);
            if tx.send(snapshot).is_err() {
                // The receiver has been dropped (App is gone). Exit the
                // worker so we don't leak a busy thread.
                break;
            }
            std::thread::sleep(AGGREGATOR_REFRESH);
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Allocate a fresh tempdir for a test. Returns both the `TempDir`
    /// guard (which removes the directory on drop) and a concrete
    /// `PathBuf` for ergonomic use. The `_name` argument is retained for
    /// call-site self-documentation.
    fn temp_dir(_name: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        (tmp, dir)
    }

    fn write_activity(dir: &Path, id: &str, lines: &[&str]) {
        let path = dir.join(format!("activity-{id}.jsonl"));
        let contents = lines.join("\n") + "\n";
        fs::write(&path, contents).unwrap();
    }

    /// Create a sibling `{id}.json` stub so `load_per_item` classifies
    /// the top-level activity log as `Provenance::Active`. Tests that
    /// represent live items must call this; otherwise an orphan top-
    /// level log is treated as historical (deleted item with failed
    /// archival).
    fn touch_work_item_json(dir: &Path, id: &str) {
        fs::write(dir.join(format!("{id}.json")), "{}").unwrap();
    }

    fn stage(secs: i64, from: &str, to: &str) -> String {
        format!(
            r#"{{"timestamp":"{secs}Z","event_type":"stage_change","payload":{{"from":"{from}","to":"{to}"}}}}"#
        )
    }

    fn pr_merged(secs: i64) -> String {
        format!(r#"{{"timestamp":"{secs}Z","event_type":"pr_merged","payload":{{}}}}"#)
    }

    #[test]
    fn empty_directory_yields_empty_snapshot() {
        let (_tmp, dir) = temp_dir("empty");
        let snap = aggregate_from_activity_logs(&dir);
        assert!(snap.created_per_day.is_empty());
        assert!(snap.done_per_day.is_empty());
        assert!(snap.prs_merged_per_day.is_empty());
        assert!(snap.cycle_times_secs.is_empty());
        assert!(snap.stuck_items.is_empty());
    }

    #[test]
    fn single_item_backlog_to_done_cycle_time() {
        let (_tmp, dir) = temp_dir("cycle-time");
        // Item created at day 0, moved to Done at day 5 (5 * 86400 secs).
        let t0: i64 = 1_700_000_000;
        let t5 = t0 + 5 * 86_400;
        let lines: Vec<String> = vec![
            stage(t0, "Backlog", "Implementing"),
            stage(t5, "Implementing", "Done"),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "abc", &refs);

        let snap = aggregate_from_activity_logs(&dir);

        assert_eq!(snap.cycle_times_secs.len(), 1);
        assert_eq!(snap.cycle_times_secs[0], 5 * 86_400);
        assert_eq!(snap.done_per_day[&secs_to_day(t5)], 1);
        assert_eq!(snap.created_per_day[&secs_to_day(t0)], 1);
    }

    #[test]
    fn pr_merged_counted_independently_of_done() {
        let (_tmp, dir) = temp_dir("pr-merged-drift");
        let t0: i64 = 1_700_000_000;
        // pr_merged on day 1 but no Done transition - this is the "drift"
        // case the Dashboard is designed to surface.
        let lines = [pr_merged(t0)];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "drift", &refs);

        let snap = aggregate_from_activity_logs(&dir);
        assert_eq!(snap.prs_merged_per_day[&secs_to_day(t0)], 1);
        assert!(snap.done_per_day.is_empty());
    }

    #[test]
    fn stuck_review_item_detected() {
        let (_tmp, dir) = temp_dir("stuck-review");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let entered_review = now - 5 * 86_400; // 5 days ago, threshold is 3
        let entered_backlog = entered_review - 86_400;
        let lines = [
            stage(entered_backlog, "Backlog", "Implementing"),
            stage(entered_review, "Implementing", "Review"),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "stuck", &refs);
        touch_work_item_json(&dir, "stuck");

        let snap = aggregate_from_activity_logs(&dir);
        assert_eq!(snap.stuck_items.len(), 1);
        assert_eq!(snap.stuck_items[0].wi_id, "stuck");
        assert_eq!(snap.stuck_items[0].status, WorkItemStatus::Review);
        assert!(snap.stuck_items[0].stuck_for_secs > STUCK_REVIEW_SECS);
    }

    #[test]
    fn fresh_review_item_is_not_stuck() {
        let (_tmp, dir) = temp_dir("fresh-review");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let entered_review = now - 3600; // 1 hour ago
        let entered_backlog = entered_review - 3600;
        let lines = [
            stage(entered_backlog, "Backlog", "Implementing"),
            stage(entered_review, "Implementing", "Review"),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "fresh", &refs);
        touch_work_item_json(&dir, "fresh");

        let snap = aggregate_from_activity_logs(&dir);
        assert!(snap.stuck_items.is_empty());
    }

    #[test]
    fn orphan_top_level_log_without_sibling_json_is_not_stuck() {
        // Regression: if `LocalFileBackend::delete()` unlinks the work
        // item JSON and then the activity log archival fails (cross-
        // device rename, permission error, ...), the orphan log is
        // deliberately left in the top-level data directory so history
        // is preserved. That log must NOT cause the deleted item to
        // show up in the Dashboard's stuck_items list. The JSON is the
        // source of truth for liveness; an orphan top-level log is
        // classified as `Provenance::Archived`.
        let (_tmp, dir) = temp_dir("orphan-not-stuck");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let entered_review = now - 5 * 86_400; // 5 days ago, well past 3-day threshold
        let entered_backlog = entered_review - 86_400;
        let lines = [
            stage(entered_backlog, "Backlog", "Implementing"),
            stage(entered_review, "Implementing", "Review"),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        // Deliberately NO `touch_work_item_json` - this simulates the
        // failed-archival orphan state.
        write_activity(&dir, "orphan", &refs);

        let snap = aggregate_from_activity_logs(&dir);
        assert!(
            snap.stuck_items.is_empty(),
            "orphan top-level logs must never appear in stuck_items, got: {:?}",
            snap.stuck_items
        );
        // Historical events should still contribute: the item was
        // created and transitioned through backlog, so those per-day
        // counts must be populated.
        assert_eq!(snap.created_per_day.values().sum::<u32>(), 1);
    }

    #[test]
    fn orphan_top_level_log_does_not_inflate_trailing_backlog() {
        // Regression sibling to the stuck_items case: an orphan top-
        // level log whose last known state is Backlog must not count
        // toward the trailing edge of `backlog_size_per_day` forever.
        // Without the JSON-liveness check, the aggregator would treat
        // the item as currently in Backlog and add it to every day
        // from its entry through now, inflating current-backlog KPIs.
        let (_tmp, dir) = temp_dir("orphan-not-backlogged");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let today_d = secs_to_day(now);
        let t_enter = (today_d - 10) * SECS_PER_DAY + 3600;
        // Planning -> Backlog with no subsequent transition: the naive
        // interval would stay open indefinitely.
        let lines = [stage(t_enter, "Planning", "Backlog")];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "orphan-bl", &refs);
        // No sibling JSON.

        let snap = aggregate_from_activity_logs(&dir);
        // Days strictly after the last observed event must not count
        // this item - archival-close-interval logic should kick in
        // because the orphan is classified as Archived.
        let d_enter = secs_to_day(t_enter);
        for d in [d_enter, d_enter + 1, d_enter + 5, today_d] {
            assert_eq!(
                snap.backlog_size_per_day.get(&d).copied(),
                Some(0),
                "day {d} must not count the orphan top-level log (d_enter={d_enter}, today={today_d})"
            );
        }
    }

    #[test]
    fn backlog_reconstruction_tracks_membership() {
        let (_tmp, dir) = temp_dir("backlog-reconstruction");
        // Anchor the test to 10 days ago at 01:00 UTC so the timestamps
        // fall inside the aggregator's rolling 365-day window and are
        // offset from day boundaries (avoids alignment edge cases).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let today_d = secs_to_day(now);
        let t_enter = (today_d - 10) * SECS_PER_DAY + 3600;
        let t_exit = t_enter + 2 * SECS_PER_DAY;
        // Two events: the item enters Backlog at t_enter (Planning ->
        // Backlog) and leaves at t_exit (Backlog -> Implementing). The
        // aggregator cannot reconstruct an interval from a single event
        // because the Backlog entry time would be unknown - and that is
        // exactly the incomplete-data case we want to avoid here.
        let lines = [
            stage(t_enter, "Planning", "Backlog"),
            stage(t_exit, "Backlog", "Implementing"),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "bl", &refs);

        let snap = aggregate_from_activity_logs(&dir);
        let d0 = secs_to_day(t_enter);
        let d1 = d0 + 1;
        let d2 = d0 + 2;
        assert_eq!(snap.backlog_size_per_day.get(&d0).copied(), Some(1));
        assert_eq!(snap.backlog_size_per_day.get(&d1).copied(), Some(1));
        assert_eq!(snap.backlog_size_per_day.get(&d2).copied(), Some(0));
    }

    #[test]
    fn archived_logs_contribute_to_snapshot() {
        let (_tmp, dir) = temp_dir("reads-archive");
        let archive = dir.join("archive");
        fs::create_dir_all(&archive).unwrap();
        let t0: i64 = 1_700_000_000;
        let lines_active = [
            stage(t0, "Backlog", "Implementing"),
            stage(t0 + 86_400, "Implementing", "Done"),
        ];
        let refs_active: Vec<&str> = lines_active.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "live", &refs_active);

        let t_archived: i64 = 1_600_000_000;
        let lines_archived = [
            stage(t_archived, "Backlog", "Implementing"),
            stage(t_archived + 3 * 86_400, "Implementing", "Done"),
        ];
        let refs_archived: Vec<&str> = lines_archived.iter().map(|s| s.as_str()).collect();
        write_activity(&archive, "old", &refs_archived);

        let snap = aggregate_from_activity_logs(&dir);
        assert_eq!(snap.cycle_times_secs.len(), 2);
        assert!(snap.cycle_times_secs.contains(&86_400));
        assert!(snap.cycle_times_secs.contains(&(3 * 86_400)));
        assert_eq!(snap.done_per_day[&secs_to_day(t0 + 86_400)], 1);
        assert_eq!(snap.done_per_day[&secs_to_day(t_archived + 3 * 86_400)], 1);
    }

    #[test]
    fn archived_in_review_item_is_not_stuck() {
        // An item that was deleted while in Review must NOT be reported
        // as stuck - it no longer exists on disk. Historical events for
        // the same item still contribute to flow metrics, but point-in-
        // time metrics like stuck_items restrict to live items only.
        let (_tmp, dir) = temp_dir("archived-review-not-stuck");
        let archive = dir.join("archive");
        fs::create_dir_all(&archive).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // Same shape as `stuck_review_item_detected`: entered Review 5
        // days ago, well over the 3-day threshold. But the log lives in
        // archive/, so it must not be flagged.
        let entered_review = now - 5 * 86_400;
        let entered_backlog = entered_review - 86_400;
        let lines = [
            stage(entered_backlog, "Backlog", "Implementing"),
            stage(entered_review, "Implementing", "Review"),
        ];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&archive, "ghost", &refs);

        let snap = aggregate_from_activity_logs(&dir);
        assert!(
            snap.stuck_items.is_empty(),
            "archived items must never appear in stuck_items, got: {:?}",
            snap.stuck_items
        );
    }

    #[test]
    fn archived_in_backlog_item_does_not_inflate_backlog_forever() {
        // An item that was deleted while in Backlog must not count
        // toward `backlog_size_per_day` for every day from its entry
        // through now. The archive timestamp closes its open backlog
        // interval; days strictly after the archive must not include
        // this item.
        let (_tmp, dir) = temp_dir("archived-backlog-not-forever");
        let archive = dir.join("archive");
        fs::create_dir_all(&archive).unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let today_d = secs_to_day(now);
        let t_enter = (today_d - 10) * SECS_PER_DAY + 3600;
        // Only a Planning -> Backlog transition. Its last known state
        // is Backlog at t_enter; archival closes the interval there.
        let lines = [stage(t_enter, "Planning", "Backlog")];
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_activity(&archive, "gone-in-backlog", &refs);

        let snap = aggregate_from_activity_logs(&dir);
        // Days strictly after the archive timestamp must not count this
        // item. The interval is [t_enter, t_enter], which is empty under
        // half-open `[start, end)` semantics, so the item contributes 0
        // to every day including its entry day.
        let d_enter = secs_to_day(t_enter);
        for d in [d_enter, d_enter + 1, d_enter + 5, today_d] {
            assert_eq!(
                snap.backlog_size_per_day.get(&d).copied(),
                Some(0),
                "day {d} must not count the archived-in-backlog item (d_enter={d_enter}, today={today_d})"
            );
        }
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let (_tmp, dir) = temp_dir("corrupt");
        let path = dir.join("activity-corrupt.jsonl");
        fs::write(
            &path,
            "not json at all\n\
             {\"timestamp\":\"1700000000Z\",\"event_type\":\"stage_change\",\"payload\":{\"from\":\"Backlog\",\"to\":\"Implementing\"}}\n\
             {\"timestamp\":\"missing-event-type\"}\n",
        )
        .unwrap();

        let snap = aggregate_from_activity_logs(&dir);
        // Only the middle line is valid; aggregator must not panic.
        assert_eq!(snap.created_per_day.values().sum::<u32>(), 1);
    }

    #[test]
    fn files_without_activity_prefix_are_ignored() {
        let (_tmp, dir) = temp_dir("prefix-filter");
        // A work item JSON and an unrelated file should not be treated as logs.
        fs::write(dir.join("some-item.json"), "{}").unwrap();
        fs::write(dir.join("notes.txt"), "hello").unwrap();

        let snap = aggregate_from_activity_logs(&dir);
        assert!(snap.created_per_day.is_empty());
    }

    #[test]
    fn backlog_boundary_events_land_on_the_correct_day() {
        // Regression: `reconstruct_backlog_per_day` must be symmetric
        // across the UTC midnight boundary when a timestamp lands on
        // the whole-day mark (`secs % 86400 == 0`). Before the fix,
        // `eod = (day + 1) * SECS_PER_DAY` was midnight of `day + 1`,
        // and `iv.start <= eod` let an item entering at that exact
        // instant leak into the prior day's sample. The fix uses
        // `eod = (day + 1) * SECS_PER_DAY - 1` so the reference
        // instant is the literal last second of `day` and half-open
        // interval membership is correct on both sides.
        let (_tmp, dir) = temp_dir("backlog-boundary");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let today_d = secs_to_day(now);
        // Day D: exactly 5 days ago. Midnight of day D in UTC seconds.
        let day_d = today_d - 5;
        let day_d_midnight = day_d * SECS_PER_DAY;

        // Item A: enters Backlog at exactly midnight of day D (start-
        // of-day boundary). It must contribute 0 to day D-1 and 1 to
        // day D, NOT leak into day D-1 just because its timestamp
        // equals day D-1's previous notion of `eod`.
        let lines_a = [stage(day_d_midnight, "Planning", "Backlog")];
        let refs_a: Vec<&str> = lines_a.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "enters-at-midnight", &refs_a);
        touch_work_item_json(&dir, "enters-at-midnight");

        // Item B: leaves Backlog at exactly midnight of day D+1 (end-
        // of-day boundary). It was in Backlog for all of day D. It
        // must count on day D - the end-side symmetric case that the
        // strict-start-only fix would have mishandled.
        let day_d_plus_1_midnight = (day_d + 1) * SECS_PER_DAY;
        let lines_b = [
            stage(day_d_midnight - SECS_PER_DAY, "Planning", "Backlog"),
            stage(day_d_plus_1_midnight, "Backlog", "Implementing"),
        ];
        let refs_b: Vec<&str> = lines_b.iter().map(|s| s.as_str()).collect();
        write_activity(&dir, "leaves-at-midnight", &refs_b);
        touch_work_item_json(&dir, "leaves-at-midnight");

        let snap = aggregate_from_activity_logs(&dir);

        // Day D-1: only item B is in backlog (entered at midnight of
        // D-1, which is day D-1's start). Item A has not yet entered.
        assert_eq!(
            snap.backlog_size_per_day.get(&(day_d - 1)).copied(),
            Some(1),
            "day D-1 must count only item B, not the midnight-entering item A"
        );
        // Day D: both items are in backlog for the entirety of day D.
        // Item A entered at midnight-of-D, item B exits at midnight-
        // of-D+1.
        assert_eq!(
            snap.backlog_size_per_day.get(&day_d).copied(),
            Some(2),
            "day D must count both items across the full day"
        );
        // Day D+1: only item A remains. Item B left at the very
        // start of D+1 and must not leak into D+1's count either.
        assert_eq!(
            snap.backlog_size_per_day.get(&(day_d + 1)).copied(),
            Some(1),
            "day D+1 must count only item A, not the midnight-exiting item B"
        );
    }

    #[test]
    fn freshly_created_backlog_item_counts_in_dashboard() {
        // Regression: before the `created` event seeding was added to
        // `LocalFileBackend::create()`, an item created in Backlog and
        // left untouched had no activity log at all and was invisible
        // to the Dashboard - it did not show up in `created_per_day`,
        // it did not contribute to the current backlog count, and the
        // user would see zero items for the day they created them.
        // This test exercises the real backend create path end-to-end
        // and asserts the item is visible in both places.
        use crate::work_item::WorkItemKind;
        use crate::work_item_backend::{
            CreateWorkItem, LocalFileBackend, RepoAssociationRecord, WorkItemBackend,
        };

        let (_tmp, dir) = temp_dir("fresh-backlog-visible");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();
        backend
            .create(CreateWorkItem {
                title: "Fresh".into(),
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

        let snap = aggregate_from_activity_logs(&dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let today = secs_to_day(now);

        assert_eq!(
            snap.created_per_day.get(&today).copied(),
            Some(1),
            "freshly created item must show up in created_per_day for today"
        );
        // Current-backlog trailing edge: the seeded `created` event
        // with `initial_status: Backlog` must open a backlog interval
        // that the reconstruction sees at `today`.
        assert_eq!(
            snap.backlog_size_per_day.get(&today).copied(),
            Some(1),
            "freshly created Backlog item must count toward current backlog"
        );
        // No stage_change to Done has occurred, so no cycle time yet.
        assert!(snap.cycle_times_secs.is_empty());
        // Stuck_items uses Backlog-is-not-stuck semantics - a fresh
        // Backlog item is not stuck regardless of its dwell time.
        assert!(snap.stuck_items.is_empty());
    }

    #[test]
    fn freshly_created_non_backlog_item_does_not_inflate_backlog() {
        // Symmetric to `freshly_created_backlog_item_counts_in_dashboard`:
        // an item created directly into Planning / Implementing / etc.
        // must show up in `created_per_day` but must NOT be counted in
        // the current-backlog trailing edge - the seeded `created`
        // event carries the initial status so the aggregator can tell
        // the difference.
        use crate::work_item::WorkItemKind;
        use crate::work_item_backend::{
            CreateWorkItem, LocalFileBackend, RepoAssociationRecord, WorkItemBackend,
        };

        let (_tmp, dir) = temp_dir("fresh-planning-visible");
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();
        backend
            .create(CreateWorkItem {
                title: "Planning fresh".into(),
                description: None,
                status: WorkItemStatus::Planning,
                kind: WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: None,
                    pr_identity: None,
                }],
            })
            .unwrap();

        let snap = aggregate_from_activity_logs(&dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let today = secs_to_day(now);

        assert_eq!(snap.created_per_day.get(&today).copied(), Some(1));
        assert_eq!(
            snap.backlog_size_per_day.get(&today).copied(),
            Some(0),
            "Planning item must not inflate current backlog"
        );
    }
}
