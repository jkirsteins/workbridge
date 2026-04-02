use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, FocusPanel, SettingsListFocus};
use crate::config;
use crate::layout;
use crate::theme::Theme;

/// Render the entire UI: left panel (tab list) and right panel (session output),
/// plus an optional status bar at the bottom.
pub fn draw(frame: &mut Frame, app: &App) {
    let theme = Theme::default_theme();
    let area = frame.area();

    // Vertical split: main area + optional 1-row status bar.
    let has_status = app.status_message.is_some();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if has_status {
            vec![Constraint::Min(0), Constraint::Length(1)]
        } else {
            vec![Constraint::Min(0)]
        })
        .split(area);

    let main_area = vertical[0];

    // Horizontal split: left panel, right panel.
    let pl = layout::compute(main_area.width, main_area.height, false);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(pl.left_width),
            Constraint::Min(0),
        ])
        .split(main_area);

    draw_tab_list(frame, app, &theme, chunks[0]);
    draw_pane_output(frame, app, &theme, chunks[1]);

    // Status bar.
    if has_status
        && let Some(msg) = &app.status_message
    {
        let style = if app.shutting_down {
            theme.style_status_shutdown()
        } else {
            theme.style_status()
        };
        let status = Paragraph::new(msg.as_str()).style(style);
        frame.render_widget(status, vertical[1]);
    }

    // Settings overlay (rendered on top of everything).
    if app.show_settings {
        draw_settings_overlay(frame, app, &theme, area);
    }
}

/// Draw the left panel containing the tab list.
fn draw_tab_list(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
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
        .title(" Tabs ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(border_style);

    if app.tabs.is_empty() {
        let text = Text::from(vec![
            Line::from(""),
            Line::from("  No tabs."),
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

    let items: Vec<ListItem> = app
        .tabs
        .iter()
        .map(|tab| {
            if !tab.alive {
                let label = format!(" {} [dead] ", tab.name);
                ListItem::new(label).style(theme.style_tab_dead())
            } else {
                let label = format!(" {} ", tab.name);
                ListItem::new(label).style(theme.style_text())
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(theme.style_tab_highlight())
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(app.selected_tab);

    frame.render_stateful_widget(list, area, &mut state);
}

/// Draw the right panel showing captured PTY output.
/// Uses vt100::Parser + tui-term PseudoTerminal for full ANSI color rendering.
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

    let active_tab = app
        .selected_tab
        .and_then(|idx| app.tabs.get(idx));

    match active_tab {
        Some(tab) if !tab.alive => {
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Session has ended."),
                Line::from(""),
                Line::from("  Press Ctrl+D or Delete"),
                Line::from("  to remove this tab."),
            ]);
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(theme.style_error());
            frame.render_widget(paragraph, area);
        }
        Some(tab) => {
            // Lock the shared parser to get the current screen state.
            // The reader thread continuously feeds PTY output to this
            // parser, so no reading happens on the UI thread.
            if let Ok(parser) = tab.parser.lock() {
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
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Welcome to workbridge"),
                Line::from(""),
                Line::from("  Ctrl+N    - Create a new tab"),
                Line::from("  Up/Down   - Navigate tabs"),
                Line::from("  Enter     - Focus right panel"),
                Line::from("  Ctrl+]    - Return to tab list"),
                Line::from("  Ctrl+D    - Delete selected tab"),
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
    use std::sync::{Arc, Mutex};
    use ratatui::{Terminal, backend::TestBackend};
    use crate::app::{App, FocusPanel, Tab};
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

    /// Helper: create a Tab without spawning a real PTY session.
    fn make_tab(name: &str, alive: bool, cols: u16, rows: u16) -> Tab {
        Tab {
            name: name.to_string(),
            parser: Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0))),
            alive,
            session: None,
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
        app.status_message = Some("Press Ctrl+N to create a new tab".to_string());
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn single_tab_selected() {
        let mut app = App::new();
        app.pane_cols = 58;
        app.pane_rows = 22;
        app.tabs.push(make_tab("Tab 0", true, 58, 22));
        app.selected_tab = Some(0);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn multiple_tabs_second_selected() {
        let mut app = App::new();
        app.pane_cols = 58;
        app.pane_rows = 22;
        app.tabs.push(make_tab("Tab 0", true, 58, 22));
        app.tabs.push(make_tab("Tab 1", true, 58, 22));
        app.tabs.push(make_tab("Tab 2", true, 58, 22));
        app.selected_tab = Some(1);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn tab_with_dead_session() {
        let mut app = App::new();
        app.pane_cols = 58;
        app.pane_rows = 22;
        app.tabs.push(make_tab("Tab 0", true, 58, 22));
        app.tabs.push(make_tab("Tab 1", false, 58, 22));
        app.selected_tab = Some(1);
        insta::assert_snapshot!(render(&app, 80, 24));
    }

    #[test]
    fn right_panel_focused() {
        let mut app = App::new();
        app.pane_cols = 58;
        app.pane_rows = 22;
        app.tabs.push(make_tab("Tab 0", true, 58, 22));
        app.selected_tab = Some(0);
        app.focus = FocusPanel::Right;
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
        let mut app = App::with_config(config);
        app.show_settings = true;
        let output = render(&app, 80, 24);

        let _ = std::fs::remove_dir_all(&base);

        insta::assert_snapshot!(output);
    }
}
