//! Kanban board view rendering.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::text::{Line, Span, Text};
use ratatui_core::widgets::{StatefulWidget, Widget};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::paragraph::Paragraph;

use crate::app::{App, BOARD_COLUMNS};
use crate::layout;
use crate::theme::Theme;
use crate::work_item::{CheckStatus, MergeableState, WorkItemStatus};

use ratatui_widgets::list::{List, ListItem, ListState};

use super::common::{SPINNER_FRAMES, dim_badge_style, wrap_text};

/// Render the board (Kanban) view: four vertical columns for Backlog,
/// Planning, Implementing, and Review. Done items are hidden.
pub fn draw_board_view(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let bl = layout::compute_board(area.width);

    // Split into 4 columns: first 3 fixed width, last gets remainder.
    let constraints = [
        Constraint::Length(bl.column_width),
        Constraint::Length(bl.column_width),
        Constraint::Length(bl.column_width),
        Constraint::Min(0),
    ];
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    for (col_idx, status) in BOARD_COLUMNS.iter().enumerate() {
        let col_area = columns[col_idx];
        let is_selected_col = col_idx == app.board_cursor.column;

        let items = app.items_for_column(*status);
        let count = items.len();

        // Column border style.
        let border_style = if is_selected_col {
            theme.style_board_column_focused()
        } else {
            theme.style_board_column_unfocused()
        };

        let col_title = format!(
            " {} ({}) ",
            match status {
                WorkItemStatus::Backlog => "Backlog",
                WorkItemStatus::Planning => "Planning",
                WorkItemStatus::Implementing => "Implementing",
                WorkItemStatus::Review => "Review",
                _ => "",
            },
            count
        );

        let block = Block::default()
            .title(col_title)
            .title_style(theme.style_board_column_header())
            .borders(Borders::ALL)
            .border_style(border_style);

        if items.is_empty() {
            let empty_text = Text::from(vec![Line::from(""), Line::from("  No items")]);
            let paragraph = Paragraph::new(empty_text)
                .block(block)
                .style(theme.style_text_muted());
            paragraph.render(col_area, buf);
            continue;
        }

        // Inner width for text wrapping (column width minus 2 for borders,
        // minus 2 for highlight symbol space).
        let inner_width = col_area.width.saturating_sub(2).saturating_sub(2) as usize;

        let list_items: Vec<ListItem<'_>> = items
            .iter()
            .enumerate()
            .map(|(row_idx, &wi_idx)| format_board_item(app, wi_idx, inner_width, theme, row_idx))
            .collect();

        let list = List::new(list_items)
            .block(block)
            .highlight_style(theme.style_board_item_highlight())
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if is_selected_col {
            state.select(app.board_cursor.row);
        }

        StatefulWidget::render(list, col_area, buf, &mut state);
    }
}

/// Format a work item for display inside a board column.
/// Uses wrapping (never truncation) to avoid clipping.
pub fn format_board_item<'a>(
    app: &App,
    wi_idx: usize,
    max_width: usize,
    theme: &Theme,
    _row_idx: usize,
) -> ListItem<'a> {
    let Some(wi) = app.work_items.get(wi_idx) else {
        return ListItem::new(Line::from("<invalid>"));
    };

    let mut lines: Vec<Line<'a>> = Vec::new();

    // Compute session presence once so it can gate both the title-prefix
    // badge styling (Blocked / Mergequeue status prefix) and the status
    // indicator line below. The "dim = no session" rule applies to every
    // badge on the row uniformly.
    let has_session = app.session_key_for(&wi.id).is_some();

    // Title line(s) -- wrap, never truncate.
    let title_prefix = if wi.status == WorkItemStatus::Blocked {
        "[BK] "
    } else if wi.status == WorkItemStatus::Mergequeue {
        "[MQ] "
    } else {
        ""
    };
    let title_text = format!("{title_prefix}{}", wi.title);
    let wrapped = wrap_text(&title_text, max_width);
    for (i, wl) in wrapped.into_iter().enumerate() {
        let style = if wi.status == WorkItemStatus::Blocked {
            dim_badge_style(
                theme.style_stage_badge(WorkItemStatus::Blocked),
                has_session,
            )
        } else if wi.status == WorkItemStatus::Mergequeue {
            dim_badge_style(
                theme.style_stage_badge(WorkItemStatus::Mergequeue),
                has_session,
            )
        } else if i == 0 {
            theme.style_text()
        } else {
            theme.style_text_muted()
        };
        lines.push(Line::from(Span::styled(wl, style)));
    }

    // Status indicators on a second line (PR badge, session status).
    let mut indicators: Vec<Span<'a>> = Vec::new();
    let is_working = app.agent_working.contains(&wi.id);
    if is_working {
        let frame = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
        indicators.push(Span::styled(
            frame.to_string(),
            theme.style_badge_session_working(),
        ));
    } else if has_session {
        indicators.push(Span::styled(
            "\u{25CF}".to_string(),
            theme.style_badge_session_idle(),
        ));
    }

    let first_pr = wi.repo_associations.iter().find_map(|a| a.pr.as_ref());
    if let Some(pr) = first_pr {
        // Add space separator if session indicator is already present.
        if !indicators.is_empty() {
            indicators.push(Span::raw(" "));
        }
        let pr_text = format!("PR#{}", pr.number);
        indicators.push(Span::styled(
            pr_text,
            dim_badge_style(theme.style_badge_pr(), has_session),
        ));
        match &pr.checks {
            CheckStatus::Passing => {
                indicators.push(Span::styled(
                    " ok",
                    dim_badge_style(theme.style_badge_ci_pass(), has_session),
                ));
            }
            CheckStatus::Failing => {
                indicators.push(Span::styled(
                    " fail",
                    dim_badge_style(theme.style_badge_ci_fail(), has_session),
                ));
            }
            CheckStatus::Pending => {
                indicators.push(Span::styled(
                    " ...",
                    dim_badge_style(theme.style_badge_ci_pending(), has_session),
                ));
            }
            CheckStatus::None | CheckStatus::Unknown => {}
        }
        if matches!(pr.mergeable, MergeableState::Conflicting) {
            indicators.push(Span::styled(
                " !merge",
                dim_badge_style(theme.style_badge_merge_conflict(), has_session),
            ));
        }
    }
    if !indicators.is_empty() {
        lines.push(Line::from(indicators));
    }

    ListItem::new(Text::from(lines))
}
