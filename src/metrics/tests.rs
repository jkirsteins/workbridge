//! Unit tests for the metrics aggregator. Kept as a separate file so
//! the test fixtures + assertions stay under the 700-line ceiling.

use std::fs;
use std::path::{Path, PathBuf};

use super::{
    MetricsSnapshot, SECS_PER_DAY, STUCK_REVIEW_SECS, aggregate_from_activity_logs, secs_to_day,
};
use crate::work_item::WorkItemStatus;

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

/// Return the current UNIX wall-clock time in seconds as `i64`, using
/// `try_from` to convert the `u64` result so clippy's `cast_possible_wrap`
/// lint is satisfied without an `as` cast. The conversion only fails for
/// timestamps past year 2262, which is not a real concern for tests.
fn now_secs_i64() -> i64 {
    let secs = crate::side_effects::clock::system_now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs();
    i64::try_from(secs).expect("timestamp fits in i64")
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
    let snap: MetricsSnapshot = aggregate_from_activity_logs(&dir);
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
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
    write_activity(&dir, "drift", &refs);

    let snap = aggregate_from_activity_logs(&dir);
    assert_eq!(snap.prs_merged_per_day[&secs_to_day(t0)], 1);
    assert!(snap.done_per_day.is_empty());
}

#[test]
fn stuck_review_item_detected() {
    let (_tmp, dir) = temp_dir("stuck-review");
    let now = now_secs_i64();
    let entered_review = now - 5 * 86_400; // 5 days ago, threshold is 3
    let entered_backlog = entered_review - 86_400;
    let lines = [
        stage(entered_backlog, "Backlog", "Implementing"),
        stage(entered_review, "Implementing", "Review"),
    ];
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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
    let now = now_secs_i64();
    let entered_review = now - 3600; // 1 hour ago
    let entered_backlog = entered_review - 3600;
    let lines = [
        stage(entered_backlog, "Backlog", "Implementing"),
        stage(entered_review, "Implementing", "Review"),
    ];
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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
    let now = now_secs_i64();
    let entered_review = now - 5 * 86_400; // 5 days ago, well past 3-day threshold
    let entered_backlog = entered_review - 86_400;
    let lines = [
        stage(entered_backlog, "Backlog", "Implementing"),
        stage(entered_review, "Implementing", "Review"),
    ];
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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
    let now = now_secs_i64();
    let today_d = secs_to_day(now);
    let t_enter = (today_d - 10) * SECS_PER_DAY + 3600;
    // Planning -> Backlog with no subsequent transition: the naive
    // interval would stay open indefinitely.
    let lines = [stage(t_enter, "Planning", "Backlog")];
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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
    let now = now_secs_i64();
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
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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
    let refs_active: Vec<&str> = lines_active
        .iter()
        .map(std::string::String::as_str)
        .collect();
    write_activity(&dir, "live", &refs_active);

    let t_archived: i64 = 1_600_000_000;
    let lines_archived = [
        stage(t_archived, "Backlog", "Implementing"),
        stage(t_archived + 3 * 86_400, "Implementing", "Done"),
    ];
    let refs_archived: Vec<&str> = lines_archived
        .iter()
        .map(std::string::String::as_str)
        .collect();
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

    let now = now_secs_i64();
    // Same shape as `stuck_review_item_detected`: entered Review 5
    // days ago, well over the 3-day threshold. But the log lives in
    // archive/, so it must not be flagged.
    let entered_review = now - 5 * 86_400;
    let entered_backlog = entered_review - 86_400;
    let lines = [
        stage(entered_backlog, "Backlog", "Implementing"),
        stage(entered_review, "Implementing", "Review"),
    ];
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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

    let now = now_secs_i64();
    let today_d = secs_to_day(now);
    let t_enter = (today_d - 10) * SECS_PER_DAY + 3600;
    // Only a Planning -> Backlog transition. Its last known state
    // is Backlog at t_enter; archival closes the interval there.
    let lines = [stage(t_enter, "Planning", "Backlog")];
    let refs: Vec<&str> = lines.iter().map(std::string::String::as_str).collect();
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
    let now = now_secs_i64();
    let today_d = secs_to_day(now);
    // Day D: exactly 5 days ago. Midnight of day D in UTC seconds.
    let day_d = today_d - 5;
    let day_d_midnight = day_d * SECS_PER_DAY;

    // Item A: enters Backlog at exactly midnight of day D (start-
    // of-day boundary). It must contribute 0 to day D-1 and 1 to
    // day D, NOT leak into day D-1 just because its timestamp
    // equals day D-1's previous notion of `eod`.
    let lines_a = [stage(day_d_midnight, "Planning", "Backlog")];
    let refs_a: Vec<&str> = lines_a.iter().map(std::string::String::as_str).collect();
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
    let refs_b: Vec<&str> = lines_b.iter().map(std::string::String::as_str).collect();
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
    let now = now_secs_i64();
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
    let now = now_secs_i64();
    let today = secs_to_day(now);

    assert_eq!(snap.created_per_day.get(&today).copied(), Some(1));
    assert_eq!(
        snap.backlog_size_per_day.get(&today).copied(),
        Some(0),
        "Planning item must not inflate current backlog"
    );
}
