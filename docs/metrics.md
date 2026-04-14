# Metrics Dashboard

The Dashboard is a global view (alongside List and Board) that surfaces
flow metrics derived from workbridge's activity logs. It answers
questions the other views can't: "am I shipping?", "where are items
getting stuck?", "is my backlog growing or shrinking?".

## Access

- `Tab` cycles `List -> Board -> Dashboard -> List`.
- Inside the Dashboard, number keys `1` `2` `3` `4` select the rolling
  time window: 7d, 30d, 90d, 365d respectively.
- The window persists for the session only; it resets to 30d on each
  launch.

## Data source

All metrics derive from the **per-work-item activity log** that
`LocalFileBackend` already appends to on every stage transition, PR
event, and review action (see `src/work_item_backend.rs` -
`ActivityEntry`, `append_activity`). The aggregator reads the raw
`activity-*.jsonl` files directly from disk via
`metrics::aggregate_from_activity_logs`; the backend trait
deliberately exposes only the writer (`append_activity`) so the
aggregator and the per-item UI each own their read path. The
aggregator reads two locations:

- `{data_dir}/activity-{uuid}.jsonl` - active work items.
- `{data_dir}/archive/activity-{uuid}.jsonl` - work items that have
  been deleted (manually, via MCP, or via auto-archival). On delete,
  `LocalFileBackend::delete()` **moves** the activity log into the
  `archive/` subdirectory instead of removing it, so historical metrics
  survive work-item lifecycle events. See `docs/CLEANUP.md` "Work item
  deletion" for the full deletion sequence.

Nothing else is persisted. No separate metrics DB, no snapshot cache,
no duplicate event log. The activity log is the single source of
truth; the Dashboard is a pure derived view.

### Invariant 6 compliance

Invariant 6 (`docs/invariants.md`) states: "if it can be derived from
git or GitHub, it must not be stored." Workbridge stage transitions,
mergequeue-origin merges, and plan-set events are **not** derivable
from git or GitHub - they are workbridge-native flow metadata. The
activity log has always persisted this data for non-metrics purposes
(the per-work-item UI history) and the retention change simply
extends its lifetime past the owning work item. No new persistence
category is introduced. The invariant's persistent-state bullet list
has been updated to mention per-item and archived activity logs
explicitly so the list reflects reality.

## Event types consumed

The aggregator understands these `ActivityEntry.event_type` values:

| Event type | Payload fields used | Used for |
|---|---|---|
| `created` | `initial_status` | Seeds the timeline so a freshly created item is visible before any stage_change happens. Parsed as a synthetic `stage_change` with `from == to == initial_status`. |
| `stage_change` | `from`, `to` | Done counts, cycle time, backlog reconstruction, stuck-item detection |
| `pr_merged` | (none) | PRs merged count |
| Any other | ignored | - |

The aggregator skips entries with malformed JSON, unknown event types,
or missing required fields. Unparseable lines do not abort the read -
the valid entries in the rest of the file still contribute.

`LocalFileBackend::create()` writes the `{id}.json` record and then
appends a single `created` entry to the activity log. Without this
seeding, an item that is created and left untouched is invisible to
the Dashboard: it has no activity log, so the aggregator never sees
it in `created_per_day`, in the current-backlog trailing edge, or
anywhere else. If the initial `append_activity` fails (e.g. disk
full) the JSON is still authoritative and the item is usable; only
the Dashboard loses that one entry until some later event appends
the first log line.

## Snapshot shape

`src/metrics.rs` exposes `MetricsSnapshot`, a self-contained value
returned by `aggregate_from_activity_logs(data_dir)`:

- `created_per_day: BTreeMap<DayNumber, u32>` - first-seen day for
  each work item (earliest event in its log, bucketed by day).
- `done_per_day: BTreeMap<DayNumber, u32>` - `stage_change -> Done`
  events bucketed by day.
- `prs_merged_per_day: BTreeMap<DayNumber, u32>` - `pr_merged` events
  bucketed by day.
- `backlog_size_per_day: BTreeMap<DayNumber, u32>` - point-in-time
  Backlog size at end of each day, reconstructed by replaying all
  `stage_change` events. Covers the full 365-day rolling window.
  Archived (deleted) items have any still-open Backlog interval
  closed at the timestamp of their last recorded event, so they
  stop counting toward the backlog as soon as they were deleted and
  never inflate the trailing "Backlog now" edge.
- `cycle_times_secs: Vec<i64>` - time from first Backlog entry to
  first Done transition, per work item that reached Done inside the
  full history. Includes archived items (flow history survives
  deletion).
- `stuck_items: Vec<StuckItem>` - items currently in Review longer
  than 3 days or Blocked longer than 1 day. Thresholds are hardcoded
  in v1. Restricted to live (non-archived) items: if a work item
  was deleted while in Review or Blocked, its archived log does not
  contribute to this list - it no longer exists on disk and "stuck
  now" is by definition a live-item property.

### Live vs archived provenance

`aggregate_from_activity_logs` tags each loaded per-item log as
`Active` or `Archived`. The rule is **not** purely file location -
the work item JSON is the source of truth for liveness. A top-level
`activity-{id}.jsonl` is classified as `Active` only if the sibling
`{id}.json` record still exists. A log from `{data_dir}/archive/`
is always `Archived`. A top-level log without a sibling JSON (the
orphan state left behind if `LocalFileBackend::delete()` unlinks
the JSON but the archival rename then fails) is classified as
`Archived` so the deleted item does not show up in the point-in-
time stuck_items list or in the current-backlog trailing edge; its
historical events still contribute to flow metrics.

Flow metrics (`created_per_day`, `done_per_day`, `prs_merged_per_day`,
`cycle_times_secs`, and the bulk of `backlog_size_per_day`) include
both provenances so history survives deletion. Point-in-time metrics
(`stuck_items` and the trailing edge of `backlog_size_per_day`)
only consider `Active` items. If the same id appears in both
locations - e.g. a user recreated a work item with the same id
after deletion - the active copy wins and the archived log is
ignored for that id.
- `computed_at_secs: i64` - wall-clock Unix seconds when the snapshot
  was built. Used by the UI to align "now" to the right edge of the
  charts.

`DayNumber` is an opaque `i64` day index since the Unix epoch,
computed via `secs_to_day(secs)` (`secs.div_euclid(86_400)`). No
`chrono` dependency - the project's existing `now_iso8601()` already
stores timestamps as `"{secs}Z"` plain decimal and the metrics code
parses that format directly.

## Background aggregator

`spawn_metrics_aggregator(data_dir)` spawns a daemon thread that
recomputes a fresh `MetricsSnapshot` every 60 seconds and sends it
through a `crossbeam_channel::unbounded` sender. The thread exits if
the receiver is dropped. `App.metrics_rx` holds the receiver and
`poll_metrics_snapshot` drains it on each salsa timer tick, keeping
only the latest snapshot (the unbounded channel lets the UI tick
quickly catch up if multiple snapshots queue during a UI freeze).

This follows the project's "Blocking I/O Prohibition" pattern
(`docs/UI.md`). The UI thread never reads activity logs directly -
render paths only touch `App.metrics_snapshot`, which is pure
in-memory state.

## Rendering

The Dashboard view is a 2x2 grid of four panels plus a KPI strip:

```
+-- Dashboard (window: 30d) -------------------------------------+
| Throughput N/30d  Cycle p50 Xd  Cycle p90 Yd  Backlog N (+/-)  |
+-- Done vs PRs merged ---+-- Created per day -------------------+
|                         |                                      |
|   grouped bar chart     |   filled sparkline                   |
|                         |                                      |
+- -30d - -20d - -10d - now +- -30d - -20d - -10d - now ---------+
+-- Backlog size over time-+-- Stuck items ----------------------+
|                          |  Review  5d0h  stuck-review-01      |
|   filled sparkline       |  Review  3d0h  stuck-review-00      |
|                          |  Blocked 2d0h  stuck-blocked-00     |
+- -30d - -20d - -10d - now +--------------------------------+
```

### Done vs PRs merged (top-left)

**Grouped bar chart** with two bars per bucket: green = workitems
Done, magenta = PRs merged. Bucket size depends on the window:

| Window | Bucket | Buckets visible |
|---|---|---|
| 7d | daily | 7 |
| 30d | daily | 30 |
| 90d | weekly | 13 |
| 365d | monthly (30 days) | 12 |

Aggregating longer windows into coarser buckets keeps the bar density
readable. Bar width and group gap are chosen per window so each chart
fills most of its panel without overflowing.

**Interpretation**: per bucket, the green bar height is the Done
count and the magenta bar height is the PR-merged count. A tall
green bar next to a short magenta bar signals "dropped work" (Done
without merge). A short green bar next to a tall magenta bar signals
"sync bug" (merge without Done).

### Created per day (top-right)

Single-series filled sparkline (1/8-cell Unicode block glyphs) using
`ratatui_widgets::sparkline::Sparkline`. The title shows `max N/day`
so the y-scale is readable even without axis labels. For windows
wider than the chart panel (90d / 365d) the series is downsampled to
the inner width with `downsample_for_sparkline` so the tail of the
window is not silently truncated.

### Backlog size over time (bottom-left)

Single-series filled sparkline (`ratatui_widgets::sparkline::Sparkline`)
showing the reconstructed backlog size per day. "Now" and "peak"
values are shown in the title. The reconstruction walks each work
item's `stage_change` events, inferring the initial status from the
`from` field of the first transition, and samples the in-Backlog set
at end-of-day UTC for each day in the window. Long windows are
downsampled to the chart panel's inner width so every day's peak is
visible instead of the tail being truncated.

### Stuck items (bottom-right)

Plain `Paragraph` listing items currently in Blocked or Review whose
latest `stage_change` is older than the threshold. Sorted by dwell
time (longest first).

### KPI strip

One-line text header above the grid:
`Throughput X/Nd  Cycle p50 Xd  Cycle p90 Yd  Backlog now N (delta)  Stuck N`.

- Throughput: total Done events in the current window.
- Cycle p50 / p90: percentile of `cycle_times_secs`, rounded to days.
- Backlog now + delta: current backlog size and delta from window
  start.
- Stuck: count of entries in `stuck_items`.

### X-axis labels

Every chart panel has labels overlaid on the bottom border showing
day offsets at 0% / 33% / 66% / 100% of the chart width. Rendered by
`draw_bottom_axis_labels` (`src/ui.rs`), which writes directly to
the bottom border row of the block after the chart has rendered. The
labels update automatically when the window changes.

### Widgets used

All charts use `ratatui-widgets` built-ins: `BarChart` for the
Done-vs-PRs-merged grouped chart, `Sparkline` for the Created-per-day
and Backlog-over-time filled-area charts, and `Paragraph` + `Block`
for the KPI strip and Stuck items list. No custom chart widgets are
required; `Sparkline` renders filled-area charts at 1/8-cell
sub-resolution using the `symbols::bar::NINE_LEVELS` glyph set.

## Verification

- **Unit tests**: `cargo test metrics` exercises the aggregator over
  synthetic activity logs (empty, single-item, multi-item,
  archive-only, backlog reconstruction, stuck-item detection, drift
  between Done and pr_merged, corrupt line skipping).
  `cargo test delete_archives` exercises the retention change in
  `LocalFileBackend`.
- **Manual TUI test**: `workbridge seed-dashboard <dir>` populates a
  target directory with ~22 active + 15 archived synthetic items
  spanning ~400 days. Run workbridge under an isolated `$HOME`
  pointing at that directory and verify the charts render as
  expected. See `docs/CLEANUP.md` for the explicit flow.

The seeder is a CLI subcommand on the main workbridge binary (not a
separate `src/bin/` target) so it reuses the real serde types and
cannot drift from the production write path.
