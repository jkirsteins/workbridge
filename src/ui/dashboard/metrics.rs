//! Dashboard chart widgets: done vs merged, created, backlog, sparkline helpers.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::{Color, Style};
use ratatui_core::text::{Line, Span};
use ratatui_core::widgets::Widget;
use ratatui_widgets::barchart::{Bar, BarChart, BarGroup};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::sparkline::Sparkline;

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

/// Color palette for the Dashboard charts. High-contrast pair for the
/// Done-vs-PRs-merged chart (green overlay line on magenta bar) and
/// distinct hues for the two single-series charts.
const DASHBOARD_DONE: Color = Color::LightGreen;
const DASHBOARD_MERGED: Color = Color::LightMagenta;
const DASHBOARD_BACKLOG: Color = Color::Yellow;
const DASHBOARD_CREATED: Color = Color::LightBlue;

/// Build a per-day series sliced to the current dashboard window.
/// Days with no events return zero so each `Vec<usize>` has exactly
/// `to_day - from_day + 1` entries.
pub fn slice_per_day(
    map: &std::collections::BTreeMap<i64, u32>,
    from_day: i64,
    to_day: i64,
) -> Vec<usize> {
    (from_day..=to_day)
        .map(|d| map.get(&d).copied().unwrap_or(0) as usize)
        .collect()
}

/// day is one group of two adjacent bars (green = done, magenta =
/// merged) with a small gap separating it from the next day. This makes
/// each day's `(done, merged)` pair read as a local cluster and avoids
/// the problem of a continuous line chart where the eye has to track
/// two shapes at once.
///
/// Longer windows aggregate into coarser buckets so the chart stays
/// legible: 7d and 30d are daily, 90d groups into weeks, 365d groups
/// into ~monthly buckets of 30 days each. Bar width and group gap are
/// chosen per window so the full chart fills most of the panel without
/// overflowing.
pub fn draw_dashboard_done_vs_merged(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    from_day: i64,
    today: i64,
    _theme: &Theme,
    area: Rect,
) {
    let done = slice_per_day(&snapshot.done_per_day, from_day, today);
    let merged = slice_per_day(&snapshot.prs_merged_per_day, from_day, today);
    let days = (today - from_day + 1).max(1) as usize;

    // Aggregation choice per window. The bar/gap sizing is tuned so the
    // visible chart fills most of a ~90-char-wide inner area without
    // overflowing: `n_buckets * (2 * bar_width + group_gap)` stays under
    // ~90 for all four windows.
    let (bucket_size, bucket_noun, bar_width, group_gap) = if days <= 10 {
        (1_usize, "daily", 4_u16, 2_u16)
    } else if days <= 40 {
        (1_usize, "daily", 1_u16, 1_u16)
    } else if days <= 120 {
        (7_usize, "weekly", 3_u16, 1_u16)
    } else {
        (30_usize, "monthly", 3_u16, 1_u16)
    };

    let bucketed_done = bucket_sum(&done, bucket_size);
    let bucketed_merged = bucket_sum(&merged, bucket_size);
    let n_buckets = bucketed_done.len().max(bucketed_merged.len());

    let max_y: u64 = bucketed_done
        .iter()
        .chain(bucketed_merged.iter())
        .copied()
        .max()
        .unwrap_or(0) as u64;

    let title = Line::from(vec![
        Span::raw(" Done vs PRs merged   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_DONE)),
        Span::raw("done   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_MERGED)),
        Span::raw(format!("merged   ({bucket_noun} buckets) ")),
    ]);
    let days_label = today - from_day + 1;
    let block = Block::default().borders(Borders::ALL).title(title);

    let mut chart = BarChart::default()
        .block(block)
        .bar_width(bar_width)
        .bar_gap(0)
        .group_gap(group_gap)
        .max(max_y.max(1));

    for i in 0..n_buckets {
        let d = bucketed_done.get(i).copied().unwrap_or(0) as u64;
        let m = bucketed_merged.get(i).copied().unwrap_or(0) as u64;
        let group = BarGroup::default().bars(&[
            Bar::default()
                .value(d)
                .style(Style::default().fg(DASHBOARD_DONE)),
            Bar::default()
                .value(m)
                .style(Style::default().fg(DASHBOARD_MERGED)),
        ]);
        chart = chart.data(group);
    }

    chart.render(area, buf);
    draw_bottom_axis_labels(buf, area, days_label);
}

/// Bucket a per-day series by summing every `bucket_size` adjacent days.
/// If `bucket_size <= 1` the input is returned unchanged. Used by the
/// Done-vs-PRs-merged chart to compress long windows into readable
/// weekly / monthly groups.
pub fn bucket_sum(series: &[usize], bucket_size: usize) -> Vec<usize> {
    if bucket_size <= 1 || series.is_empty() {
        return series.to_vec();
    }
    let n_buckets = series.len().div_ceil(bucket_size);
    let mut out = vec![0_usize; n_buckets];
    for (i, &v) in series.iter().enumerate() {
        out[i / bucket_size] += v;
    }
    out
}

/// Resample a per-day series to exactly `target_width` columns for
/// `ratatui_widgets::Sparkline`, which renders one data point per column
/// (truncating the tail if `data.len() > width` and leaving trailing
/// columns blank if `data.len() < width`). We always want the chart to
/// fill its panel edge-to-edge, so the series is resized to the inner
/// width in both directions:
///
/// - **Wide windows (n > w)**: each output column takes the *max* of
///   the contiguous input range `[col*n/w, (col+1)*n/w)`. "Max" (not
///   "sum") keeps the visible amplitude meaningful for both count-
///   per-day and snapshot-per-day series, and matches the natural
///   reading of a sparkline peak.
/// - **Narrow windows (n <= w)**: each output column picks the value
///   at input index `col * n / w`, which stretches each data point
///   across a contiguous column block of width `w/n`. This matches
///   the block-center math used by `draw_bottom_axis_labels` so
///   labels sit exactly above the block they describe.
///
/// Empty input or `target_width == 0` returns an empty vector.
pub fn downsample_for_sparkline(series: &[usize], target_width: usize) -> Vec<u64> {
    if target_width == 0 || series.is_empty() {
        return Vec::new();
    }
    let n = series.len();
    if n >= target_width {
        // Downsample: each output column summarizes one or more days.
        (0..target_width)
            .map(|col| {
                let start = col * n / target_width;
                let end = ((col + 1) * n / target_width).max(start + 1);
                series[start..end.min(n)].iter().copied().max().unwrap_or(0) as u64
            })
            .collect()
    } else {
        // Upsample (stretch): replicate each input across `w/n` columns.
        (0..target_width)
            .map(|col| series[(col * n / target_width).min(n - 1)] as u64)
            .collect()
    }
}
/// Overlay x-axis labels on the bottom border of a chart block.
///
/// A chart for a `days`-day window plots `days` data points where the
/// leftmost point is `(days - 1)` days ago and the rightmost point is
/// today. The function picks four data indices `[0, days/3, 2*days/3,
/// days-1]`, derives the day-ago label for each, and places the label
/// text centered on the corresponding data block's center column. This
/// matches the `downsample_for_sparkline` block-based stretching
/// (`data_idx = col * n / w`), so every intermediate label sits
/// exactly above the block whose value it represents - no straddling
/// between neighboring days.
///
/// Writes directly into the buffer row at `area.bottom() - 1`,
/// overwriting the border `─` characters. Must be called after the
/// block is rendered so the border is already in place for the labels
/// to sit on top of.
pub fn draw_bottom_axis_labels(buf: &mut Buffer, area: Rect, days: i64) {
    if area.width < 20 || area.height == 0 || days < 1 {
        return;
    }
    let y = area.bottom() - 1;
    let x_start = area.left() + 1; // skip left corner
    let x_end = area.right() - 1; // exclusive, skip right corner
    if x_end <= x_start {
        return;
    }
    let inner_width = i64::from(x_end - x_start);

    let n = days.max(1); // number of data points = window size
    let max_ago = n - 1; // oldest day-offset shown in the chart

    // Pick four data indices to label: first, 1/3, 2/3, last.
    // `n.saturating_sub(1)` for the last entry avoids overflow for tiny
    // windows; callers always pass `n >= 7` but the guard is cheap.
    let label_indices: [i64; 4] = [0, n / 3, (2 * n) / 3, n - 1];

    for (label_idx, &data_idx) in label_indices.iter().enumerate() {
        // Day-ago value at this data index. Data index 0 = oldest,
        // data index `n-1` = today (0 days ago).
        let days_ago = max_ago - data_idx;
        let label_text = if days_ago == 0 {
            " now ".to_string()
        } else {
            format!(" -{days_ago}d ")
        };
        let label_len = label_text.chars().count() as i64;

        // Block-based widget mapping gives data point `i` the column
        // range `[i*w/n, (i+1)*w/n)`. The center column of that range
        // is `((2i+1) * w) / (2n)`.
        let block_center_rel = ((2 * data_idx + 1) * inner_width) / (2 * n);

        // Anchor the label: left-aligned to the chart start for the
        // leftmost label, right-aligned for `now`, center-aligned on
        // the block center for the intermediate labels.
        let start_x = match label_idx {
            0 => i64::from(x_start),
            3 => i64::from(x_end) - label_len,
            _ => (i64::from(x_start) + block_center_rel) - label_len / 2,
        };
        let clamped = start_x
            .max(i64::from(x_start))
            .min(i64::from(x_end) - label_len);
        for (i, ch) in label_text.chars().enumerate() {
            let cx = (clamped + i as i64) as u16;
            if cx >= x_end {
                break;
            }
            buf[(cx, y)].set_symbol(ch.encode_utf8(&mut [0; 4]));
        }
    }
}

/// Workitems created per day, rendered as a single-series filled
/// sparkline using `ratatui_widgets::sparkline::Sparkline`. The title
/// carries the y-axis max so the implied scale is readable without an
/// axis widget.
pub fn draw_dashboard_created(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    from_day: i64,
    today: i64,
    _theme: &Theme,
    area: Rect,
) {
    let series = slice_per_day(&snapshot.created_per_day, from_day, today);
    let max_v = series.iter().copied().max().unwrap_or(0);
    let title = Line::from(vec![
        Span::raw(" Created per day   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_CREATED)),
        Span::raw(format!("max {max_v}/day ")),
    ]);
    let days = today - from_day + 1;
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    block.render(area, buf);

    // Downsample to the inner width so long windows (90d/365d) do not
    // get truncated at the tail by the built-in `Sparkline`, which
    // renders exactly one column per data point.
    let data = downsample_for_sparkline(&series, inner.width as usize);
    Sparkline::default()
        .data(&data)
        .style(Style::default().fg(DASHBOARD_CREATED))
        .render(inner, buf);
    draw_bottom_axis_labels(buf, area, days);
}

/// Backlog size over time, rendered as a single-series filled sparkline
/// using `ratatui_widgets::sparkline::Sparkline`. Reconstructed by the
/// aggregator from `stage_change` events; see `metrics::backlog_intervals`.
/// Title shows current and peak values because a flat line at zero is
/// hard to read otherwise.
pub fn draw_dashboard_backlog(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    from_day: i64,
    today: i64,
    _theme: &Theme,
    area: Rect,
) {
    let series = slice_per_day(&snapshot.backlog_size_per_day, from_day, today);
    let peak = series.iter().copied().max().unwrap_or(0);
    let now_value = series.last().copied().unwrap_or(0);
    let title = Line::from(vec![
        Span::raw(" Backlog size over time   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_BACKLOG)),
        Span::raw(format!("now {now_value} / peak {peak} ")),
    ]);
    let days = today - from_day + 1;
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    block.render(area, buf);

    // Downsample to the inner width so long windows (90d/365d) do not
    // get truncated at the tail by the built-in `Sparkline`, which
    // renders exactly one column per data point.
    let data = downsample_for_sparkline(&series, inner.width as usize);
    Sparkline::default()
        .data(&data)
        .style(Style::default().fg(DASHBOARD_BACKLOG))
        .render(inner, buf);
    draw_bottom_axis_labels(buf, area, days);
}

#[cfg(test)]
mod downsample_tests {
    use super::downsample_for_sparkline;

    #[test]
    fn empty_input_returns_empty() {
        let out = downsample_for_sparkline(&[], 10);
        assert!(out.is_empty());
    }

    #[test]
    fn zero_target_width_returns_empty() {
        let out = downsample_for_sparkline(&[1, 2, 3], 0);
        assert!(out.is_empty());
    }

    #[test]
    fn identity_when_n_equals_target_width() {
        // n == w falls into the `n >= target_width` branch. Each output
        // column covers exactly one input index, so values round-trip
        // unchanged (cast to u64).
        let input = [0usize, 3, 1, 7, 2];
        let out = downsample_for_sparkline(&input, input.len());
        assert_eq!(out, vec![0u64, 3, 1, 7, 2]);
    }

    #[test]
    fn strict_downsample_takes_block_max() {
        // n=12, w=4: each output column summarizes n/w=3 input values
        // via max. Block boundaries are [0..3), [3..6), [6..9), [9..12).
        let input = [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let out = downsample_for_sparkline(&input, 4);
        assert_eq!(out, vec![3u64, 6, 9, 12]);
    }

    #[test]
    fn strict_downsample_respects_non_uniform_blocks() {
        // n=5, w=3: floor math gives blocks [0..1), [1..3), [3..5) with
        // the `(start+1).max(end)` guard keeping the first block
        // non-empty. Expected maxes: 10, max(3,7)=7, max(2,9)=9.
        let input = [10usize, 3, 7, 2, 9];
        let out = downsample_for_sparkline(&input, 3);
        assert_eq!(out, vec![10u64, 7, 9]);
    }

    #[test]
    fn strict_upsample_replicates_each_input() {
        // n=3, w=9: each input value should appear in ~w/n=3 adjacent
        // output columns via `series[col * n / w]` block-stretch math.
        let input = [5usize, 8, 2];
        let out = downsample_for_sparkline(&input, 9);
        assert_eq!(out, vec![5u64, 5, 5, 8, 8, 8, 2, 2, 2]);
    }

    #[test]
    fn upsample_never_indexes_out_of_bounds() {
        // For the last output column, (col * n / w) can equal n-1 via
        // floor math; the `.min(n - 1)` clamp must keep the read in
        // bounds. This test exercises a width that is not a clean
        // multiple of n.
        let input = [1usize, 2];
        let out = downsample_for_sparkline(&input, 5);
        // col * 2 / 5 -> 0, 0, 0, 1, 1
        assert_eq!(out, vec![1u64, 1, 1, 2, 2]);
    }
}
