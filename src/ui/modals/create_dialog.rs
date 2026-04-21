//! The "Create Work Item" dialog + quickstart variant + text-style helper.
use rat_widget::scrolled::Scroll;
use rat_widget::text::TextStyle;
use rat_widget::text_input::TextInput;
use rat_widget::textarea::{TextArea, TextWrap};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::text::Line;
use ratatui_core::widgets::{StatefulWidget, Widget};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::clear::Clear;
use ratatui_widgets::list::{List, ListItem, ListState};
use ratatui_widgets::paragraph::{Paragraph, Wrap};

use super::super::common::{centered_rect_fixed, dim_background, ensure_rendered_cursor};
use crate::create_dialog::{CreateDialog, CreateDialogFocus};
use crate::theme::Theme;

pub const DESC_TEXTAREA_HEIGHT: u16 = 6;

/// Minimum height of the description text area.
///
/// When the terminal is too short for the full dialog, the textarea is
/// the sole flex element and shrinks down to this floor. A 2-row floor
/// keeps the cursor visible while typing even in the worst case; if the
/// terminal cannot accommodate even this, the dialog renders the
/// "terminal too small" fallback instead (see `draw_create_dialog`).
pub const DESC_TEXTAREA_MIN_HEIGHT: u16 = 2;

pub fn draw_create_dialog(buf: &mut Buffer, dialog: &mut CreateDialog, theme: &Theme, area: Rect) {
    const FIXED_INNER_ROWS_WITHOUT_REPOS: u16 = 12;
    const CHROME_ROWS: u16 = 4;

    // Dim the background so the dialog is the clear focal point.
    dim_background(buf, area);

    if dialog.quickstart_mode {
        draw_quickstart_dialog(buf, dialog, theme, area);
        return;
    }

    // Row budget inside the dialog's `inner` rect (i.e. excluding the
    // border and the 1-cell padding on each side):
    //   Title label(1) + Title input(1) + blank(1)
    //   + Description label(1) + Description textarea(preferred
    //     DESC_TEXTAREA_HEIGHT, flex down to DESC_TEXTAREA_MIN_HEIGHT)
    //   + blank(1) + Repos label(1) + repo_lines + blank(1)
    //   + Branch label(1) + Branch input(1) + blank(1)
    //   + error(1) + hint(1)
    // Fixed (non-textarea, non-repo-list): 12 rows.
    // Chrome (outer border + 1-cell top/bottom padding): 4 rows.
    //
    // The Description textarea is the SOLE flex element. When the
    // terminal is too short for the full dialog, the textarea shrinks
    // first; we also clamp the repo list harder (max 4 rows instead
    // of 6) to give the textarea breathing room. Without that
    // discipline, ratatui's constraint solver silently scales every
    // `Length` down proportionally, crushing the textarea to 1-2 rows
    // and causing rat-text's cursor-follow scroll to hide the first
    // row of typed characters (the bug this layout replaced).

    let repo_list_len = dialog.repo_list.len() as u16;
    let repo_lines_preferred = repo_list_len.clamp(1, 6);
    let preferred_with_max_repos =
        FIXED_INNER_ROWS_WITHOUT_REPOS + repo_lines_preferred + DESC_TEXTAREA_HEIGHT + CHROME_ROWS;
    let repo_lines = if preferred_with_max_repos > area.height {
        repo_list_len.clamp(1, 4)
    } else {
        repo_lines_preferred
    };

    let fixed_inner_rows = FIXED_INNER_ROWS_WITHOUT_REPOS + repo_lines;
    let preferred_height = fixed_inner_rows + DESC_TEXTAREA_HEIGHT + CHROME_ROWS;
    let minimum_height = fixed_inner_rows + DESC_TEXTAREA_MIN_HEIGHT + CHROME_ROWS;

    if area.height < minimum_height {
        // The terminal cannot fit even the compact dialog with a
        // 2-row textarea. Show a fallback message so keyboard focus
        // never lands on an invisible textarea: the previous behavior
        // of rendering the normal dialog anyway produced an
        // unresponsive-looking popup where typed characters seemed
        // to vanish.
        draw_create_dialog_too_small(buf, theme, area);
        return;
    }

    let dialog_height = preferred_height.min(area.height);
    // Textarea absorbs whatever vertical space remains after the
    // chrome and fixed-height sections are satisfied, subject to the
    // DESC_TEXTAREA_MIN_HEIGHT floor (guaranteed by `minimum_height`
    // check above).
    let textarea_height = dialog_height - fixed_inner_rows - CHROME_ROWS;
    let dialog_width = (area.width * 60 / 100).max(40).min(area.width);

    let popup = centered_rect_fixed(dialog_width, dialog_height, area);
    Clear.render(popup, buf);

    let block = Block::default()
        .title(" Create Work Item ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());

    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // Inner area with 1-cell padding.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),               // [0] Title label
            Constraint::Length(1),               // [1] Title input
            Constraint::Length(1),               // [2] blank
            Constraint::Length(1),               // [3] Description label
            Constraint::Length(textarea_height), // [4] Description textarea (flex)
            Constraint::Length(1),               // [5] blank
            Constraint::Length(1),               // [6] Repos label
            Constraint::Length(repo_lines),      // [7] Repos list
            Constraint::Length(1),               // [8] blank
            Constraint::Length(1),               // [9] Branch label
            Constraint::Length(1),               // [10] Branch input
            Constraint::Length(1),               // [11] blank
            Constraint::Length(1),               // [12] error / blank
            Constraint::Length(1),               // [13] hint line
            Constraint::Min(0),                  // [14] absorb remaining
        ])
        .split(inner);

    // Title label
    let title_label_style = if dialog.focus_field == CreateDialogFocus::Title {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Title:", title_label_style)).render(sections[0], buf);

    // Title input (rat_widget::text_input::TextInput).
    // Sync focus flag to dialog focus state before rendering.
    dialog
        .title_input
        .focus
        .set(dialog.focus_field == CreateDialogFocus::Title);
    StatefulWidget::render(
        TextInput::new().styles(create_dialog_text_style(theme)),
        sections[1],
        buf,
        &mut dialog.title_input,
    );

    // Description label
    let desc_label_style = if dialog.focus_field == CreateDialogFocus::Description {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Description (optional):", desc_label_style))
        .render(sections[3], buf);

    // Description textarea (rat_widget::textarea::TextArea).
    // - `TextWrap::Word(2)` wraps long descriptions at word boundaries,
    //   preferring breaks in the last two columns before the right margin.
    // - `Scroll::new()` on the vertical axis wires a scrollbar and lets
    //   the textarea scroll when content exceeds DESC_TEXTAREA_HEIGHT.
    dialog
        .description_input
        .focus
        .set(dialog.focus_field == CreateDialogFocus::Description);
    StatefulWidget::render(
        TextArea::new()
            .text_wrap(TextWrap::Word(2))
            .vscroll(Scroll::new())
            .styles(create_dialog_text_style(theme)),
        sections[4],
        buf,
        &mut dialog.description_input,
    );

    // Repos label
    let repos_label_style = if dialog.focus_field == CreateDialogFocus::Repos {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Repos:", repos_label_style)).render(sections[6], buf);

    // Repos list
    if dialog.repo_list.is_empty() {
        let msg = Line::styled("  (no repos configured)", theme.style_text_muted());
        Paragraph::new(msg).render(sections[7], buf);
    } else {
        let items: Vec<ListItem<'_>> = dialog
            .repo_list
            .iter()
            .map(|(path, selected)| {
                let marker = if *selected { "[x]" } else { "[ ]" };
                let line = format!(" {marker} {}", path.display());
                ListItem::new(Line::from(line)).style(theme.style_text())
            })
            .collect();

        let list = List::new(items)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if dialog.focus_field == CreateDialogFocus::Repos {
            state.select(Some(dialog.repo_cursor));
        }

        StatefulWidget::render(list, sections[7], buf, &mut state);
    }

    // Branch label
    let branch_label_style = if dialog.focus_field == CreateDialogFocus::Branch {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Branch (optional):", branch_label_style)).render(sections[9], buf);

    // Branch input (rat_widget::text_input::TextInput).
    dialog
        .branch_input
        .focus
        .set(dialog.focus_field == CreateDialogFocus::Branch);
    StatefulWidget::render(
        TextInput::new().styles(create_dialog_text_style(theme)),
        sections[10],
        buf,
        &mut dialog.branch_input,
    );

    // Error message (if any)
    if let Some(ref err) = dialog.error_message {
        Paragraph::new(Line::styled(err.as_str(), theme.style_error())).render(sections[12], buf);
    }

    // Hint line
    let hint = Line::styled(
        "Enter: Create | Esc: Cancel | Tab: Next field | Space: Toggle repo",
        theme.style_text_muted(),
    );
    Paragraph::new(hint).render(sections[13], buf);
}

/// Render a compact "terminal too small" fallback for the Create Work
/// Item dialog when `area.height` is below the minimum needed to draw
/// the full dialog with even a 2-row description textarea.
///
/// The fallback is a plain `Block` + `Paragraph` so it stays within the
/// ratatui built-in widget vocabulary (no custom layout heroics) and
/// never leaves keyboard focus on an invisible textarea. Esc still
/// dismisses the dialog because the normal dialog event handlers run
pub fn draw_create_dialog_too_small(buf: &mut Buffer, theme: &Theme, area: Rect) {
    let message = "Terminal too small to show the Create Work Item dialog. Enlarge the \
         terminal or press Esc to cancel.";
    let dialog_width = (area.width * 60 / 100).max(40).min(area.width);
    // Minimum usable fallback: border(2) + padding(2) + at least 1 row
    // of message. Prefer 3 rows of message so the wrapped text has
    // room at common widths; shrink if the terminal is really tiny.
    let preferred_height = 7_u16;
    let dialog_height = preferred_height.min(area.height.max(3));
    let popup = centered_rect_fixed(dialog_width, dialog_height, area);
    Clear.render(popup, buf);

    let block = Block::default()
        .title(" Create Work Item ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());
    let block_inner = block.inner(popup);
    block.render(popup, buf);

    Paragraph::new(Line::styled(message, theme.style_error()))
        .wrap(Wrap { trim: true })
        .render(block_inner, buf);
}

/// Render the compact "Quick start - select repo" dialog opened by Ctrl+N
/// when more than one managed repo is configured.
///
/// Unlike the full create dialog (Ctrl+B), this view shows only the repo
/// list - no Title, Description, or Branch fields. The work item's title is
/// hardcoded to `QUICKSTART_TITLE` and its branch is auto-generated by
/// `App::create_quickstart_work_item_for_repo`; the agent later renames the
pub fn draw_quickstart_dialog(buf: &mut Buffer, dialog: &CreateDialog, theme: &Theme, area: Rect) {
    // Compute dialog height: border(1) + padding(1) + Repos label(1)
    //   + repo_lines + blank(1) + error(1) + hint(1) + padding(1) + border(1).
    // Allow up to 8 visible repo rows (the dialog is otherwise small).
    let repo_lines = dialog.repo_list.len().clamp(1, 8) as u16;
    let dialog_height = 1 + 1 + 1 + repo_lines + 1 + 1 + 1 + 1 + 1;
    let dialog_width = (area.width * 60 / 100).max(40).min(area.width);

    let popup = centered_rect_fixed(dialog_width, dialog_height, area);
    Clear.render(popup, buf);

    let block = Block::default()
        .title(" Quick start - select repo ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());

    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // Inner area with 1-cell padding on each side.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // [0] Repos label
            Constraint::Length(repo_lines), // [1] Repos list
            Constraint::Length(1),          // [2] blank
            Constraint::Length(1),          // [3] error / blank
            Constraint::Length(1),          // [4] hint line
            Constraint::Min(0),             // [5] absorb remaining
        ])
        .split(inner);

    // Repos label - always rendered as the focused-style heading because
    // the repo list is the only focusable field in quick-start mode.
    Paragraph::new(Line::styled("Repos:", theme.style_heading())).render(sections[0], buf);

    // Repos list
    if dialog.repo_list.is_empty() {
        let msg = Line::styled("  (no repos configured)", theme.style_text_muted());
        Paragraph::new(msg).render(sections[1], buf);
    } else {
        let items: Vec<ListItem<'_>> = dialog
            .repo_list
            .iter()
            .map(|(path, selected)| {
                let marker = if *selected { "[x]" } else { "[ ]" };
                let line = format!(" {marker} {}", path.display());
                ListItem::new(Line::from(line)).style(theme.style_text())
            })
            .collect();

        let list = List::new(items)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");

        let mut state = ListState::default();
        state.select(Some(dialog.repo_cursor));

        StatefulWidget::render(list, sections[1], buf, &mut state);
    }

    // Error message (if any)
    if let Some(ref err) = dialog.error_message {
        Paragraph::new(Line::styled(err.as_str(), theme.style_error())).render(sections[3], buf);
    }

    // Hint line - quickstart-specific, no Tab/Title/Description guidance.
    let hint = Line::styled(
        "Enter: Create | Esc: Cancel | Up/Down: Move | Space: Select repo",
        theme.style_text_muted(),
    );
    Paragraph::new(hint).render(sections[4], buf);
}

/// Build the shared `TextStyle` used by the Create Work Item dialog's
/// text fields (`TextInput` for Title / Branch, `TextArea` for
/// Description).
///
/// - `style` is the base text color (plain, not dimmed).
/// - `focus` is left at the base style so focused fields don't visually
///   change the run of text itself; the adjacent label (e.g. `Title:`)
///   already switches to the heading color when the field has focus.
/// - `cursor` uses the tab-highlight foreground/background so the caret
///   block is visible against the terminal's default background. This
///   is only honoured when the rat-text cursor type is
pub fn create_dialog_text_style(theme: &Theme) -> TextStyle {
    ensure_rendered_cursor();
    let base = theme.style_text();
    let cursor = ratatui_core::style::Style::default()
        .fg(theme.tab_highlight_fg)
        .bg(theme.tab_highlight_bg);
    TextStyle {
        style: base,
        focus: Some(base),
        cursor: Some(cursor),
        ..Default::default()
    }
}
