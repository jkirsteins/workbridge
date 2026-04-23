//! Global-assistant drawer overlay.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::text::{Line, Text};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::clear::Clear;
use ratatui_widgets::paragraph::Paragraph;
use tui_term::widget::PseudoTerminal;

use super::super::common::dim_background;
use super::super::selection::render_selection_overlay;
use crate::app::App;
use crate::theme::Theme;

pub fn draw_global_drawer(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    // 1. Dim every cell in the buffer to push the background behind the drawer.
    dim_background(buf, area);

    // 2. Compute drawer rect via shared helper (overflow-safe).
    let dl = crate::layout::compute_drawer(area.width, area.height);
    let drawer_width = dl.drawer_width;
    let drawer_height = dl.drawer_height;
    let drawer_x = area.x + 2;
    let drawer_y = area.y + area.height.saturating_sub(drawer_height);
    let drawer_rect = Rect::new(drawer_x, drawer_y, drawer_width, drawer_height);

    // 3. Clear the drawer area and draw the border.
    Clear.render(drawer_rect, buf);

    let drawer_in_scrollback = app
        .global_drawer
        .session
        .as_ref()
        .is_some_and(|e| e.scrollback_offset > 0);
    let drawer_title = if drawer_in_scrollback {
        " Global Assistant [SCROLLBACK] (Ctrl+G to close) "
    } else {
        " Global Assistant (Ctrl+G to close) "
    };

    let block = Block::default()
        .title(drawer_title)
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());
    let inner = block.inner(drawer_rect);
    block.render(drawer_rect, buf);

    // 4. Render the global session PTY or a placeholder.
    match &app.global_drawer.session {
        Some(entry) if entry.alive => {
            if let Ok(mut parser) = entry.parser.lock() {
                // Same clamp as draw_pane_output - see comment there.
                let rows = parser.screen().size().0 as usize;
                let clamped = entry.scrollback_offset.min(rows);
                parser.set_scrollback(clamped);
                let pseudo_term = PseudoTerminal::new(parser.screen());
                pseudo_term.render(inner, buf);
                if let Some(ref sel) = entry.selection {
                    render_selection_overlay(buf, inner, sel);
                }
            } else {
                let text = Text::from(vec![Line::from(""), Line::from("  [render error]")]);
                let paragraph = Paragraph::new(text).style(theme.style_error());
                paragraph.render(inner, buf);
            }
        }
        Some(_) => {
            // Session is dead.
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Global assistant session ended."),
                Line::from("  Press Ctrl+G to restart."),
            ]);
            let paragraph = Paragraph::new(text).style(theme.style_text_muted());
            paragraph.render(inner, buf);
        }
        None => {
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Starting global assistant..."),
            ]);
            let paragraph = Paragraph::new(text).style(theme.style_text_muted());
            paragraph.render(inner, buf);
        }
    }
}
