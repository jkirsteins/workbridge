//! Metrics dashboard view.
pub mod board_stats;
pub mod kpis;
pub mod metrics;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::{BorderType, Borders};
use ratatui_widgets::paragraph::{Paragraph, Wrap};

pub use self::board_stats::*;
pub use self::kpis::*;
pub use self::metrics::*;
use crate::app::App;
use crate::metrics::secs_to_day;
use crate::theme::Theme;

/// Render the global metrics Dashboard. All data comes from
/// `App.metrics_snapshot`, populated by the background aggregator thread.
/// This function performs zero file I/O - safe on the UI thread per
/// `docs/UI.md` "Blocking I/O Prohibition".
pub fn draw_dashboard_view(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title(format!(
            " Dashboard (window: {}) ",
            app.dashboard_window.label()
        ))
        .border_style(theme.style_border_focused());
    let inner = outer.inner(area);
    outer.render(area, buf);

    let Some(snapshot) = app.metrics.snapshot.as_ref() else {
        Paragraph::new(
            "Computing metrics...\n\nThe background aggregator scans the activity log on first launch. Charts will appear shortly.",
        )
        .style(theme.style_view_mode_hints())
        .wrap(Wrap { trim: true })
        .render(inner, buf);
        return;
    };

    let today = secs_to_day(snapshot.computed_at_secs);
    let days = app.dashboard_window.days();
    let from_day = today - days + 1;

    // Vertical: KPI strip (2 rows: blank + KPIs) + main 2x2 grid.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    draw_dashboard_kpis(buf, snapshot, days, from_day, today, theme, chunks[0]);

    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);
    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(row_chunks[0]);
    let bot_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(row_chunks[1]);

    draw_dashboard_done_vs_merged(buf, snapshot, from_day, today, theme, top_cols[0]);
    draw_dashboard_created(buf, snapshot, from_day, today, theme, top_cols[1]);
    draw_dashboard_backlog(buf, snapshot, from_day, today, theme, bot_cols[0]);
    draw_dashboard_stuck(buf, snapshot, theme, bot_cols[1]);
}
