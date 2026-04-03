use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, DisplayEntry, FocusPanel, SettingsListFocus, WorkItemContext};
use crate::config;
use crate::layout;
use crate::theme::Theme;
use crate::work_item::{CheckStatus, PrState, WorkItemError};

/// Render the entire UI: left panel (work item list) and right panel
/// (session output), plus optional context bar and status bar at the bottom.
pub fn draw(frame: &mut Frame, app: &App) {
    let theme = Theme::default_theme();
    let area = frame.area();

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
        .constraints([
            Constraint::Length(pl.left_width),
            Constraint::Min(0),
        ])
        .split(main_area);

    draw_work_item_list(frame, app, &theme, chunks[0]);
    draw_pane_output(frame, app, &theme, chunks[1]);

    // Context bar (persistent work-item info).
    if let Some(area) = context_area
        && let Some(ctx) = app.selected_work_item_context()
    {
        draw_context_bar(frame, &ctx, &theme, area);
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
        frame.render_widget(status, area);
    }

    // Settings overlay (rendered on top of everything).
    if app.show_settings {
        draw_settings_overlay(frame, app, &theme, area);
    }
}

/// Draw the left panel containing the grouped work item list.
fn draw_work_item_list(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
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
        .title(" Work Items ")
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
        frame.render_widget(paragraph, area);
        return;
    }

    // Available width inside the block borders.
    let inner_width = area.width.saturating_sub(2) as usize;

    let items: Vec<ListItem> = app
        .display_list
        .iter()
        .map(|entry| match entry {
            DisplayEntry::GroupHeader { label, count } => {
                let text = format!("{label} ({count})");
                ListItem::new(Line::from(vec![Span::styled(
                    text,
                    theme.style_group_header(),
                )]))
            }
            DisplayEntry::EmptyState(msg) => {
                ListItem::new(Line::from(vec![Span::styled(
                    msg.clone(),
                    theme.style_text_muted(),
                )]))
            }
            DisplayEntry::UnlinkedItem(idx) => {
                format_unlinked_item(app, *idx, inner_width, theme)
            }
            DisplayEntry::WorkItemEntry(idx) => {
                format_work_item_entry(app, *idx, inner_width, theme)
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(theme.style_tab_highlight())
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(app.selected_item);

    frame.render_stateful_widget(list, area, &mut state);
}

/// Format an unlinked PR entry for the left panel list.
fn format_unlinked_item<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
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
        .saturating_sub(prefix.len())
        .saturating_sub(right.len())
        .saturating_sub(1);
    let truncated_title = truncate_str(title, available);

    let padding =
        max_width.saturating_sub(prefix.len() + truncated_title.len() + right.len());
    let pad_str: String = " ".repeat(padding);

    ListItem::new(Line::from(vec![
        Span::styled(prefix.to_string(), theme.style_unlinked_marker()),
        Span::styled(truncated_title, theme.style_text()),
        Span::raw(pad_str),
        Span::styled(right, theme.style_badge_pr()),
    ]))
}

/// Format a work item entry for the left panel list.
fn format_work_item_entry<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
) -> ListItem<'a> {
    let Some(wi) = app.work_items.get(idx) else {
        return ListItem::new(Line::from("  <invalid>"));
    };

    // Build the right-side badge string.
    let mut right_parts: Vec<(String, ratatui::style::Style)> = Vec::new();

    // PR badge: show first PR if any.
    let first_pr = wi
        .repo_associations
        .iter()
        .find_map(|a| a.pr.as_ref());
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

    let right_text: String = right_parts.iter().map(|(s, _)| s.as_str()).collect();

    // Title.
    let prefix = "  ";
    let available = max_width
        .saturating_sub(prefix.len())
        .saturating_sub(right_text.len())
        .saturating_sub(1);
    let truncated_title = truncate_str(&wi.title, available);

    let padding =
        max_width.saturating_sub(prefix.len() + truncated_title.len() + right_text.len());
    let pad_str: String = " ".repeat(padding);

    let mut spans = vec![
        Span::raw(prefix.to_string()),
        Span::styled(truncated_title, theme.style_text()),
        Span::raw(pad_str),
    ];
    for (text, style) in right_parts {
        spans.push(Span::styled(text, style));
    }

    ListItem::new(Line::from(spans))
}

/// Truncate a string to fit within max_len characters.
/// If truncated, appends "..".
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else if max_len <= 2 {
        s.chars().take(max_len).collect()
    } else {
        let mut result: String = s.chars().take(max_len - 2).collect();
        result.push_str("..");
        result
    }
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
            format!(
                "Issue #{issue_number} not found in {}",
                repo_path.display()
            ),
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

/// Draw the right panel showing captured PTY output.
/// Uses vt100::Parser + tui-term PseudoTerminal for full ANSI color rendering.
///
/// The active session is determined by the currently selected work item:
/// - If selected item is a work item with a session -> render PseudoTerminal
/// - If selected item is a work item without a session -> prompt to start
/// - If selected item is an unlinked PR -> prompt to import
/// - If nothing selected -> show welcome message
fn draw_pane_output(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
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
    let selected_entry = app
        .selected_item
        .and_then(|idx| app.display_list.get(idx));

    match selected_entry {
        Some(DisplayEntry::WorkItemEntry(wi_idx)) => {
            let work_item_id = app.work_items.get(*wi_idx).map(|wi| &wi.id);
            let session_entry = work_item_id.and_then(|id| app.sessions.get(id));

            match session_entry {
                Some(entry) if !entry.alive => {
                    let text = Text::from(vec![
                        Line::from(""),
                        Line::from("  Session has ended."),
                        Line::from(""),
                        Line::from("  Press Enter to start"),
                        Line::from("  a new session."),
                    ]);
                    let paragraph = Paragraph::new(text)
                        .block(block)
                        .style(theme.style_error());
                    frame.render_widget(paragraph, area);
                }
                Some(entry) => {
                    // Lock the shared parser to get the current screen state.
                    if let Ok(parser) = entry.parser.lock() {
                        let pseudo_term = PseudoTerminal::new(parser.screen())
                            .block(block);
                        frame.render_widget(pseudo_term, area);
                    } else {
                        // Parser lock poisoned - show a fallback message.
                        let text = Text::from(vec![
                            Line::from(""),
                            Line::from("  [render error]"),
                        ]);
                        let paragraph = Paragraph::new(text)
                            .block(block)
                            .style(theme.style_error());
                        frame.render_widget(paragraph, area);
                    }
                }
                None => {
                    let wi = app.work_items.get(*wi_idx);
                    let errors = wi.map(|w| &w.errors);
                    let has_errors = errors.is_some_and(|e| !e.is_empty());

                    if has_errors {
                        let mut lines = vec![
                            Line::from(""),
                            Line::from(Span::styled(
                                "  Errors:",
                                theme.style_error(),
                            )),
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
                        lines.push(Line::from(Span::styled(
                            "  Press Enter to start a session.",
                            theme.style_text_muted(),
                        )));
                        let text = Text::from(lines);
                        let paragraph = Paragraph::new(text).block(block);
                        frame.render_widget(paragraph, area);
                    } else {
                        let text = Text::from(vec![
                            Line::from(""),
                            Line::from("  Press Enter to start"),
                            Line::from("  a session for this"),
                            Line::from("  work item."),
                        ]);
                        let paragraph = Paragraph::new(text)
                            .block(block)
                            .style(theme.style_text_muted());
                        frame.render_widget(paragraph, area);
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
            frame.render_widget(paragraph, area);
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
            frame.render_widget(paragraph, area);
        }
    }
}

/// Draw the work-item context bar showing title, repo path, and labels.
fn draw_context_bar(frame: &mut Frame, ctx: &WorkItemContext, theme: &Theme, area: Rect) {
    let labels_part = if ctx.labels.is_empty() {
        String::new()
    } else {
        format!(" | {}", ctx.labels.join(", "))
    };

    let full = format!("{} | {}{}", ctx.title, ctx.repo_path, labels_part);

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
    frame.render_widget(paragraph, area);
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
fn draw_settings_overlay(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let popup = centered_rect(70, 80, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Settings (press ? or Esc to close) ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());

    let block_inner = block.inner(popup);
    frame.render_widget(block, popup);

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
            ListItem::new(format!(" {marker} {} ({source_label})", entry.path.display()))
                .style(theme.style_text()),
        );
    }

    // Build available repo items (discovered but not managed).
    let available_entries = app.available_repos();
    let mut available_items: Vec<ListItem<'_>> = Vec::new();
    for entry in &available_entries {
        let marker = if entry.git_dir_present { "+" } else { "-" };
        available_items.push(
            ListItem::new(format!(" {marker} {}", entry.path.display()))
                .style(theme.style_text()),
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
    frame.render_widget(Paragraph::new(source_text), sections[0]);

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
    frame.render_widget(Paragraph::new(Text::from(base_lines)), sections[1]);

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
        let empty = Paragraph::new(Line::styled("  (none)", theme.style_text_muted()))
            .block(managed_block);
        frame.render_widget(empty, repo_cols[0]);
    } else {
        let list = List::new(managed_items)
            .block(managed_block)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if app.settings_list_focus == SettingsListFocus::Managed {
            state.select(Some(app.settings_repo_selected.min(managed_count.saturating_sub(1))));
        }
        frame.render_stateful_widget(list, repo_cols[0], &mut state);
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
        frame.render_widget(empty, repo_cols[1]);
    } else {
        let list = List::new(available_items)
            .block(available_block)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if app.settings_list_focus == SettingsListFocus::Available {
            state.select(Some(
                app.settings_available_selected.min(available_count.saturating_sub(1)),
            ));
        }
        frame.render_stateful_widget(list, repo_cols[1], &mut state);
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
    frame.render_widget(Paragraph::new(defaults_text), sections[4]);

    // Section 5: Hint line.
    let hint = Line::styled(
        "Tab: switch list, Enter: move, Up/Down: navigate",
        theme.style_text_muted(),
    );
    frame.render_widget(Paragraph::new(hint), sections[5]);
}

#[cfg(test)]
mod snapshot_tests {
    use std::path::PathBuf;
    use ratatui::{Terminal, backend::TestBackend};
    use crate::app::{App, FocusPanel, StubBackend};
    use crate::work_item::{
        BackendType, CheckStatus, PrInfo, PrState, RepoAssociation, ReviewDecision,
        UnlinkedPr, WorkItem, WorkItemError, WorkItemId, WorkItemStatus,
    };
    use crate::work_item_backend::{BackendError, WorkItemBackend, WorkItemRecord,
        CreateWorkItem};
    use super::draw;

    /// Helper: render the app into a TestBackend and return the buffer as a string.
    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, app))
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
        fn import(&self, _unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not implemented".into()))
        }
        fn backend_type(&self) -> BackendType {
            BackendType::LocalFile
        }
    }

    /// Create an App with predefined work items and unlinked PRs
    /// without going through the backend.
    fn app_with_items(
        work_items: Vec<WorkItem>,
        unlinked_prs: Vec<UnlinkedPr>,
    ) -> App {
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
            status,
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
    fn work_item_selected_no_session() {
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Todo,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        // Select the first work item entry (index 1, since index 0 is the
        // TODO group header).
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn unlinked_pr_selected() {
        let items = vec![
            make_work_item(
                "prog-1",
                "Active feature",
                WorkItemStatus::InProgress,
                Some(make_pr_info(30, CheckStatus::Passing)),
                1,
            ),
        ];
        let unlinked = vec![
            make_unlinked_pr("fix-typo", 45, false),
        ];
        let mut app = app_with_items(items, unlinked);
        // Select the unlinked item (index 1, since index 0 is UNLINKED header).
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn right_panel_focused_with_session() {
        // We cannot easily create a real session in tests, so we test the
        // "no session" case and the welcome message case instead.
        // The focused border styling is tested here via focus state.
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Todo,
                None,
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = Some(1);
        app.focus = FocusPanel::Right;
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_with_context_bar() {
        use crate::work_item::IssueInfo;
        use crate::work_item::IssueState;
        let mut wi = make_work_item(
            "ctx-1",
            "Fix resize bug",
            WorkItemStatus::Todo,
            None,
            1,
        );
        // Add issue with labels to trigger the context bar.
        wi.repo_associations[0].issue = Some(IssueInfo {
            number: 42,
            title: "Fix resize bug".into(),
            state: IssueState::Open,
            labels: vec!["bug".into(), "P1".into()],
        });
        let mut app = app_with_items(vec![wi], vec![]);
        // Select the work item entry (index 1, after TODO header).
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_context_bar_no_labels() {
        let items = vec![make_work_item(
            "ctx-2",
            "Add authentication",
            WorkItemStatus::Todo,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_context_bar_with_status() {
        use crate::work_item::IssueInfo;
        use crate::work_item::IssueState;
        let mut wi = make_work_item(
            "ctx-3",
            "Fix resize bug",
            WorkItemStatus::Todo,
            None,
            1,
        );
        wi.repo_associations[0].issue = Some(IssueInfo {
            number: 42,
            title: "Fix resize bug".into(),
            state: IssueState::Open,
            labels: vec!["bug".into()],
        });
        let mut app = app_with_items(vec![wi], vec![]);
        app.selected_item = Some(1);
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
                WorkItemStatus::Todo,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
            make_work_item(
                "todo-2",
                "Add user settings page",
                WorkItemStatus::Todo,
                None,
                1,
            ),
            make_work_item(
                "prog-1",
                "Refactor backend API",
                WorkItemStatus::InProgress,
                Some(make_pr_info(88, CheckStatus::Failing)),
                2,
            ),
            make_work_item(
                "prog-2",
                "Update dependencies",
                WorkItemStatus::InProgress,
                Some(make_pr_info(12, CheckStatus::Pending)),
                1,
            ),
        ];
        let app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn work_item_list_with_unlinked() {
        let items = vec![
            make_work_item(
                "prog-1",
                "Active feature",
                WorkItemStatus::InProgress,
                Some(make_pr_info(30, CheckStatus::Passing)),
                1,
            ),
        ];
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
    fn work_item_with_errors_no_session() {
        let items = vec![WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/data/err.json")),
            backend_type: BackendType::LocalFile,
            title: "Broken work item".to_string(),
            status: WorkItemStatus::InProgress,
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
        // Select the work item entry (index 2: header at 0, empty-todo at 1,
        // IN PROGRESS header at 2, work item at 3).
        // Actually: TODO(0) header at 0, empty at 1, IN PROGRESS(1) header
        // at 2, work item at 3.
        app.selected_item = Some(3);
        insta::assert_snapshot!(render(&app, 80, 24));
    }
}
