//! Dashboard "stuck items" list.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::text::{Line, Text};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::paragraph::{Paragraph, Wrap};

use crate::metrics::{MetricsSnapshot, StuckItem};
use crate::theme::Theme;
use crate::work_item::WorkItemStatus;

/// Stuck items list: items currently in Blocked or Review beyond their
/// dwell threshold. Threshold values come from the metrics module.
pub fn draw_dashboard_stuck(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    theme: &Theme,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Stuck items ");
    let inner = block.inner(area);
    block.render(area, buf);

    if snapshot.stuck_items.is_empty() {
        Paragraph::new("None - everything in Review/Blocked is moving.")
            .style(theme.style_view_mode_hints())
            .wrap(Wrap { trim: true })
            .render(inner, buf);
        return;
    }

    let lines: Vec<Line<'_>> = snapshot
        .stuck_items
        .iter()
        .take(inner.height as usize)
        .map(format_stuck_item_line)
        .collect();
    Paragraph::new(Text::from(lines)).render(inner, buf);
}

pub fn format_stuck_item_line(item: &StuckItem) -> Line<'static> {
    let days = item.stuck_for_secs / 86_400;
    let hours = (item.stuck_for_secs % 86_400) / 3600;
    let dwell = if days > 0 {
        format!("{days}d{hours}h")
    } else {
        format!("{hours}h")
    };
    let status_label = match item.status {
        WorkItemStatus::Review => "Review ",
        WorkItemStatus::Blocked => "Blocked",
        _ => "       ",
    };
    Line::from(format!("  {status_label}  {dwell:>6}  {}", item.wi_id))
}
