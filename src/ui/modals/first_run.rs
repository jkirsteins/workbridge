//! First-run Ctrl+G global-assistant harness picker modal.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span, Text};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::{BorderType, Borders};
use ratatui_widgets::clear::Clear;
use ratatui_widgets::paragraph::{Paragraph, Wrap};

use crate::app::FirstRunGlobalHarnessModal;
use crate::theme::Theme;

/// Draw the first-run Ctrl+G harness picker modal. Centred, bordered,
/// lists each available harness with its single-letter keybinding. Esc
/// cancels. The key-handling lives in `event.rs`
/// (`handle_first_run_global_harness_modal`).
pub fn draw_first_run_global_harness_modal(
    buf: &mut Buffer,
    modal: &FirstRunGlobalHarnessModal,
    theme: &Theme,
    frame_area: Rect,
) {
    let body_line_count = 3 + modal.available_harnesses.len() + 2;
    let inner_height = body_line_count.min(12) as u16;
    let height = (inner_height + 2).min(frame_area.height);
    let width: u16 = 64.min(frame_area.width);
    let area = Rect {
        x: frame_area.x + frame_area.width.saturating_sub(width) / 2,
        y: frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    Clear.render(area, buf);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Pick a harness for the global assistant ")
        .border_style(theme.style_text());

    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            "Press the highlighted key to choose a harness for Ctrl+G.",
            theme.style_text(),
        )),
        Line::from(Span::styled(
            "The pick is saved to config.toml and can be changed via",
            theme.style_text(),
        )),
        Line::from(Span::styled(
            "`workbridge config set global-assistant-harness <name>`.",
            theme.style_text(),
        )),
        Line::from(""),
    ];
    for kind in &modal.available_harnesses {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  [{}]  ", kind.keybinding()),
                theme.style_text().add_modifier(Modifier::BOLD),
            ),
            Span::styled(kind.display_name(), theme.style_text()),
            Span::styled(
                format!("  ({} on PATH)", kind.binary_name()),
                theme.style_text().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Esc to cancel.",
        theme.style_text().add_modifier(Modifier::DIM),
    )));

    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    paragraph.render(area, buf);
}
