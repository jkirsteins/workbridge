//! Metrics aggregator for the global Dashboard view.
//!
//! Reads activity logs from the `LocalFileBackend` data directory, including
//! the `archive/` subdirectory where logs are moved when a work item is
//! deleted, and produces a `MetricsSnapshot` of flow metrics. All logic is
//! pure and synchronous; the background thread that drives refresh lives
//! in `spawn_metrics_aggregator`. The UI thread only ever reads the
//! snapshot via a crossbeam channel - never hits disk directly.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::work_item::WorkItemStatus;

mod aggregator;

#[cfg(test)]
mod tests;

pub use aggregator::aggregate_from_activity_logs;

/// How often the background aggregator re-scans the activity log directory.
/// 60s is comfortable: the data only changes when the user is actively
/// completing or stage-changing work items, and the dashboard does not
/// need second-by-second freshness.
const AGGREGATOR_REFRESH: Duration = Duration::from_secs(60);

pub const SECS_PER_DAY: i64 = 86_400;

/// Day index since the Unix epoch (1970-01-01 UTC). Used as the bucket key
/// for all per-day metric maps; opaque but `Ord`-sortable.
pub type DayNumber = i64;

/// Convert a Unix timestamp in seconds to a day index.
pub const fn secs_to_day(secs: i64) -> DayNumber {
    secs.div_euclid(SECS_PER_DAY)
}

/// Stuck-item dwell threshold for Review: items currently in Review whose
/// latest `stage_change` is older than this count as stuck.
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

/// Resolve the same workbridge work-items directory that
/// `LocalFileBackend::new()` uses, without going through the backend
/// trait. Returns None on platforms where `ProjectDirs` cannot determine
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
            crate::side_effects::clock::sleep(AGGREGATOR_REFRESH);
        }
    });
    rx
}
