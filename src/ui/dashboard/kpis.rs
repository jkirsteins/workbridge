//! Dashboard KPI strip.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::text::{Line, Text};
use ratatui_core::widgets::Widget;
use ratatui_widgets::paragraph::Paragraph;

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

/// KPI strip: throughput, cycle time p50/p90, current backlog (delta from
/// window start), stuck count.
pub fn draw_dashboard_kpis(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    days: i64,
    from_day: i64,
    today: i64,
    theme: &Theme,
    area: Rect,
) {
    // Throughput: total Done events in the window.
    let throughput: u32 = snapshot
        .done_per_day
        .range(from_day..=today)
        .map(|(_, v)| *v)
        .sum();

    // Cycle time percentiles in days. cycle_times_secs is unsorted; clone
    // and sort locally so we don't mutate the shared snapshot.
    let mut sorted = snapshot.cycle_times_secs.clone();
    sorted.sort_unstable();
    let p50 = percentile_days(&sorted, 50);
    let p90 = percentile_days(&sorted, 90);

    // Backlog now and delta from window start.
    let backlog_now = snapshot
        .backlog_size_per_day
        .get(&today)
        .copied()
        .unwrap_or(0);
    let backlog_then = snapshot
        .backlog_size_per_day
        .get(&from_day)
        .copied()
        .unwrap_or(backlog_now);
    let delta = i64::from(backlog_now) - i64::from(backlog_then);
    let delta_str = if delta >= 0 {
        format!("+{delta}")
    } else {
        format!("{delta}")
    };

    let stuck = snapshot.stuck_items.len();

    let line = format!(
        "Throughput {throughput}/{days}d   Cycle p50 {p50}d   Cycle p90 {p90}d   Backlog now {backlog_now} ({delta_str})   Stuck {stuck}"
    );
    Paragraph::new(Text::from(vec![Line::from(""), Line::from(line)]))
        .style(theme.style_view_mode_hints())
        .render(area, buf);
}

/// Compute the p-th percentile of a sorted vector of seconds, returned in
/// whole days (rounded). Returns 0 if the input is empty.
pub fn percentile_days(sorted_secs: &[i64], pct: u32) -> i64 {
    if sorted_secs.is_empty() {
        return 0;
    }
    // Integer arithmetic with round-half-up: idx = round(pct * (len-1) / 100).
    // Using u128 intermediate to avoid overflow on very large slices.
    let last = sorted_secs.len() - 1;
    // usize -> u128 via try_from (From is not impl'd because usize width is target-dependent).
    let last_u128 = u128::try_from(last).unwrap_or(u128::MAX);
    let numerator = u128::from(pct) * last_u128;
    let idx_u128 = (numerator + 50) / 100;
    let idx = usize::try_from(idx_u128).unwrap_or(last).min(last);
    let v = sorted_secs[idx];
    (v + 43_200) / 86_400 // round to nearest whole day
}
