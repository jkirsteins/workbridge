use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, FocusPanel};
use crate::layout;

/// Render the entire UI: left panel (tab list) and right panel (session output),
/// plus an optional status bar at the bottom.
pub fn draw(frame: &mut Frame, app: &App) {
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

    draw_tab_list(frame, app, chunks[0]);
    draw_pane_output(frame, app, chunks[1]);

    // Status bar.
    if has_status
        && let Some(msg) = &app.status_message
    {
        let style = if app.shutting_down {
            Style::default().fg(Color::White).bg(Color::Red)
        } else {
            Style::default().fg(Color::Yellow).bg(Color::DarkGray)
        };
        let status = Paragraph::new(msg.as_str()).style(style);
        frame.render_widget(status, vertical[1]);
    }
}

/// Draw the left panel containing the tab list.
fn draw_tab_list(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let border_color = if app.focus == FocusPanel::Left {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .title(" Tabs ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

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
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem> = app
        .tabs
        .iter()
        .map(|tab| {
            if !tab.alive {
                let label = format!(" {} [dead] ", tab.name);
                let style = Style::default().fg(Color::Red);
                ListItem::new(label).style(style)
            } else {
                let label = format!(" {} ", tab.name);
                ListItem::new(label).style(Style::default())
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut state = ListState::default();
    state.select(app.selected_tab);

    frame.render_stateful_widget(list, area, &mut state);
}

/// Draw the right panel showing captured PTY output.
/// Uses vt100::Parser + tui-term PseudoTerminal for full ANSI color rendering.
fn draw_pane_output(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let border_color = if app.focus == FocusPanel::Right {
        Color::Green
    } else {
        Color::White
    };

    let title = if app.focus == FocusPanel::Right {
        " Claude Code [INPUT] "
    } else {
        " Claude Code "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

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
                .style(Style::default().fg(Color::Red));
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
                    .style(Style::default().fg(Color::Red));
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
                Line::from("  Q/Ctrl+Q  - Quit"),
            ]);
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(paragraph, area);
        }
    }
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
        // Collect each row from the buffer, trimming trailing whitespace
        // to keep snapshots readable.
        let buf = terminal.backend().buffer().clone();
        let mut lines = Vec::new();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        // Trim trailing empty lines
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
}
