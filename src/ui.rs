use ratatui_core::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{StatefulWidget, Widget},
};
use ratatui_widgets::{
    block::Block,
    borders::Borders,
    clear::Clear,
    list::{List, ListItem, ListState},
    paragraph::Paragraph,
};
use tui_term::widget::PseudoTerminal;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DisplayEntry, FocusPanel, SettingsListFocus, WorkItemContext};
use crate::config;
use crate::create_dialog::{CreateDialog, CreateDialogFocus};
use crate::layout;
use crate::theme::Theme;
use crate::work_item::{BackendType, CheckStatus, PrState, WorkItemError, WorkItemStatus};

/// Render the entire UI: left panel (work item list) and right panel
/// (session output), plus optional context bar and status bar at the bottom.
///
/// Buffer-based rendering entry point. Called by the rat-salsa render
/// callback. All rendering uses Widget::render(area, buf) and
/// StatefulWidget::render(widget, area, buf, &mut state) directly.
pub fn draw_to_buffer(area: Rect, buf: &mut Buffer, app: &App, theme: &Theme) {
    // Vertical split: main area + optional 1-row context bar + optional 1-row status bar.
    let has_context = app.selected_work_item_context().is_some();
    let has_status = app.status_message.is_some();

    let mut constraints = vec![Constraint::Min(0)];
    if has_context {
        constraints.push(Constraint::Length(1));
    }
    if has_status {
        constraints.push(Constraint::Length(1));
    }
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let main_area = vertical[0];
    let mut next_slot = 1;

    let context_area = if has_context {
        let a = vertical[next_slot];
        next_slot += 1;
        Some(a)
    } else {
        None
    };

    let status_area = if has_status {
        Some(vertical[next_slot])
    } else {
        None
    };

    // Horizontal split: left panel, right panel.
    let pl = layout::compute(main_area.width, main_area.height, 0);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(pl.left_width), Constraint::Min(0)])
        .split(main_area);

    draw_work_item_list(buf, app, theme, chunks[0]);
    draw_pane_output(buf, app, theme, chunks[1]);

    // Context bar (persistent work-item info).
    if let Some(area) = context_area
        && let Some(ctx) = app.selected_work_item_context()
    {
        draw_context_bar(buf, &ctx, theme, area);
    }

    // Status bar (transient messages).
    if let Some(area) = status_area
        && let Some(msg) = &app.status_message
    {
        let style = if app.shutting_down {
            theme.style_status_shutdown()
        } else {
            theme.style_status()
        };
        let status = Paragraph::new(msg.as_str()).style(style);
        status.render(area, buf);
    }

    // Settings overlay (rendered on top of everything).
    if app.show_settings {
        draw_settings_overlay(buf, app, theme, area);
    }

    // Create dialog overlay (rendered on top of everything).
    if app.create_dialog.visible {
        draw_create_dialog(buf, &app.create_dialog, theme, area);
    }
}

/// Draw the left panel containing the grouped work item list.
fn draw_work_item_list(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    // When the settings overlay is open, dim background panels so the
    // overlay is the clear focal point.
    let border_style = if app.show_settings {
        theme.style_border_unfocused()
    } else if app.focus == FocusPanel::Left {
        theme.style_border_focused()
    } else {
        theme.style_border_unfocused()
    };

    let block = Block::default()
        .title(format!(" Work Items ({}) ", app.work_items.len()))
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(border_style);

    if app.display_list.is_empty() {
        let text = Text::from(vec![
            Line::from(""),
            Line::from("  No work items."),
            Line::from(""),
            Line::from("  Press Ctrl+N"),
            Line::from("  to create one."),
        ]);
        let paragraph = Paragraph::new(text)
            .block(block)
            .style(theme.style_text_muted());
        paragraph.render(area, buf);
        return;
    }

    // Available width inside the block borders, minus highlight symbol space.
    // The List widget reserves space for the highlight symbol ("> ") on all rows.
    let inner_width = area.width.saturating_sub(2).saturating_sub(2) as usize;

    let items: Vec<ListItem> = app
        .display_list
        .iter()
        .enumerate()
        .map(|(i, entry)| match entry {
            DisplayEntry::GroupHeader { label, count } => {
                let text = format!("{label} ({count})");
                ListItem::new(Line::from(vec![Span::styled(
                    text,
                    theme.style_group_header(),
                )]))
            }
            DisplayEntry::UnlinkedItem(idx) => {
                let selected = app.selected_item == Some(i);
                format_unlinked_item(app, *idx, inner_width, theme, selected)
            }
            DisplayEntry::WorkItemEntry(idx) => {
                let selected = app.selected_item == Some(i);
                format_work_item_entry(app, *idx, inner_width, theme, selected)
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(theme.style_tab_highlight_bg())
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(app.selected_item);

    StatefulWidget::render(list, area, buf, &mut state);
}

/// Format an unlinked PR entry for the left panel list.
fn format_unlinked_item<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
    is_selected: bool,
) -> ListItem<'a> {
    let Some(unlinked) = app.unlinked_prs.get(idx) else {
        return ListItem::new(Line::from("  ? <invalid>"));
    };

    let pr_badge = format!("PR#{}", unlinked.pr.number);
    let mut draft_suffix = String::new();
    if unlinked.pr.is_draft {
        draft_suffix.push_str(" draft");
    }
    let right = format!("{pr_badge}{draft_suffix}");

    // Title: branch name for unlinked items.
    let title = &unlinked.branch;

    // Layout: "? title    PR#N [draft]"
    // Reserve space: 2 for "? " prefix, right.len() for badge, 1 for gap.
    let prefix = "? ";
    let available = max_width
        .saturating_sub(prefix.width())
        .saturating_sub(right.width())
        .saturating_sub(1);
    let truncated_title = truncate_str(title, available);

    let padding =
        max_width.saturating_sub(prefix.width() + truncated_title.width() + right.width());
    let pad_str: String = " ".repeat(padding);

    let (marker_style, title_style, badge_style) = if is_selected {
        let hl = theme.style_tab_highlight();
        (hl, hl, hl)
    } else {
        (
            theme.style_unlinked_marker(),
            theme.style_text(),
            theme.style_badge_pr(),
        )
    };

    ListItem::new(Line::from(vec![
        Span::styled(prefix.to_string(), marker_style),
        Span::styled(truncated_title, title_style),
        Span::raw(pad_str),
        Span::styled(right, badge_style),
    ]))
}

/// Format a work item entry for the left panel list.
///
/// Returns a 2-line ListItem:
///   Line 1: title (+ PR badge + CI badge if present)
///   Line 2: repo-name  branch-name  [no wt] (all muted)
fn format_work_item_entry<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
    is_selected: bool,
) -> ListItem<'a> {
    let Some(wi) = app.work_items.get(idx) else {
        return ListItem::new(Line::from("<invalid>"));
    };

    // -- Line 1: title + badges --

    // Build the right-side badge string.
    let mut right_parts: Vec<(String, ratatui_core::style::Style)> = Vec::new();

    // PR badge: show first PR if any.
    let first_pr = wi.repo_associations.iter().find_map(|a| a.pr.as_ref());
    if let Some(pr) = first_pr {
        let pr_text = format!("PR#{}", pr.number);
        let pr_style = if pr.state == PrState::Merged {
            theme.style_text_muted()
        } else {
            theme.style_badge_pr()
        };
        right_parts.push((pr_text, pr_style));

        // CI badge.
        match &pr.checks {
            CheckStatus::Passing => {
                right_parts.push((" ok".to_string(), theme.style_badge_ci_pass()));
            }
            CheckStatus::Failing => {
                right_parts.push((" fail".to_string(), theme.style_badge_ci_fail()));
            }
            CheckStatus::Pending => {
                right_parts.push((" ...".to_string(), theme.style_badge_ci_pending()));
            }
            CheckStatus::None | CheckStatus::Unknown => {}
        }
    }

    // Multi-repo indicator.
    let repo_count = wi.repo_associations.len();
    if repo_count > 1 {
        right_parts.push((format!(" [{repo_count} repos]"), theme.style_text_muted()));
    }

    // Stage badge + title. Done items omit the badge since the DONE group
    // header already communicates their status.
    let badge = wi.status.badge_text();
    let prefix = if wi.status == WorkItemStatus::Done {
        String::new()
    } else {
        format!("{badge} ")
    };
    // Minimum number of display columns reserved for the title so it never
    // vanishes when badges consume all available width.
    const MIN_TITLE_BUDGET: usize = 5;

    let space_for_content = max_width.saturating_sub(prefix.width());

    // Drop badges from the right until the title gets at least MIN_TITLE_BUDGET
    // columns (or we run out of badges to drop).
    let mut visible_badge_count = right_parts.len();
    let mut right_text: String = right_parts.iter().map(|(s, _)| s.as_str()).collect();
    while visible_badge_count > 0 {
        let title_budget = space_for_content
            .saturating_sub(right_text.width())
            .saturating_sub(1); // gap between title and badges
        if title_budget >= MIN_TITLE_BUDGET {
            break;
        }
        visible_badge_count -= 1;
        right_text = right_parts[..visible_badge_count]
            .iter()
            .map(|(s, _)| s.as_str())
            .collect();
    }

    let available = space_for_content
        .saturating_sub(right_text.width())
        .saturating_sub(if right_text.is_empty() { 0 } else { 1 });

    // When selected, the List widget only sets bg (via style_tab_highlight_bg).
    // We apply fg per-span here so title+badge get the original highlight look
    // (Black + BOLD) while branch metadata stays muted (DarkGray).
    let hl = theme.style_tab_highlight();
    let (title_style, badge_style, right_badge_style, meta_style) = if is_selected {
        (
            hl,
            hl,
            hl,
            ratatui_core::style::Style::default().fg(ratatui_core::style::Color::DarkGray),
        )
    } else {
        let ts = if wi.status == WorkItemStatus::Done {
            theme.style_done_item()
        } else {
            theme.style_text()
        };
        (
            ts,
            theme.style_stage_badge(&wi.status),
            ratatui_core::style::Style::default(), // right badges have their own per-part styles
            theme.style_text_muted(),
        )
    };

    // Wrap the title: first line shares space with badge + right badges,
    // continuation lines get the full panel width with no indent.
    let title_lines = wrap_two_widths(&wi.title, available.max(1), max_width);
    let first_title = title_lines.first().cloned().unwrap_or_default();

    let padding =
        max_width.saturating_sub(prefix.width() + first_title.width() + right_text.width());
    let pad_str: String = " ".repeat(padding);

    let mut line1_spans = if wi.status == WorkItemStatus::Done {
        vec![
            Span::styled(first_title, title_style),
            Span::raw(pad_str),
        ]
    } else {
        vec![
            Span::styled(badge.to_string(), badge_style),
            Span::raw(" "),
            Span::styled(first_title, title_style),
            Span::raw(pad_str),
        ]
    };
    for (text, style) in &right_parts[..visible_badge_count] {
        let s = if is_selected {
            right_badge_style
        } else {
            *style
        };
        line1_spans.push(Span::styled(text.clone(), s));
    }
    let line1 = Line::from(line1_spans);

    // -- Line 2+: metadata (only if the work item has meaningful context) --
    //
    // A work item can be in different states of completeness:
    // - Has branch + worktree + PR: show branch (repo) with all badges
    // - Has branch but no worktree: show branch (repo) [no wt]
    // - Has no branch (pre-planning): show just repo name, no tags
    // - Has no repo associations: show nothing (shouldn't happen per invariant 1)

    let first_assoc = wi.repo_associations.first();
    let has_branch = first_assoc.is_some_and(|a| a.branch.is_some());
    let has_worktree = first_assoc.is_some_and(|a| a.worktree_path.is_some());

    let mut lines = vec![line1];

    // Title continuation lines (no indent).
    for title_cont in title_lines.iter().skip(1) {
        lines.push(Line::from(vec![Span::styled(
            title_cont.clone(),
            title_style,
        )]));
    }

    if has_branch {
        // Branch + [no wt] indicator. Repo is shown in the group header.
        let branch_name = first_assoc.and_then(|a| a.branch.as_deref()).unwrap_or("");
        let wt_indicator = if has_worktree { "" } else { " [no wt]" };

        let meta_content = format!("{branch_name}{wt_indicator}");
        for wrapped_line in wrap_text(&meta_content, max_width) {
            lines.push(Line::from(vec![Span::styled(wrapped_line, meta_style)]));
        }
    }
    // No repo associations = no line 2 (invariant 1 violation, but render gracefully)

    ListItem::new(lines)
}

/// Word-wrap a string to fit within max_width display columns.
/// Breaks at word boundaries (space, /, -, paren) when possible.
/// Wraps to as many lines as needed - no artificial cap.
/// When `indent` is true, continuation lines are indented with 4 spaces.
/// Every output line is guaranteed to be <= max_width display columns.
fn wrap_text_impl(s: &str, max_width: usize, indent: bool) -> Vec<String> {
    const INDENT_STR: &str = "    ";
    let indent_width = if indent { INDENT_STR.width() } else { 0 };

    if max_width == 0 {
        return vec![];
    }

    if s.width() <= max_width {
        return vec![s.to_string()];
    }

    let mut lines = Vec::new();
    let mut remaining = s;

    while !remaining.is_empty() {
        // Continuation lines have less space due to indent
        let effective_width = if lines.is_empty() {
            max_width
        } else {
            max_width.saturating_sub(indent_width)
        };

        // Guard: if effective_width is 0 (max_width < indent), force at least 1 char
        let effective_width = effective_width.max(1);

        if remaining.width() <= effective_width {
            lines.push(remaining.to_string());
            break;
        }

        // Find the byte index where cumulative display width reaches effective_width
        let byte_limit = remaining
            .char_indices()
            .scan(0usize, |acc, (i, c)| {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                *acc += w;
                Some((i, *acc))
            })
            .take_while(|&(_, cum_w)| cum_w <= effective_width)
            .last()
            .map(|(i, _)| {
                // Advance past this char to get the end byte index
                i + remaining[i..].chars().next().map_or(0, |c| c.len_utf8())
            })
            .unwrap_or_else(|| {
                // First char is already wider than effective_width; take it anyway
                remaining.chars().next().map_or(0, |c| c.len_utf8())
            });

        // Try to break at a word boundary within the limit
        let break_at = remaining[..byte_limit]
            .rfind([' ', '/', '-', '('])
            .map(|i| i + 1)
            .unwrap_or(byte_limit);

        let (line, rest) = remaining.split_at(break_at);
        lines.push(line.to_string());

        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        remaining = trimmed;
    }

    // Prepend indent to continuation lines, but only if max_width can
    // accommodate it without exceeding the width guarantee.
    if indent && max_width > indent_width {
        for line in lines.iter_mut().skip(1) {
            *line = format!("{INDENT_STR}{line}");
        }
    }

    lines
}

/// Word-wrap with 4-space continuation indent (default behavior).
fn wrap_text(s: &str, max_width: usize) -> Vec<String> {
    wrap_text_impl(s, max_width, true)
}

/// Word-wrap with no continuation indent.
fn wrap_text_flat(s: &str, max_width: usize) -> Vec<String> {
    wrap_text_impl(s, max_width, false)
}

/// Word-wrap where the first line has a narrower budget than subsequent lines.
/// Used for titles where line 1 shares space with badge + right badges.
fn wrap_two_widths(s: &str, first_width: usize, rest_width: usize) -> Vec<String> {
    if first_width == 0 || s.is_empty() {
        return vec![];
    }
    // If it fits on the first line, done.
    if s.width() <= first_width {
        return vec![s.to_string()];
    }
    // Break the first line at first_width.
    let first_lines = wrap_text_flat(s, first_width);
    let first = first_lines[0].clone();
    // Reconstruct the remainder from the original string.
    let used_bytes = first.trim_end().len();
    let rest = s[used_bytes..].trim_start();
    if rest.is_empty() {
        return vec![first];
    }
    let mut lines = vec![first];
    lines.extend(wrap_text_flat(rest, rest_width));
    lines
}

/// Truncate a string to fit within max_len display columns.
/// If truncated, appends "..".
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.width() <= max_len {
        s.to_string()
    } else if max_len <= 2 {
        truncate_to_width(s, max_len)
    } else {
        let mut result = truncate_to_width(s, max_len - 2);
        result.push_str("..");
        result
    }
}

/// Take chars from `s` until their cumulative display width reaches `max_cols`.
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > max_cols {
            break;
        }
        width += cw;
        result.push(c);
    }
    result
}

/// Format a WorkItemError into a user-facing message and optional suggestion.
fn format_work_item_error(error: &WorkItemError) -> (String, Option<String>) {
    match error {
        WorkItemError::MultiplePrsForBranch {
            repo_path,
            branch,
            count,
        } => (
            format!(
                "{count} open PRs for branch '{branch}' in {}",
                repo_path.display()
            ),
            Some("Close duplicate PRs to resolve.".into()),
        ),
        WorkItemError::DetachedHead {
            repo_path,
            worktree_path,
        } => (
            format!(
                "Detached HEAD at {} ({})",
                worktree_path.display(),
                repo_path.display()
            ),
            Some("Check out a branch in this worktree.".into()),
        ),
        WorkItemError::IssueNotFound {
            repo_path,
            issue_number,
        } => (
            format!("Issue #{issue_number} not found in {}", repo_path.display()),
            Some("The issue may have been deleted or the number is wrong.".into()),
        ),
        WorkItemError::CorruptBackendRecord { reason, backend } => (
            format!("Corrupt {backend:?} record: {reason}"),
            Some("Delete and re-create this work item.".into()),
        ),
        WorkItemError::WorktreeGone {
            repo_path,
            expected_path,
        } => (
            format!(
                "Worktree missing: {} ({})",
                expected_path.display(),
                repo_path.display()
            ),
            Some("The worktree directory was removed from disk.".into()),
        ),
    }
}

/// Draw a structured detail view for a work item with no active session.
///
/// Shows title, status, backend type, repo, branch, worktree, PR, issue,
/// and errors, followed by a prompt to start a session.
fn draw_work_item_detail(
    buf: &mut Buffer,
    wi: Option<&crate::work_item::WorkItem>,
    theme: &Theme,
    block: Block<'_>,
    area: Rect,
) {
    let Some(wi) = wi else {
        let text = Text::from(vec![
            Line::from(""),
            Line::from("  Press Enter to start"),
            Line::from("  a session."),
        ]);
        let paragraph = Paragraph::new(text)
            .block(block)
            .style(theme.style_text_muted());
        paragraph.render(area, buf);
        return;
    };

    let first_assoc = wi.repo_associations.first();
    let label_style = theme.style_heading();
    let none_style = theme.style_text_muted();

    let status_str = match wi.status {
        WorkItemStatus::Backlog => "Backlog",
        WorkItemStatus::Planning => "Planning",
        WorkItemStatus::Implementing => "Implementing",
        WorkItemStatus::Blocked => "Blocked",
        WorkItemStatus::Review => "Review",
        WorkItemStatus::Done => "Done",
    };

    let backend_str = match wi.backend_type {
        BackendType::LocalFile => "Local file",
        BackendType::GithubIssue => "GitHub issue",
        BackendType::GithubProject => "GitHub project",
    };

    let repo_str = first_assoc
        .map(|a| a.repo_path.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());

    let branch_str = first_assoc
        .and_then(|a| a.branch.as_deref())
        .unwrap_or("(none)");

    let worktree_str = first_assoc
        .and_then(|a| a.worktree_path.as_ref())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());

    let pr_str = first_assoc
        .and_then(|a| a.pr.as_ref())
        .map(|pr| format!("#{} - {}", pr.number, pr.title))
        .unwrap_or_else(|| "(none)".to_string());

    let issue_str = first_assoc
        .and_then(|a| a.issue.as_ref())
        .map(|issue| format!("#{} - {}", issue.number, issue.title))
        .unwrap_or_else(|| "(none)".to_string());

    let errors_str = if wi.errors.is_empty() {
        "(none)".to_string()
    } else {
        wi.errors
            .iter()
            .map(|e| format_work_item_error(e).0)
            .collect::<Vec<_>>()
            .join("; ")
    };

    // Helper: style a value as muted if it is "(none)", otherwise default.
    let val_style = |s: &str| -> ratatui_core::style::Style {
        if s == "(none)" {
            none_style
        } else {
            theme.style_text()
        }
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(format!("  {}", wi.title), theme.style_text())),
        Line::from(""),
    ];

    // Each detail row: "  Label:      value"
    let fields: Vec<(&str, &str)> = vec![
        ("Status", status_str),
        ("Backend", backend_str),
        ("Repo", &repo_str),
        ("Branch", branch_str),
        ("Worktree", &worktree_str),
        ("PR", &pr_str),
        ("Issue", &issue_str),
        ("Errors", &errors_str),
    ];

    for (label, value) in &fields {
        lines.push(Line::from(vec![
            Span::styled(format!("  {label:<12}"), label_style),
            Span::styled(value.to_string(), val_style(value)),
        ]));
    }

    lines.push(Line::from(""));
    let hint = match wi.status {
        WorkItemStatus::Backlog => "  Press Shift+Right to move to Planning.",
        WorkItemStatus::Done => "  Done.",
        _ => "  Press Enter to start a session.",
    };
    lines.push(Line::from(Span::styled(hint, none_style)));

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text).block(block);
    paragraph.render(area, buf);
}

/// Draw the right panel showing captured PTY output.
/// Uses vt100::Parser + tui-term PseudoTerminal for full ANSI color rendering.
///
/// The active session is determined by the currently selected work item:
/// - If selected item is a work item with a session -> render PseudoTerminal
/// - If selected item is a work item without a session -> prompt to start
/// - If selected item is an unlinked PR -> prompt to import
/// - If nothing selected -> show welcome message
fn draw_pane_output(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    // When the settings overlay is open, dim background panels.
    let border_style = if app.show_settings {
        theme.style_border_unfocused()
    } else if app.focus == FocusPanel::Right {
        theme.style_border_input()
    } else {
        theme.style_border_default()
    };

    let title = if app.focus == FocusPanel::Right {
        " Claude Code [INPUT] "
    } else {
        " Claude Code "
    };

    let block = Block::default()
        .title(title)
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(border_style);

    // Determine what to show based on the selected display list entry.
    let selected_entry = app.selected_item.and_then(|idx| app.display_list.get(idx));

    match selected_entry {
        Some(DisplayEntry::WorkItemEntry(wi_idx)) => {
            // Check if the review gate is running for this work item.
            let review_gate_active = app
                .work_items
                .get(*wi_idx)
                .map(|wi| app.review_gate_wi.as_ref() == Some(&wi.id))
                .unwrap_or(false);

            if review_gate_active {
                let spinner_chars = [b'|', b'/', b'-', b'\\'];
                let frame = app.review_gate_spinner_frame as usize % spinner_chars.len();
                let spinner = spinner_chars[frame] as char;
                let text = Text::from(vec![
                    Line::from(""),
                    Line::from(format!("  {spinner} Running review gate...")),
                    Line::from(""),
                    Line::from("  Checking implementation against plan."),
                ]);
                let paragraph = Paragraph::new(text)
                    .block(block)
                    .style(theme.style_text_muted());
                paragraph.render(area, buf);
                return;
            }

            let session_key = app
                .work_items
                .get(*wi_idx)
                .and_then(|wi| app.session_key_for(&wi.id));
            let session_entry = session_key.as_ref().and_then(|key| app.sessions.get(key));

            match session_entry {
                Some(entry) if !entry.alive => {
                    let text = Text::from(vec![
                        Line::from(""),
                        Line::from("  Session has ended."),
                        Line::from(""),
                        Line::from("  Press Enter to start"),
                        Line::from("  a new session."),
                    ]);
                    let paragraph = Paragraph::new(text).block(block).style(theme.style_error());
                    paragraph.render(area, buf);
                }
                Some(entry) => {
                    // Lock the shared parser to get the current screen state.
                    if let Ok(parser) = entry.parser.lock() {
                        let pseudo_term = PseudoTerminal::new(parser.screen()).block(block);
                        pseudo_term.render(area, buf);
                    } else {
                        // Parser lock poisoned - show a fallback message.
                        let text = Text::from(vec![Line::from(""), Line::from("  [render error]")]);
                        let paragraph =
                            Paragraph::new(text).block(block).style(theme.style_error());
                        paragraph.render(area, buf);
                    }
                }
                None => {
                    let wi = app.work_items.get(*wi_idx);
                    let errors = wi.map(|w| &w.errors);
                    let has_errors = errors.is_some_and(|e| !e.is_empty());

                    if has_errors {
                        let mut lines = vec![
                            Line::from(""),
                            Line::from(Span::styled("  Errors:", theme.style_error())),
                        ];
                        for error in errors.unwrap() {
                            lines.push(Line::from(""));
                            let (msg, suggestion) = format_work_item_error(error);
                            lines.push(Line::from(Span::styled(
                                format!("  - {msg}"),
                                theme.style_error(),
                            )));
                            if let Some(hint) = suggestion {
                                lines.push(Line::from(Span::styled(
                                    format!("    {hint}"),
                                    theme.style_text_muted(),
                                )));
                            }
                        }
                        lines.push(Line::from(""));
                        let hint = match wi.map(|w| &w.status) {
                            Some(WorkItemStatus::Backlog) => {
                                "  Press Shift+Right to move to Planning."
                            }
                            Some(WorkItemStatus::Done) => "  Done.",
                            _ => "  Press Enter to start a session.",
                        };
                        lines.push(Line::from(Span::styled(
                            hint,
                            theme.style_text_muted(),
                        )));
                        let text = Text::from(lines);
                        let paragraph = Paragraph::new(text).block(block);
                        paragraph.render(area, buf);
                    } else {
                        draw_work_item_detail(buf, wi, theme, block, area);
                    }
                }
            }
        }
        Some(DisplayEntry::UnlinkedItem(_)) => {
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Press Enter to import"),
                Line::from("  this PR as a work item."),
            ]);
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(theme.style_text_muted());
            paragraph.render(area, buf);
        }
        _ => {
            // Nothing selected or non-selectable entry.
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Welcome to workbridge"),
                Line::from(""),
                Line::from("  Ctrl+N    - Create work item"),
                Line::from("  Up/Down   - Navigate items"),
                Line::from("  Enter     - Open session / Import"),
                Line::from("  Ctrl+]    - Return to item list"),
                Line::from("  Ctrl+D    - Delete work item"),
                Line::from("  ?         - Settings"),
                Line::from("  Q/Ctrl+Q  - Quit"),
            ]);
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(theme.style_text_muted());
            paragraph.render(area, buf);
        }
    }
}

/// Draw the work-item context bar showing title, repo path, and labels.
fn draw_context_bar(buf: &mut Buffer, ctx: &WorkItemContext, theme: &Theme, area: Rect) {
    let labels_part = if ctx.labels.is_empty() {
        String::new()
    } else {
        format!(" | {}", ctx.labels.join(", "))
    };

    let activity_part = match ctx.last_activity {
        Some(ref a) => format!(" | {a}"),
        None => String::new(),
    };

    let full = format!(
        "{} | [{}] | {}{}{}",
        ctx.title, ctx.stage, ctx.repo_path, labels_part, activity_part
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

/// Return a centered rect using the given percentage of the outer rect.
fn centered_rect(percent_x: u16, percent_y: u16, outer: Rect) -> Rect {
    let popup_width = outer.width * percent_x / 100;
    let popup_height = outer.height * percent_y / 100;
    let x = outer.x + (outer.width.saturating_sub(popup_width)) / 2;
    let y = outer.y + (outer.height.saturating_sub(popup_height)) / 2;
    Rect::new(x, y, popup_width, popup_height)
}

/// Maximum visible rows in each repo list before scrolling kicks in.
const REPOS_LIST_MAX_ROWS: u16 = 6;

/// Draw the settings overlay: a centered popup with structured sections.
///
/// Layout (top to bottom):
///   - Config source (2 lines)
///   - Base directories (header + entries)
///   - Repos section: horizontal split of Active and Excluded lists
///   - Defaults (2 lines)
///   - Hint line
fn draw_settings_overlay(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let popup = centered_rect(70, 80, area);
    Clear.render(popup, buf);

    let block = Block::default()
        .title(" Settings (press ? or Esc to close) ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());

    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // Add 1-cell padding inside the overlay border on all sides.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };

    // Build managed repo items.
    let managed_repos = &app.active_repo_cache;
    let mut managed_items: Vec<ListItem<'_>> = Vec::new();
    for entry in managed_repos {
        let source_label = match entry.source {
            config::RepoSource::Explicit => "explicit",
            config::RepoSource::Discovered => "discovered",
        };
        let marker = if entry.git_dir_present { "+" } else { "-" };
        managed_items.push(
            ListItem::new(format!(
                " {marker} {} ({source_label})",
                entry.path.display()
            ))
            .style(theme.style_text()),
        );
    }

    // Build available repo items (discovered but not managed).
    let available_entries = app.available_repos();
    let mut available_items: Vec<ListItem<'_>> = Vec::new();
    for entry in &available_entries {
        let marker = if entry.git_dir_present { "+" } else { "-" };
        available_items.push(
            ListItem::new(format!(" {marker} {}", entry.path.display())).style(theme.style_text()),
        );
    }

    // Compute repos section height.
    let managed_count = managed_items.len();
    let available_count = available_items.len();
    let max_count = managed_count.max(available_count);
    let repos_visible = if max_count == 0 {
        1
    } else {
        (max_count as u16).min(REPOS_LIST_MAX_ROWS)
    };
    let repos_section_height = repos_visible + 2; // +2 for block borders

    // Count base_dirs lines.
    let base_dirs_lines: u16 = if app.config.base_dirs.is_empty() {
        1
    } else {
        app.config.base_dirs.len() as u16
    };

    let source_height = 2;
    let base_dirs_height = 1 + base_dirs_lines + 1;
    let defaults_height = 3;
    let hint_height = 1;

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(source_height),
            Constraint::Length(base_dirs_height),
            Constraint::Length(repos_section_height),
            Constraint::Length(1), // blank
            Constraint::Length(defaults_height),
            Constraint::Length(hint_height),
            Constraint::Min(0), // absorb remaining space
        ])
        .split(inner);

    // Section 0: Config source.
    let source_text = Text::from(vec![
        Line::styled("Config source:", theme.style_heading()),
        Line::from(format!("  {}", app.config.source)),
    ]);
    Paragraph::new(source_text).render(sections[0], buf);

    // Section 1: Base directories.
    let mut base_lines = vec![Line::styled("Base directories:", theme.style_heading())];
    if app.config.base_dirs.is_empty() {
        base_lines.push(Line::styled("  (none)", theme.style_text_muted()));
    } else {
        for dir in &app.config.base_dirs {
            let expanded = config::expand_tilde(dir);
            let marker = if expanded.is_dir() { "+" } else { "-" };
            base_lines.push(Line::from(format!("  {marker} {dir}")));
        }
    }
    Paragraph::new(Text::from(base_lines)).render(sections[1], buf);

    // Section 2: Repos - horizontal split of Active and Excluded lists.
    let repo_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(sections[2]);

    // Managed repos list (left).
    let managed_border = if app.settings_list_focus == SettingsListFocus::Managed {
        theme.style_border_focused()
    } else {
        theme.style_border_subtle()
    };
    let managed_title = format!(" Managed repos ({}) ", managed_count);
    let managed_block = Block::default()
        .title(managed_title.as_str())
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(managed_border);

    if managed_items.is_empty() {
        let empty =
            Paragraph::new(Line::styled("  (none)", theme.style_text_muted())).block(managed_block);
        empty.render(repo_cols[0], buf);
    } else {
        let list = List::new(managed_items)
            .block(managed_block)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if app.settings_list_focus == SettingsListFocus::Managed {
            state.select(Some(
                app.settings_repo_selected
                    .min(managed_count.saturating_sub(1)),
            ));
        }
        StatefulWidget::render(list, repo_cols[0], buf, &mut state);
    }

    // Available repos list (right).
    let available_border = if app.settings_list_focus == SettingsListFocus::Available {
        theme.style_border_focused()
    } else {
        theme.style_border_subtle()
    };
    let available_title = format!(" Available ({}) ", available_count);
    let available_block = Block::default()
        .title(available_title.as_str())
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(available_border);

    if available_items.is_empty() {
        let empty = Paragraph::new(Line::styled("  (none)", theme.style_text_muted()))
            .block(available_block);
        empty.render(repo_cols[1], buf);
    } else {
        let list = List::new(available_items)
            .block(available_block)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if app.settings_list_focus == SettingsListFocus::Available {
            state.select(Some(
                app.settings_available_selected
                    .min(available_count.saturating_sub(1)),
            ));
        }
        StatefulWidget::render(list, repo_cols[1], buf, &mut state);
    }

    // Section 4: Defaults.
    let defaults_text = Text::from(vec![
        Line::styled("Defaults:", theme.style_heading()),
        Line::from(format!(
            "  worktree_dir: {}",
            app.config.defaults.worktree_dir
        )),
        Line::from(format!(
            "  branch_issue_pattern: {}",
            app.config.defaults.branch_issue_pattern
        )),
    ]);
    Paragraph::new(defaults_text).render(sections[4], buf);

    // Section 5: Hint line.
    let hint = Line::styled(
        "Tab: switch list, Enter: move, Up/Down: navigate",
        theme.style_text_muted(),
    );
    Paragraph::new(hint).render(sections[5], buf);
}

/// Draw the work item creation dialog as a centered popup overlay.
///
/// Layout:
///   +-- Create Work Item ---------------------------------+
///   |                                                     |
///   |  Title:                                             |
///   |  [_______________________________________________]  |
///   |                                                     |
///   |  Repos:                                             |
///   |  [x] /path/to/repo-a                                |
///   |  [ ] /path/to/repo-b                                |
///   |                                                     |
///   |  Branch (optional):                                 |
///   |  [_______________________________________________]  |
///   |                                                     |
///   |  [error message if any]                             |
///   |  Enter: Create  |  Esc: Cancel  |  Tab: Next field  |
///   +-----------------------------------------------------+
/// Height of the description text area (visible lines).
pub const DESC_TEXTAREA_HEIGHT: u16 = 3;

fn draw_create_dialog(buf: &mut Buffer, dialog: &CreateDialog, theme: &Theme, area: Rect) {
    // Compute dialog height based on content.
    // Rows: border(1) + blank(1) + "Title:" label(1) + input(1) + blank(1)
    //   + "Description:" label(1) + textarea(3) + blank(1)
    //   + "Repos:" label(1) + repo_lines(max 6) + blank(1)
    //   + "Branch:" label(1) + input(1) + blank(1)
    //   + error_line(1) + hint(1) + border(1)
    let repo_lines = dialog.repo_list.len().clamp(1, 6) as u16;
    let dialog_height =
        2 + 2 + 1 + 1 + DESC_TEXTAREA_HEIGHT + 1 + 1 + repo_lines + 1 + 2 + 1 + 2 + 2;
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
            Constraint::Length(1),                    // [0] Title label
            Constraint::Length(1),                    // [1] Title input
            Constraint::Length(1),                    // [2] blank
            Constraint::Length(1),                    // [3] Description label
            Constraint::Length(DESC_TEXTAREA_HEIGHT), // [4] Description textarea
            Constraint::Length(1),                    // [5] blank
            Constraint::Length(1),                    // [6] Repos label
            Constraint::Length(repo_lines),           // [7] Repos list
            Constraint::Length(1),                    // [8] blank
            Constraint::Length(1),                    // [9] Branch label
            Constraint::Length(1),                    // [10] Branch input
            Constraint::Length(1),                    // [11] blank
            Constraint::Length(1),                    // [12] error / blank
            Constraint::Length(1),                    // [13] hint line
            Constraint::Min(0),                       // [14] absorb remaining
        ])
        .split(inner);

    // Title label
    let title_label_style = if dialog.focus_field == CreateDialogFocus::Title {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Title:", title_label_style)).render(sections[0], buf);

    // Title input
    draw_text_input_field(
        buf,
        &dialog.title_input,
        theme,
        sections[1],
        dialog.focus_field == CreateDialogFocus::Title,
    );

    // Description label
    let desc_label_style = if dialog.focus_field == CreateDialogFocus::Description {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Description (optional):", desc_label_style))
        .render(sections[3], buf);

    // Description textarea
    draw_text_area_field(
        buf,
        &dialog.description_input,
        theme,
        sections[4],
        dialog.focus_field == CreateDialogFocus::Description,
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
    Paragraph::new(Line::styled("Branch (optional):", branch_label_style))
        .render(sections[9], buf);

    // Branch input
    draw_text_input_field(
        buf,
        &dialog.branch_input,
        theme,
        sections[10],
        dialog.focus_field == CreateDialogFocus::Branch,
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

/// Draw a simple text input field with a visual cursor indicator.
///
/// When focused, the text is rendered with a cursor position marker.
/// When unfocused, just the text is shown dimmed.
fn draw_text_input_field(
    buf: &mut Buffer,
    input: &crate::create_dialog::SimpleTextInput,
    theme: &Theme,
    area: Rect,
    focused: bool,
) {
    let text = input.text();
    let inner_width = area.width.saturating_sub(2) as usize; // 1 char padding each side

    if focused {
        let cursor_pos = input.cursor_char_pos();
        // Build the display: text with a cursor block character.
        let before: String = text.chars().take(cursor_pos).collect();
        let cursor_char: String = text
            .chars()
            .nth(cursor_pos)
            .map(|c| c.to_string())
            .unwrap_or_else(|| " ".to_string());
        let after: String = text.chars().skip(cursor_pos + 1).collect();

        // Truncate to fit. Simple approach: if text is longer than inner_width,
        // scroll so cursor is visible.
        let total_chars = text.chars().count().max(cursor_pos + 1);
        let (display_before, display_cursor, display_after) = if total_chars <= inner_width {
            (before, cursor_char, after)
        } else {
            // Scroll window to keep cursor visible.
            let start = if cursor_pos >= inner_width {
                cursor_pos - inner_width + 1
            } else {
                0
            };
            let b: String = text.chars().skip(start).take(cursor_pos - start).collect();
            let c: String = text
                .chars()
                .nth(cursor_pos)
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| " ".to_string());
            let remaining = inner_width.saturating_sub(cursor_pos - start + 1);
            let a: String = text.chars().skip(cursor_pos + 1).take(remaining).collect();
            (b, c, a)
        };

        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(display_before, theme.style_text()),
            Span::styled(
                display_cursor,
                ratatui_core::style::Style::default()
                    .fg(theme.tab_highlight_fg)
                    .bg(theme.tab_highlight_bg),
            ),
            Span::styled(display_after, theme.style_text()),
        ]);
        Paragraph::new(line).render(area, buf);
    } else {
        // Unfocused: show text dimmed.
        let display: String = if text.is_empty() {
            "(empty)".to_string()
        } else {
            text.chars().take(inner_width).collect()
        };
        let line = Line::from(vec![
            Span::raw(" "),
            Span::styled(display, theme.style_text_muted()),
        ]);
        Paragraph::new(line).render(area, buf);
    }
}

/// Draw a multi-line text area field for the description input.
///
/// Shows visible lines from the textarea with cursor when focused.
/// When unfocused, shows the first few lines dimmed.
fn draw_text_area_field(
    buf: &mut Buffer,
    textarea: &crate::create_dialog::SimpleTextArea,
    theme: &Theme,
    area: Rect,
    focused: bool,
) {
    let height = area.height as usize;
    let inner_width = area.width.saturating_sub(2) as usize;
    let visible = textarea.visible_lines(height);
    let (cursor_row, cursor_char_col) = textarea.cursor_pos();
    let scroll = textarea.scroll_offset;

    for (i, row_area) in (0..height).map(|i| {
        (
            i,
            Rect {
                x: area.x,
                y: area.y + i as u16,
                width: area.width,
                height: 1,
            },
        )
    }) {
        let line_idx = scroll + i;
        let line_text = visible.get(i).map(|s| s.as_str()).unwrap_or("");

        if focused && line_idx == cursor_row {
            // Render this line with a cursor block.
            let before: String = line_text.chars().take(cursor_char_col).collect();
            let cursor_char: String = line_text
                .chars()
                .nth(cursor_char_col)
                .map(|c| c.to_string())
                .unwrap_or_else(|| " ".to_string());
            let after: String = line_text.chars().skip(cursor_char_col + 1).collect();

            // Truncate to fit width (simple: no horizontal scroll for now).
            let b: String = before.chars().take(inner_width).collect();
            let remaining = inner_width.saturating_sub(b.chars().count() + 1);
            let a: String = after.chars().take(remaining).collect();

            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(b, theme.style_text()),
                Span::styled(
                    cursor_char,
                    ratatui_core::style::Style::default()
                        .fg(theme.tab_highlight_fg)
                        .bg(theme.tab_highlight_bg),
                ),
                Span::styled(a, theme.style_text()),
            ]);
            Paragraph::new(line).render(row_area, buf);
        } else if focused {
            let display: String = line_text.chars().take(inner_width).collect();
            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(display, theme.style_text()),
            ]);
            Paragraph::new(line).render(row_area, buf);
        } else {
            let display: String = if i == 0 && line_text.is_empty() && visible.len() <= 1 {
                "(empty)".to_string()
            } else {
                line_text.chars().take(inner_width).collect()
            };
            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(display, theme.style_text_muted()),
            ]);
            Paragraph::new(line).render(row_area, buf);
        }
    }
}

/// Return a centered rect with fixed width and height within the outer rect.
fn centered_rect_fixed(width: u16, height: u16, outer: Rect) -> Rect {
    let w = width.min(outer.width);
    let h = height.min(outer.height);
    let x = outer.x + (outer.width.saturating_sub(w)) / 2;
    let y = outer.y + (outer.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_text;

    /// Every output line must fit within max_width (measured in display columns).
    fn assert_all_lines_fit(lines: &[String], max_width: usize) {
        use unicode_width::UnicodeWidthStr;
        for (i, line) in lines.iter().enumerate() {
            let display_width = line.width();
            assert!(
                display_width <= max_width,
                "line {i} is {display_width} cols but max_width is {max_width}: {:?}",
                line,
            );
        }
    }

    #[test]
    fn short_string_no_wrap() {
        let result = wrap_text("hello", 20);
        assert_eq!(result, vec!["hello"]);
        assert_all_lines_fit(&result, 20);
    }

    #[test]
    fn exact_fit_no_wrap() {
        let s = "exactly twenty chars";
        assert_eq!(s.len(), 20);
        let result = wrap_text(s, 20);
        assert_eq!(result.len(), 1);
        assert_all_lines_fit(&result, 20);
    }

    #[test]
    fn wraps_at_word_boundary() {
        let result = wrap_text("  hello world foo bar", 14);
        assert_all_lines_fit(&result, 14);
        assert!(result.len() >= 2, "should wrap: {:?}", result);
    }

    #[test]
    fn wraps_at_slash_boundary() {
        // Simulates a branch like "janiskirsteins/agent-specific-labels"
        let result = wrap_text("  janiskirsteins/agent-specific-labels (walleyboard)", 25);
        assert_all_lines_fit(&result, 25);
        assert!(result.len() >= 2, "should wrap: {:?}", result);
    }

    #[test]
    fn continuation_lines_indented() {
        let result = wrap_text("  long content that must wrap to next line", 20);
        assert_all_lines_fit(&result, 20);
        if result.len() > 1 {
            assert!(
                result[1].starts_with("    "),
                "continuation should be indented: {:?}",
                result[1]
            );
        }
    }

    #[test]
    fn all_content_preserved() {
        let input = "  [no branch] (workbridge) [no wt]";
        let result = wrap_text(input, 20);
        assert_all_lines_fit(&result, 20);
        // All key content words must appear somewhere in the output.
        // Words may be split across lines by the wrapper.
        let flat: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["no", "branch", "workbridge", "wt"] {
            assert!(flat.contains(word), "missing '{word}': {flat}");
        }
    }

    #[test]
    fn very_narrow_width() {
        let result = wrap_text("  hello world", 8);
        assert_all_lines_fit(&result, 8);
        assert!(!result.is_empty());
    }

    #[test]
    fn empty_string() {
        let result = wrap_text("", 20);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn zero_width() {
        let result = wrap_text("hello", 0);
        assert!(result.is_empty());
    }

    #[test]
    fn realistic_workitem_narrow_panel() {
        // 23 chars inner width (100 col terminal, 25% left panel)
        let input = "  [no branch] (workbridge) [no wt]";
        let result = wrap_text(input, 23);
        assert_all_lines_fit(&result, 23);
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("[no wt]"), "must not clip [no wt]");
    }

    #[test]
    fn realistic_branch_narrow_panel() {
        let input = "  janiskirsteins/agent-specific-labels (walleyboard)";
        let result = wrap_text(input, 23);
        assert_all_lines_fit(&result, 23);
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("walleyboard"), "must not clip repo name");
    }

    #[test]
    fn multibyte_utf8_no_panic() {
        // Accented chars (2 bytes each in UTF-8), must not panic on slice
        let input = "  feature/korrektur-andern-loschen (projekt)";
        let result = wrap_text(input, 20);
        assert_all_lines_fit(&result, 20);
        assert!(!result.is_empty());
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("projekt"), "must preserve repo name");
    }

    #[test]
    fn wide_cjk_characters_respect_display_width() {
        use unicode_width::UnicodeWidthStr;
        // CJK ideographs: each is 2 display columns, 1 char, 3 bytes
        // \u{4e16}\u{754c} = 2 chars, 4 display columns
        let input = "  \u{4e16}\u{754c}/test (repo)";
        assert!(
            input.width() > input.chars().count(),
            "CJK should be wider than char count: width={}, chars={}",
            input.width(),
            input.chars().count()
        );
        let result = wrap_text(input, 12);
        assert_all_lines_fit(&result, 12);
        assert!(!result.is_empty());
    }

    #[test]
    fn emoji_display_width() {
        // \u{1f600} = grinning face, 2 display columns, 1 char, 4 bytes
        let input = "  fix/\u{1f600}bug (my-repo)";
        let result = wrap_text(input, 14);
        assert_all_lines_fit(&result, 14);
        assert!(!result.is_empty());
    }
}

#[cfg(test)]
mod wrap_variant_tests {
    use super::{wrap_text_flat, wrap_two_widths};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wrap_text_flat_no_indent_on_continuation() {
        let result = wrap_text_flat("hello world foo bar baz", 12);
        assert_eq!(result[0], "hello world ");
        // Continuation has no leading spaces
        assert!(!result[1].starts_with(' '));
    }

    #[test]
    fn wrap_two_widths_first_line_narrow() {
        // First line budget 10, rest budget 25
        let result = wrap_two_widths(
            "Add Kanban board view with column-based work item organization",
            10,
            25,
        );
        // First line fits within 10 columns
        assert!(
            result[0].width() <= 10,
            "first line too wide: {:?}",
            result[0]
        );
        // Continuation lines use the wider budget
        for line in result.iter().skip(1) {
            assert!(line.width() <= 25, "continuation too wide: {:?}", line);
        }
        // All words present
        let joined: String = result.join(" ");
        assert!(joined.contains("organization"));
    }

    #[test]
    fn wrap_two_widths_fits_first_line() {
        let result = wrap_two_widths("Short title", 20, 40);
        assert_eq!(result, vec!["Short title"]);
    }

    #[test]
    fn wrap_two_widths_empty() {
        let result = wrap_two_widths("", 10, 20);
        assert!(result.is_empty());
    }
}

#[cfg(test)]
mod format_entry_tests {
    use super::format_work_item_entry;
    use crate::app::{App, StubBackend};
    use crate::theme::Theme;
    use crate::work_item::*;
    use std::path::PathBuf;
    use unicode_width::UnicodeWidthStr;

    /// Render a ListItem to a string by putting it in a List widget and
    /// rendering to a buffer.
    fn render_list_item_to_string(
        item: ratatui_widgets::list::ListItem<'_>,
        width: usize,
    ) -> String {
        use ratatui_core::buffer::Buffer;
        use ratatui_core::layout::Rect;
        use ratatui_core::widgets::Widget;
        let height = item.height() as u16;
        let area = Rect::new(0, 0, width as u16, height);
        let list = ratatui_widgets::list::List::new(vec![item]);
        let mut buf = Buffer::empty(area);
        list.render(area, &mut buf);
        let mut lines = Vec::new();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width as u16 {
                if let Some(cell) = buf.cell((x, y)) {
                    line.push_str(cell.symbol());
                }
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    fn make_app_with_work_item(wi: WorkItem) -> App {
        let mut app = App::with_config(crate::config::Config::default(), Box::new(StubBackend));
        app.work_items = vec![wi];
        app
    }

    /// Pre-planning items (no branch, no worktree) should NOT show
    /// [no branch] or [no wt] tags. They just show the repo name.
    #[test]
    fn pre_planning_item_no_tags() {
        let wi = WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            title: "Fix auth bug".to_string(),
            description: None,
            status: WorkItemStatus::Backlog,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: None,
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 40, &theme, false);
        let text = render_list_item_to_string(item, 40);

        assert!(
            !text.contains("[no branch]"),
            "should not show [no branch] tag: {text}"
        );
        assert!(
            !text.contains("[no wt]"),
            "should not show [no wt] tag: {text}"
        );
        // Repo name is now in the group header, not per-item.
        assert!(
            !text.contains("myrepo"),
            "repo should be in group header, not item: {text}"
        );
    }

    /// Work items with a branch should show branch on line 2.
    #[test]
    fn item_with_branch_shows_metadata() {
        let wi = WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            title: "Fix auth bug".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: Some("42-fix-auth".to_string()),
                worktree_path: Some(PathBuf::from("/Projects/myrepo/.worktrees/42-fix-auth")),
                pr: None,
                issue: None,
                git_state: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 40, &theme, false);
        let text = render_list_item_to_string(item, 40);

        assert!(text.contains("42-fix-auth"), "should show branch: {text}");
        // Repo name is now in the group header, not per-item.
        assert!(!text.contains("[no wt]"), "has worktree, no tag: {text}");
    }

    /// Work items with branch but no worktree show [no wt].
    #[test]
    fn item_with_branch_no_worktree_shows_no_wt() {
        let wi = WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            title: "Planned work".to_string(),
            description: None,
            status: WorkItemStatus::Backlog,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: Some("feature-x".to_string()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 40, &theme, false);
        let text = render_list_item_to_string(item, 40);

        assert!(text.contains("feature-x"), "should show branch: {text}");
        assert!(text.contains("[no wt]"), "should show [no wt]: {text}");
    }

    /// Every line in the rendered item must fit within max_width.
    #[test]
    fn all_lines_fit_within_max_width() {
        let wi = WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            title: "A very long title that should be truncated properly".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/walleyboard"),
                branch: Some("janiskirsteins/agent-specific-labels".to_string()),
                worktree_path: Some(PathBuf::from("/Projects/walleyboard")),
                pr: None,
                issue: None,
                git_state: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let max_width = 21; // Narrow panel (100 col terminal)
        let item = format_work_item_entry(&app, 0, max_width, &theme, false);
        let text = render_list_item_to_string(item, max_width);
        for (i, line) in text.lines().enumerate() {
            let line_width = line.width();
            assert!(
                line_width <= max_width,
                "line {i} is {line_width} cols but max is {max_width}: {:?}",
                line,
            );
        }
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::draw_to_buffer;
    use crate::app::{App, FocusPanel, StubBackend, is_selectable};
    use crate::theme::Theme;
    use crate::work_item::{
        BackendType, CheckStatus, PrInfo, PrState, RepoAssociation, ReviewDecision, UnlinkedPr,
        WorkItem, WorkItemError, WorkItemId, WorkItemStatus,
    };
    use crate::work_item_backend::{BackendError, CreateWorkItem, WorkItemBackend, WorkItemRecord};
    use ratatui_core::{backend::TestBackend, terminal::Terminal};
    use std::path::PathBuf;

    /// Helper: render the app into a TestBackend and return the buffer as a string.
    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default_theme();
        terminal
            .draw(|frame: &mut ratatui_core::terminal::Frame<'_>| {
                draw_to_buffer(frame.area(), frame.buffer_mut(), app, &theme)
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut lines = Vec::new();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// A mock backend that returns predefined records for testing the display.
    struct MockBackend {
        records: Vec<WorkItemRecord>,
    }

    impl WorkItemBackend for MockBackend {
        fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
            Ok(crate::work_item_backend::ListResult {
                records: self.records.clone(),
                corrupt: Vec::new(),
            })
        }
        fn create(&self, _req: CreateWorkItem) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not implemented".into()))
        }
        fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_status(
            &self,
            _id: &WorkItemId,
            _status: WorkItemStatus,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn import(&self, _unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not implemented".into()))
        }
        fn append_activity(
            &self,
            _id: &WorkItemId,
            _entry: &crate::work_item_backend::ActivityEntry,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_activity(
            &self,
            _id: &WorkItemId,
        ) -> Result<Vec<crate::work_item_backend::ActivityEntry>, BackendError> {
            Ok(Vec::new())
        }
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(None)
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> BackendType {
            BackendType::LocalFile
        }
    }

    /// Create an App with predefined work items and unlinked PRs
    /// without going through the backend.
    fn app_with_items(work_items: Vec<WorkItem>, unlinked_prs: Vec<UnlinkedPr>) -> App {
        let mut app = App::new();
        app.work_items = work_items;
        app.unlinked_prs = unlinked_prs;
        app.build_display_list();
        app
    }

    fn make_work_item(
        id_suffix: &str,
        title: &str,
        status: WorkItemStatus,
        pr: Option<PrInfo>,
        repo_count: usize,
    ) -> WorkItem {
        let mut associations = Vec::new();
        for i in 0..repo_count.max(1) {
            associations.push(RepoAssociation {
                repo_path: PathBuf::from(format!("/repo/{id_suffix}/{i}")),
                branch: Some(format!("branch-{id_suffix}")),
                worktree_path: None,
                pr: if i == 0 { pr.clone() } else { None },
                issue: None,
                git_state: None,
            });
        }
        WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from(format!("/data/{id_suffix}.json"))),
            backend_type: BackendType::LocalFile,
            title: title.to_string(),
            description: None,
            status,
            status_derived: false,
            repo_associations: associations,
            errors: Vec::new(),
        }
    }

    fn make_pr_info(number: u64, checks: CheckStatus) -> PrInfo {
        PrInfo {
            number,
            title: format!("PR #{number}"),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks,
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    fn make_unlinked_pr(branch: &str, number: u64, is_draft: bool) -> UnlinkedPr {
        UnlinkedPr {
            repo_path: PathBuf::from("/repo/unlinked"),
            pr: PrInfo {
                number,
                title: format!("Unlinked PR #{number}"),
                state: PrState::Open,
                is_draft,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: format!("https://github.com/o/r/pull/{number}"),
            },
            branch: branch.to_string(),
        }
    }

    #[test]
    fn empty_app_default_view() {
        let app = App::new();
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn empty_app_with_status_message() {
        let mut app = App::new();
        app.status_message = Some("Press Ctrl+N to create a work item".to_string());
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn panel_title_shows_item_count() {
        let items = vec![
            make_work_item("a", "First item", WorkItemStatus::Backlog, None, 1),
            make_work_item("b", "Second item", WorkItemStatus::Implementing, None, 1),
            make_work_item("c", "Third item", WorkItemStatus::Backlog, None, 1),
        ];
        let app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_selected_no_session() {
        let items = vec![make_work_item(
            "todo-1",
            "Fix authentication bug",
            WorkItemStatus::Backlog,
            Some(make_pr_info(14, CheckStatus::Passing)),
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        // Select the first selectable work item entry.
        app.selected_item = app.display_list.iter().position(|e| is_selectable(e));
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn unlinked_pr_selected() {
        let items = vec![make_work_item(
            "prog-1",
            "Active feature",
            WorkItemStatus::Implementing,
            Some(make_pr_info(30, CheckStatus::Passing)),
            1,
        )];
        let unlinked = vec![make_unlinked_pr("fix-typo", 45, false)];
        let mut app = app_with_items(items, unlinked);
        // Select the unlinked item (index 1, since index 0 is UNLINKED header).
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn unlinked_pr_selected_badge_has_highlight_style() {
        let items = vec![make_work_item(
            "prog-1",
            "Active feature",
            WorkItemStatus::Implementing,
            Some(make_pr_info(30, CheckStatus::Passing)),
            1,
        )];
        let unlinked = vec![make_unlinked_pr("fix-typo", 45, false)];
        let mut app = app_with_items(items, unlinked);
        app.selected_item = Some(1); // select the unlinked item

        let width: u16 = 80;
        let height: u16 = 24;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default_theme();
        terminal
            .draw(|frame| draw_to_buffer(frame.area(), frame.buffer_mut(), &app, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // The selected row is row 2 (row 0 = border, row 1 = UNLINKED header, row 2 = item).
        let selected_row: u16 = 2;
        let hl = theme.style_tab_highlight();
        let mut found_badge = false;
        for x in 0..width {
            let cell = buf.cell((x, selected_row)).unwrap();
            if cell.symbol() == "P" {
                let next: String = (x..x + 5)
                    .filter_map(|cx| buf.cell((cx, selected_row)).map(|c| c.symbol().to_string()))
                    .collect();
                if next == "PR#45" {
                    assert_eq!(
                        cell.style().fg,
                        hl.fg,
                        "PR badge fg on selected unlinked item should match highlight style, \
                         got {:?} (expected {:?}). Green-on-Cyan is invisible.",
                        cell.style().fg,
                        hl.fg,
                    );
                    found_badge = true;
                    break;
                }
            }
        }
        assert!(found_badge, "PR#45 badge not found on the selected row");
    }

    #[test]
    fn right_panel_focused_with_session() {
        // We cannot easily create a real session in tests, so we test the
        // "no session" case and the welcome message case instead.
        // The focused border styling is tested here via focus state.
        let items = vec![make_work_item(
            "todo-1",
            "Fix authentication bug",
            WorkItemStatus::Backlog,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(|e| is_selectable(e));
        app.focus = FocusPanel::Right;
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_with_context_bar() {
        use crate::work_item::IssueInfo;
        use crate::work_item::IssueState;
        let mut wi = make_work_item("ctx-1", "Fix resize bug", WorkItemStatus::Backlog, None, 1);
        // Add issue with labels to trigger the context bar.
        wi.repo_associations[0].issue = Some(IssueInfo {
            number: 42,
            title: "Fix resize bug".into(),
            state: IssueState::Open,
            labels: vec!["bug".into(), "P1".into()],
        });
        let mut app = app_with_items(vec![wi], vec![]);
        // Select the first selectable work item entry.
        app.selected_item = app.display_list.iter().position(|e| is_selectable(e));
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_context_bar_no_labels() {
        let items = vec![make_work_item(
            "ctx-2",
            "Add authentication",
            WorkItemStatus::Backlog,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(|e| is_selectable(e));
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_context_bar_with_status() {
        use crate::work_item::IssueInfo;
        use crate::work_item::IssueState;
        let mut wi = make_work_item("ctx-3", "Fix resize bug", WorkItemStatus::Backlog, None, 1);
        wi.repo_associations[0].issue = Some(IssueInfo {
            number: 42,
            title: "Fix resize bug".into(),
            state: IssueState::Open,
            labels: vec!["bug".into()],
        });
        let mut app = app_with_items(vec![wi], vec![]);
        app.selected_item = app.display_list.iter().position(|e| is_selectable(e));
        app.status_message = Some("Right panel focused - press Ctrl+] to return".into());
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn settings_overlay_with_config() {
        use crate::config::Config;

        // Use real temp dirs so Config::all_repos() can discover them.
        let base = std::env::temp_dir().join("workbridge-test-settings-overlay");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("discovered-a/.git")).unwrap();
        std::fs::create_dir_all(base.join("discovered-b/.git")).unwrap();

        let base_str = base.display().to_string();
        let discovered_a = base.join("discovered-a").display().to_string();

        let config = Config {
            base_dirs: vec![base_str],
            repos: vec!["~/Forks/special-repo".into()],
            included_repos: vec![discovered_a],
            ..Config::for_test()
        };
        let mut app = App::with_config(config, Box::new(StubBackend));
        app.show_settings = true;
        let output = render(&app, 80, 24);

        let _ = std::fs::remove_dir_all(&base);

        insta::assert_snapshot!(output);
    }

    // -- Work item display tests --

    #[test]
    fn work_item_list_grouped() {
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Backlog,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
            make_work_item(
                "todo-2",
                "Add user settings page",
                WorkItemStatus::Backlog,
                None,
                1,
            ),
            make_work_item(
                "prog-1",
                "Refactor backend API",
                WorkItemStatus::Implementing,
                Some(make_pr_info(88, CheckStatus::Failing)),
                2,
            ),
            make_work_item(
                "prog-2",
                "Update dependencies",
                WorkItemStatus::Implementing,
                Some(make_pr_info(12, CheckStatus::Pending)),
                1,
            ),
        ];
        let app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_list_with_unlinked() {
        let items = vec![make_work_item(
            "prog-1",
            "Active feature",
            WorkItemStatus::Implementing,
            Some(make_pr_info(30, CheckStatus::Passing)),
            1,
        )];
        let unlinked = vec![
            make_unlinked_pr("fix-typo", 45, false),
            make_unlinked_pr("update-deps", 12, true),
        ];
        let app = app_with_items(items, unlinked);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_list_empty_groups() {
        let app = app_with_items(vec![], vec![]);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_list_with_done_group() {
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Backlog,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
            make_work_item(
                "prog-1",
                "Refactor backend API",
                WorkItemStatus::Implementing,
                Some(make_pr_info(88, CheckStatus::Failing)),
                1,
            ),
            make_work_item(
                "done-1",
                "Update dependencies",
                WorkItemStatus::Done,
                Some(PrInfo {
                    number: 50,
                    title: "Update deps".to_string(),
                    state: PrState::Merged,
                    is_draft: false,
                    review_decision: ReviewDecision::None,
                    checks: CheckStatus::Passing,
                    url: "https://github.com/o/r/pull/50".to_string(),
                }),
                1,
            ),
        ];
        let app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_with_errors_no_session() {
        let items = vec![WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/data/err.json")),
            backend_type: BackendType::LocalFile,
            title: "Broken work item".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/repo/alpha"),
                branch: Some("42-fix-bug".to_string()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![
                WorkItemError::MultiplePrsForBranch {
                    repo_path: PathBuf::from("/repo/alpha"),
                    branch: "42-fix-bug".to_string(),
                    count: 2,
                },
                WorkItemError::IssueNotFound {
                    repo_path: PathBuf::from("/repo/alpha"),
                    issue_number: 42,
                },
            ],
        }];
        let mut app = app_with_items(items, vec![]);
        // Select the first selectable work item entry (skipping group headers).
        app.selected_item = app.display_list.iter().position(|e| is_selectable(e));
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn create_dialog_default_view() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![
            PathBuf::from("/Volumes/X10/Projects/workbridge"),
            PathBuf::from("/Volumes/X10/Projects/other-repo"),
        ];
        app.create_dialog.open(
            &repos,
            Some(&PathBuf::from("/Volumes/X10/Projects/workbridge")),
        );
        assert!(app.create_dialog.visible);
        assert_eq!(app.create_dialog.focus_field, CreateDialogFocus::Title);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn create_dialog_with_input_and_repos_focused() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![
            PathBuf::from("/repo/alpha"),
            PathBuf::from("/repo/beta"),
            PathBuf::from("/repo/gamma"),
        ];
        app.create_dialog
            .open(&repos, Some(&PathBuf::from("/repo/beta")));
        // Type a title
        app.create_dialog.title_input.set_text("My feature");
        // Focus on repos
        app.create_dialog.focus_field = CreateDialogFocus::Repos;
        app.create_dialog.repo_cursor = 1; // beta is selected
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn create_dialog_with_error() {
        let mut app = App::new();
        let repos = vec![PathBuf::from("/repo/only")];
        app.create_dialog.open(&repos, None);
        app.create_dialog.error_message = Some("Title cannot be empty".to_string());
        insta::assert_snapshot!(render(&app, 80, 24));
    }
}
