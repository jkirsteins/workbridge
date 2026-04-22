//! View-mode header (segmented tab bar).
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::text::Span;
use ratatui_core::widgets::Widget;
use ratatui_widgets::paragraph::Paragraph;
use ratatui_widgets::tabs::Tabs;

use crate::app::{App, ViewMode};
use crate::theme::Theme;

/// Draw the view mode header: a segmented tab bar showing
/// List / Board / Dashboard with contextual keybinding hints on the right.
pub fn draw_view_mode_header(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let selected = match app.view_mode {
        ViewMode::FlatList => 0,
        ViewMode::Board => usize::from(!app.board_drill_down),
        ViewMode::Dashboard => 2,
    };

    let tabs = Tabs::new(vec![" List ", " Board ", " Dashboard "])
        .select(selected)
        .style(theme.style_view_mode_tab())
        .highlight_style(theme.style_view_mode_tab_active())
        .divider("");

    // Keybinding hints (right-aligned).
    let hints = if app.view_mode == ViewMode::Board && !app.board_drill_down {
        "Tab: switch view | <-/->: columns | Shift+arrow: move item | Enter: drill down"
    } else if app.view_mode == ViewMode::Dashboard {
        "Tab: switch view | 1/2/3/4: 7d / 30d / 90d / 365d window"
    } else {
        "Tab: switch view"
    };

    // Split header: tabs on left, hints on right.
    let hints_width = u16::try_from(hints.len())
        .unwrap_or(u16::MAX)
        .saturating_add(1); // +1 for trailing space
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(hints_width)])
        .split(area);

    tabs.render(cols[0], buf);
    Paragraph::new(Span::styled(hints, theme.style_view_mode_hints())).render(cols[1], buf);
}
