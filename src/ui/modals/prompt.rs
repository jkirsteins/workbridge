//! Modal prompt dialog: key-choice / text-input / alert variants.
use rat_widget::text_input::TextInput;
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::text::{Line, Span};
use ratatui_core::widgets::{StatefulWidget, Widget};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::{BorderType, Borders};
use ratatui_widgets::clear::Clear;
use ratatui_widgets::paragraph::{Paragraph, Wrap};

use super::super::common::{centered_rect_fixed, dim_background, wrap_text_flat};
use super::create_dialog::create_dialog_text_style;
use crate::theme::Theme;

/// Content variants for prompt dialogs.
///
/// `KeyChoice` presents a question with labelled key options.
/// `TextInput` presents a text field with a hint line.
pub enum PromptDialogKind<'a> {
    KeyChoice {
        title: &'a str,
        body: &'a str,
        options: &'a [(&'a str, &'a str)],
    },
    TextInput {
        title: &'a str,
        body: &'a str,
        input: &'a mut rat_widget::text_input::TextInputState,
        hint: &'a str,
    },
    /// Red-bordered alert for errors/warnings. Dismissed with Enter or Esc.
    Alert { title: &'a str, body: &'a str },
}

/// Draw a modal prompt dialog centered on screen with a dimmed background.
///
/// Prompt dialogs use `BorderType::Rounded` to be visually distinct from
/// other overlays (settings, create dialog) which use plain borders.
pub fn draw_prompt_dialog(buf: &mut Buffer, theme: &Theme, area: Rect, kind: PromptDialogKind<'_>) {
    // 1. Dim the entire background so the dialog is the clear focal point.
    dim_background(buf, area);

    // 2. Compute dialog dimensions.
    let (title, body, inner_height) = match &kind {
        PromptDialogKind::KeyChoice {
            title,
            body,
            options,
        } => {
            // body(1) + blank(1) + options(N) + blank(1)
            let h = 1u16 + 1 + u16::try_from(options.len()).unwrap_or(u16::MAX) + 1;
            (*title, *body, h)
        }
        PromptDialogKind::TextInput {
            title, body, hint, ..
        } => {
            // body(1) + blank(1) + input(1) + blank(1) + hint(1)
            let _ = hint;
            let h = 1u16 + 1 + 1 + 1 + 1;
            (*title, *body, h)
        }
        PromptDialogKind::Alert { title, body } => {
            // Height is computed after dialog_width is known (body may wrap).
            // Use 0 as placeholder; overridden below for Alert.
            (*title, *body, 0u16)
        }
    };

    // Minimum width: longest line + 2 (padding) + 2 (border).
    // Clamp between 40 and 60, further clamped to terminal width.
    let min_content_width = u16::try_from(body.len().max(title.len() + 4)).unwrap_or(u16::MAX);
    let dialog_width = (min_content_width + 4).clamp(40, 60).min(area.width);

    // For Alert dialogs, compute body line count based on actual word-wrapping.
    let inner_height = if matches!(kind, PromptDialogKind::Alert { .. }) {
        // Usable content width: dialog - 2 (border) - 2 (padding).
        let content_width = usize::from(dialog_width.saturating_sub(4).max(1));
        let body_lines = if body.is_empty() {
            1u16
        } else {
            // Use word-wrap simulation to get accurate line count.
            // wrap_text_flat breaks at word boundaries like ratatui's Wrap.
            u16::try_from(wrap_text_flat(body, content_width).len())
                .unwrap_or(u16::MAX)
                .max(1)
        };
        // body(N) + blank(1) + hint(1) + blank(1)
        body_lines + 1 + 1 + 1
    } else {
        inner_height
    };
    // Height: border(2) + blank(1) + inner_height + blank(1) = inner_height + 4.
    let dialog_height = (inner_height + 4).min(area.height);

    // 3. Center and clear the popup area.
    let popup = centered_rect_fixed(dialog_width, dialog_height, area);
    Clear.render(popup, buf);

    // 4. Draw rounded-border block. Alert dialogs use a red border;
    //    all other prompt dialogs use the standard cyan overlay border.
    let border_style = match &kind {
        PromptDialogKind::Alert { .. } => theme.style_border_alert(),
        _ => theme.style_border_overlay(),
    };
    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);
    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // 5. 1-cell padding inside the border.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // 6. Render content rows using a vertical layout.
    match kind {
        PromptDialogKind::KeyChoice { body, options, .. } => {
            render_prompt_key_choice(buf, theme, inner, body, options);
        }
        PromptDialogKind::TextInput {
            body, input, hint, ..
        } => {
            render_prompt_text_input(buf, theme, inner, body, input, hint);
        }
        PromptDialogKind::Alert { body, .. } => {
            render_prompt_alert(buf, theme, inner, body);
        }
    }
}

/// Render the `KeyChoice` body: prompt text, blank line, then one line
/// per (key, description) option.
fn render_prompt_key_choice(
    buf: &mut Buffer,
    theme: &Theme,
    inner: Rect,
    body: &str,
    options: &[(&str, &str)],
) {
    let mut constraints = vec![
        Constraint::Length(1), // body
        Constraint::Length(1), // blank
    ];
    for _ in options {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(0)); // remaining space

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    Paragraph::new(body)
        .style(theme.style_text())
        .render(rows[0], buf);
    // rows[1] is blank.
    for (i, (key_label, description)) in options.iter().enumerate() {
        let line = Line::from(vec![
            Span::styled(*key_label, theme.style_heading()),
            Span::raw("  "),
            Span::styled(*description, theme.style_text()),
        ]);
        Paragraph::new(line).render(rows[2 + i], buf);
    }
}

/// Render the `TextInput` body: prompt text, a focused rat-widget text
/// input, and a hint line below.
fn render_prompt_text_input(
    buf: &mut Buffer,
    theme: &Theme,
    inner: Rect,
    body: &str,
    input: &mut rat_widget::text_input::TextInputState,
    hint: &str,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // body
            Constraint::Length(1), // blank
            Constraint::Length(1), // input field
            Constraint::Length(1), // blank
            Constraint::Length(1), // hint
            Constraint::Min(0),    // remaining
        ])
        .split(inner);

    Paragraph::new(body)
        .style(theme.style_text())
        .render(rows[0], buf);
    // rows[1] is blank.
    // Focused prompt input: render with rat-widget's TextInput so
    // the caret is drawn by the same stateful widget used by the
    // Create Work Item dialog (no custom single-line widget).
    input.focus.set(true);
    StatefulWidget::render(
        TextInput::new().styles(create_dialog_text_style(theme)),
        rows[2],
        buf,
        input,
    );
    // rows[3] is blank.
    Paragraph::new(hint)
        .style(theme.style_text_muted())
        .render(rows[4], buf);
}

/// Render the `Alert` body: word-wrapped error text and a standard
/// `[Enter/Esc] OK` hint line.
fn render_prompt_alert(buf: &mut Buffer, theme: &Theme, inner: Rect, body: &str) {
    let content_w = usize::from(inner.width.max(1));
    let body_lines = if body.is_empty() {
        1u16
    } else {
        // Use word-wrap simulation for accurate line count.
        u16::try_from(wrap_text_flat(body, content_w).len())
            .unwrap_or(u16::MAX)
            .max(1)
    };
    // Rows: body (may wrap to multiple lines), blank, hint.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(body_lines), // body
            Constraint::Length(1),          // blank
            Constraint::Length(1),          // hint
            Constraint::Min(0),             // remaining
        ])
        .split(inner);

    Paragraph::new(body)
        .style(theme.style_error())
        .wrap(Wrap { trim: false })
        .render(rows[0], buf);
    // rows[1] is blank.
    let hint_line = Line::from(vec![
        Span::styled("[Enter/Esc]", theme.style_heading()),
        Span::raw("  "),
        Span::styled("OK", theme.style_text()),
    ]);
    Paragraph::new(hint_line).render(rows[2], buf);
}
