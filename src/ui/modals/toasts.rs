//! Top-right toast stack.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::text::{Line, Span};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::clear::Clear;
use ratatui_widgets::paragraph::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::Toasts;
use crate::theme::Theme;

/// Draw the top-right transient toast stack. Each toast is a small
/// bordered block; multiple stack vertically with the newest on top.
/// Toasts whose rect would overflow the frame are skipped (rather
/// than clipped) so a small terminal degrades gracefully.
///
/// Takes the `Toasts` subsystem (not a raw `&[Toast]`) so the render
/// path talks to the same narrow API as the mutators.
pub fn draw_toasts(buf: &mut Buffer, toasts: &Toasts, theme: &Theme, frame_area: Rect) {
    const TOAST_HEIGHT: u16 = 3; // bordered block + 1 content row
    const MAX_WIDTH: u16 = 60;
    const MIN_WIDTH: u16 = 16;
    const MARGIN_RIGHT: u16 = 2;
    const MARGIN_TOP: u16 = 1;

    if toasts.is_empty() {
        return;
    }

    // Newest toast first (visually on top of the stack).
    for (index, toast) in toasts.iter().rev().enumerate() {
        let index_u16 = index as u16;
        // `value.len() + 4` = text + two borders + two pad cells.
        let desired = (UnicodeWidthStr::width(toast.text.as_str()) as u16).saturating_add(4);
        let width = desired.clamp(MIN_WIDTH, MAX_WIDTH);

        // Frame too narrow for even the minimum toast width: bail.
        if width > frame_area.width.saturating_sub(MARGIN_RIGHT) {
            return;
        }

        let y = frame_area
            .y
            .saturating_add(MARGIN_TOP)
            .saturating_add(index_u16.saturating_mul(TOAST_HEIGHT));
        if y.saturating_add(TOAST_HEIGHT) > frame_area.y.saturating_add(frame_area.height) {
            // This toast and every further (older) toast would
            // overflow the bottom. Stop stacking.
            return;
        }

        let x = frame_area
            .x
            .saturating_add(frame_area.width)
            .saturating_sub(width)
            .saturating_sub(MARGIN_RIGHT);

        let rect = Rect {
            x,
            y,
            width,
            height: TOAST_HEIGHT,
        };

        // Clear under the toast so it occludes whatever was drawn
        // previously (status bar, context bar, etc.).
        Clear.render(rect, buf);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.style_text());
        let paragraph = Paragraph::new(Line::from(Span::styled(
            toast.text.clone(),
            theme.style_text(),
        )))
        .block(block);
        paragraph.render(rect, buf);
    }
}
