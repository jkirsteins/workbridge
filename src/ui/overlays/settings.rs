//! Settings overlay and its per-tab rendering helpers.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::text::{Line, Span, Text};
use ratatui_core::widgets::{StatefulWidget, Widget};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::clear::Clear;
use ratatui_widgets::paragraph::Paragraph;

use crate::app::{App, SettingsListFocus, SettingsTab};
use crate::config;
use crate::theme::Theme;

use rat_widget::text_input::TextInput;
use ratatui_widgets::list::{List, ListItem, ListState};
use ratatui_widgets::tabs::Tabs;

use super::super::common::{centered_rect, dim_background};
use super::super::modals::create_dialog::create_dialog_text_style;

const REPOS_LIST_MAX_ROWS: u16 = 6;

/// Draw the settings overlay: a centered popup with structured sections.
///
/// Layout (top to bottom):
///   - Config source (2 lines)
///   - Base directories (header + entries)
///   - Repos section: horizontal split of Active and Excluded lists
///   - Defaults (2 lines)
pub fn draw_settings_overlay(buf: &mut Buffer, app: &mut App, theme: &Theme, area: Rect) {
    // Dim the background so the overlay is the clear focal point.
    dim_background(buf, area);

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

    // Top-level layout: tab bar (1 row) + body (rest).
    let [tab_bar_area, body_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .areas(inner);

    // Tab bar.
    let tab_selected = match app.settings_tab {
        SettingsTab::Repos => 0,
        SettingsTab::ReviewGate => 1,
        SettingsTab::Keybindings => 2,
    };
    let tabs = Tabs::new(vec![" Repos ", " Review Gate ", " Keybindings "])
        .select(tab_selected)
        .style(theme.style_text_muted())
        .highlight_style(theme.style_view_mode_tab_active())
        .divider("|");
    tabs.render(tab_bar_area, buf);

    // Body layout: content (fills) + hint line (1 row).
    let [content_area, hint_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .areas(body_area);

    match app.settings_tab {
        SettingsTab::Keybindings => {
            draw_settings_keybindings_tab(buf, app, theme, content_area);
            let hint = Line::styled(
                "Tab: switch tab   Up/Down: scroll   ?: close",
                theme.style_text_muted(),
            );
            Paragraph::new(hint).render(hint_area, buf);
        }
        SettingsTab::ReviewGate => {
            draw_settings_review_gate_tab(buf, app, theme, content_area);
            let hint = if app.settings_review_skill_editing {
                Line::styled("Enter: save   Esc: cancel", theme.style_text_muted())
            } else {
                Line::styled(
                    "Tab: switch tab   Enter: edit   ?: close",
                    theme.style_text_muted(),
                )
            };
            Paragraph::new(hint).render(hint_area, buf);
        }
        SettingsTab::Repos => {
            draw_settings_repos_tab(buf, app, theme, content_area);
            let hint = Line::styled(
                "Tab: switch tab   Left/Right: switch column   Enter: move repo   Up/Down: navigate",
                theme.style_text_muted(),
            );
            Paragraph::new(hint).render(hint_area, buf);
        }
    }
}

pub fn draw_settings_repos_tab(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
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

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(source_height),
            Constraint::Length(base_dirs_height),
            Constraint::Length(repos_section_height),
            Constraint::Length(1), // blank
            Constraint::Length(defaults_height),
            Constraint::Min(0), // absorb remaining space
        ])
        .split(area);

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

    // Section 2: Repos - horizontal split of Managed and Available lists.
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
    let managed_title = format!(" Managed repos ({managed_count}) ");
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
    let available_title = format!(" Available ({available_count}) ");
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
}

pub fn draw_settings_review_gate_tab(buf: &mut Buffer, app: &mut App, theme: &Theme, area: Rect) {
    // Layout: heading (1) + blank (1) + label (1) + input (1) + blank (1) + description.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // heading
            Constraint::Length(1), // blank
            Constraint::Length(1), // label
            Constraint::Length(1), // value / input field
            Constraint::Length(1), // blank
            Constraint::Min(0),    // description
        ])
        .split(area);

    let heading = Line::styled("Review Gate Skill", theme.style_heading());
    Paragraph::new(heading).render(rows[0], buf);

    let label = Line::styled("Skill (slash command):", theme.style_text());
    Paragraph::new(label).render(rows[2], buf);

    if app.settings_review_skill_editing {
        // Render with rat-widget's TextInput so the caret is drawn by the
        // same stateful widget used by the Create Work Item dialog.
        app.settings_review_skill_input.focus.set(true);
        StatefulWidget::render(
            TextInput::new().styles(create_dialog_text_style(theme)),
            rows[3],
            buf,
            &mut app.settings_review_skill_input,
        );
    } else {
        // Show the current value; mirror the unfocused single-line style.
        let value = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                app.config.defaults.review_skill.as_str(),
                theme.style_text(),
            ),
        ]);
        Paragraph::new(value).render(rows[3], buf);
    }

    let desc = Text::from(vec![
        Line::styled(
            "The initial prompt sent to the coding agent when the review gate runs.",
            theme.style_text_muted(),
        ),
        Line::styled(
            "Can be a slash command (e.g. /claude-adversarial-review for Claude Code)",
            theme.style_text_muted(),
        ),
        Line::styled(
            "or plain-text guidance that any coding agent can follow.",
            theme.style_text_muted(),
        ),
        Line::from(""),
        Line::styled(
            "Default: /claude-adversarial-review",
            theme.style_text_muted(),
        ),
    ]);
    Paragraph::new(desc).render(rows[5], buf);
}

pub fn draw_settings_keybindings_tab(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let h = theme.style_heading();
    let k = theme.style_text(); // key name style
    let d = theme.style_text_muted(); // description style

    let binding = |key: &'static str, desc: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {key:<26}"), k),
            Span::styled(desc, d),
        ])
    };

    let lines: Vec<Line<'_>> = vec![
        Line::styled("Global", h),
        binding("Ctrl+N", "Quick-start session"),
        binding("Ctrl+B", "New backlog ticket"),
        binding("Ctrl+G", "Global assistant"),
        binding("Ctrl+R", "Refresh GitHub data"),
        binding("Ctrl+\\", "Cycle Session <-> Terminal tab"),
        binding("?", "Settings / keybindings (this overlay)"),
        binding("Q / Ctrl+Q", "Quit"),
        Line::from(""),
        Line::styled("List focused", h),
        binding("Up / Down", "Navigate items"),
        binding("Enter", "Open session / Import"),
        binding("Shift+Right", "Advance stage"),
        binding("Shift+Left", "Retreat stage"),
        binding("Ctrl+D / Delete", "Delete work item"),
        binding("o", "Open PR in default browser"),
        binding("Ctrl+]", "Focus session panel"),
        Line::from(""),
        Line::styled("Board view", h),
        binding("Left / Right", "Move between columns"),
        binding("Shift+Left / Shift+Right", "Move item to adjacent column"),
        binding("Up / Down", "Navigate within column"),
        binding("Enter", "Open drill-down / session"),
        Line::from(""),
        Line::styled("Session active (right panel)", h),
        binding("Ctrl+]", "Return to item list"),
        Line::from(vec![Span::styled(
            "  (all other keys forwarded to the session)",
            d,
        )]),
        Line::from(""),
        Line::styled("Creation dialog  (Ctrl+B)", h),
        binding("Tab / Shift+Tab", "Cycle fields"),
        binding("Enter", "Create  (newline in description)"),
        binding("Space", "Toggle repo selection"),
        binding("Esc", "Cancel"),
        Line::from(""),
        Line::styled("Settings overlay  (?)", h),
        binding("Tab", "Switch tab (Repos / Keybindings)"),
        binding("Left / Right", "Switch column focus  (Repos tab)"),
        binding("Up / Down", "Navigate / scroll"),
        binding("Enter", "Move repo in or out of managed"),
        binding("? / Esc", "Close"),
    ];

    Paragraph::new(Text::from(lines))
        .scroll((app.settings_keybindings_scroll, 0))
        .render(area, buf);
}
