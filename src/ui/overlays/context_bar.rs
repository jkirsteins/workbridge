//! Context bar: single-row work-item info strip below the main panels.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::widgets::Widget;
use ratatui_widgets::paragraph::Paragraph;

use crate::app::WorkItemContext;
use crate::theme::Theme;

pub fn draw_context_bar(buf: &mut Buffer, ctx: &WorkItemContext, theme: &Theme, area: Rect) {
    let labels_part = if ctx.labels.is_empty() {
        String::new()
    } else {
        format!(" | {}", ctx.labels.join(", "))
    };

    let full = format!(
        "{} | [{}] | {}{}",
        ctx.title, ctx.stage, ctx.repo_name, labels_part
    );

    // Truncate to fit width. Use char-based indexing for multi-byte safety.
    let width = area.width as usize;
    let display = if full.chars().count() > width {
        if width > 3 {
            let truncated: String = full.chars().take(width - 3).collect();
            format!("{truncated}...")
        } else {
            full.chars().take(width).collect()
        }
    } else {
        full
    };

    let paragraph = Paragraph::new(display).style(theme.style_context());
    paragraph.render(area, buf);
}
