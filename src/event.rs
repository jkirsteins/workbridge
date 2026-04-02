use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;

use crate::app::{App, FocusPanel, SettingsListFocus};
use crate::layout;

/// The tick interval for periodic updates (reading PTY output).
const TICK_RATE: Duration = Duration::from_millis(200);

/// Poll for the next event and handle it. Blocks for at most the remaining
/// time until the next tick.
///
/// Returns `Ok(true)` if a tick boundary was crossed during this call (i.e.,
/// `last_tick` was reset). The caller can use this to gate periodic work
/// that should happen at tick frequency rather than on every loop iteration.
///
/// Returns `Err` if the terminal event stream fails, so the caller can exit
/// through the normal teardown path instead of spinning silently.
pub fn poll_and_handle(app: &mut App, last_tick: &mut Instant) -> io::Result<bool> {
    let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());

    if event::poll(timeout)? {
        let ev = event::read()?;
        match ev {
            Event::Key(key) => {
                handle_key(app, key);
            }
            Event::Resize(cols, rows) => {
                handle_resize(app, cols, rows);
            }
            _ => {}
        }
    }

    if last_tick.elapsed() >= TICK_RATE {
        *last_tick = Instant::now();
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Handle a key event by dispatching based on focus panel.
fn handle_key(app: &mut App, key: KeyEvent) {
    // During shutdown, only Q triggers force quit. All other keys are ignored.
    if app.shutting_down {
        if matches!(
            (key.modifiers, key.code),
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
                | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        ) {
            app.force_kill_all();
            app.should_quit = true;
        }
        return;
    }

    // When the settings overlay is open, handle overlay-specific keys.
    if app.show_settings {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?'))
            | (_, KeyCode::Esc) => {
                app.show_settings = false;
                app.settings_repo_selected = 0;
                app.settings_available_selected = 0;
                app.settings_list_focus = SettingsListFocus::Managed;
            }
            (_, KeyCode::Up) => match app.settings_list_focus {
                SettingsListFocus::Managed => {
                    app.settings_repo_selected =
                        app.settings_repo_selected.saturating_sub(1);
                }
                SettingsListFocus::Available => {
                    app.settings_available_selected =
                        app.settings_available_selected.saturating_sub(1);
                }
            },
            (_, KeyCode::Down) => match app.settings_list_focus {
                SettingsListFocus::Managed => {
                    let max = app.total_repos().saturating_sub(1);
                    if app.settings_repo_selected < max {
                        app.settings_repo_selected += 1;
                    }
                }
                SettingsListFocus::Available => {
                    let max = app.available_repos().len().saturating_sub(1);
                    if app.settings_available_selected < max {
                        app.settings_available_selected += 1;
                    }
                }
            },
            (_, KeyCode::Tab) => {
                app.settings_list_focus = match app.settings_list_focus {
                    SettingsListFocus::Managed => SettingsListFocus::Available,
                    SettingsListFocus::Available => SettingsListFocus::Managed,
                };
            }
            (_, KeyCode::Enter) | (_, KeyCode::Right)
                if app.settings_list_focus == SettingsListFocus::Managed =>
            {
                app.unmanage_selected_repo();
            }
            (_, KeyCode::Enter) | (_, KeyCode::Left)
                if app.settings_list_focus == SettingsListFocus::Available =>
            {
                app.manage_selected_repo();
            }
            _ => {}
        }
        return;
    }

    // Any key other than the expected confirmation clears pending confirmations.
    let is_quit_confirm = app.confirm_quit
        && matches!(
            (key.modifiers, key.code),
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
                | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        );
    let is_delete_confirm = app.confirm_delete
        && (key.code == KeyCode::Delete
            || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('d')));

    let had_status = app.status_message.is_some();
    if app.confirm_quit && !is_quit_confirm {
        app.confirm_quit = false;
        app.status_message = None;
    }
    if app.confirm_delete && !is_delete_confirm {
        app.confirm_delete = false;
        app.status_message = None;
    }
    // If cancelling a confirmation hid the status bar, resync layout so
    // pane dimensions match the new visible area.
    if had_status && app.status_message.is_none() {
        sync_layout(app);
    }

    match app.focus {
        FocusPanel::Left => handle_key_left(app, key),
        FocusPanel::Right => handle_key_right(app, key),
    }
}

/// Key handling when left panel (tab list) is focused.
fn handle_key_left(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        // Q/q (bare) or Ctrl+Q - quit with confirmation
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
        | (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
            if app.tabs.is_empty() {
                // No sessions to lose - quit immediately.
                app.should_quit = true;
            } else if app.confirm_quit {
                app.should_quit = true;
            } else {
                app.confirm_quit = true;
                app.status_message =
                    Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        // Ctrl+N - new tab (inherits parent's working directory)
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
            let had_status = app.status_message.is_some();
            let cwd = std::env::current_dir().ok();
            app.new_tab(cwd.as_deref());
            if app.status_message.is_some() != had_status {
                sync_layout(app);
            }
        }
        // Ctrl+D or Delete - delete tab with confirmation
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Delete) => {
            let Some(idx) = app.selected_tab else {
                return;
            };
            if idx >= app.tabs.len() {
                return;
            }
            if app.confirm_delete {
                app.confirm_delete = false;
                let had_status = app.status_message.is_some();
                app.delete_tab();
                if app.status_message.is_some() != had_status {
                    sync_layout(app);
                }
            } else {
                app.confirm_delete = true;
                app.status_message = Some("Press again to kill this session".into());
                sync_layout(app);
            }
        }
        // Up arrow - previous tab
        (_, KeyCode::Up) => {
            app.prev_tab();
        }
        // Down arrow - next tab
        (_, KeyCode::Down) => {
            app.next_tab();
        }
        // Enter - focus right panel (if a tab is selected and alive)
        (_, KeyCode::Enter) => {
            if let Some(idx) = app.selected_tab
                && idx < app.tabs.len()
                && app.tabs[idx].alive
            {
                app.focus = FocusPanel::Right;
                app.status_message = Some("Right panel focused - press Ctrl+] to return".into());
                // Status bar visibility changed - resize PTY to match.
                sync_layout(app);
            }
        }
        // ? - toggle settings overlay
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) => {
            app.show_settings = !app.show_settings;
        }
        _ => {}
    }
}

/// Key handling when right panel (PTY session) is focused.
/// Most keys are forwarded to the PTY session as raw bytes.
/// Ctrl+] returns focus to the left panel (standard "escape from session"
/// key, matching telnet/SSH conventions). Escape is forwarded to the PTY
/// so Claude Code can use it.
fn handle_key_right(app: &mut App, key: KeyEvent) {
    let had_status = app.status_message.is_some();

    // Check if the active tab is dead before forwarding keys. If dead,
    // auto-return focus to the left panel instead of spamming errors.
    if let Some(idx) = app.selected_tab
        && idx < app.tabs.len()
        && !app.tabs[idx].alive
    {
        app.focus = FocusPanel::Left;
        app.status_message = Some("Session has ended - returned to tab list".into());
        sync_layout(app);
        return;
    }

    match key.code {
        // Ctrl+] returns focus to the left panel.
        //
        // We match BOTH Char(']') and Char('5') with CONTROL because
        // crossterm 0.28's legacy keyboard parser maps byte 0x1D (Ctrl+])
        // to KeyCode::Char('5') with KeyModifiers::CONTROL.
        KeyCode::Char(']') | KeyCode::Char('5')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.focus = FocusPanel::Left;
            app.status_message = None;
            // Status bar visibility changed - resize PTY to match.
            sync_layout(app);
        }
        // Forward Escape to PTY.
        KeyCode::Esc => {
            app.send_bytes_to_active(b"\x1b");
        }
        // Forward Enter to PTY.
        KeyCode::Enter => {
            app.send_bytes_to_active(b"\r");
        }
        // Forward regular characters.
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+A = 0x01, Ctrl+B = 0x02, ..., Ctrl+Z = 0x1A
                let byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
                if byte <= 26 {
                    app.send_bytes_to_active(&[byte]);
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                // Alt+<char> = ESC byte (0x1B) followed by the character.
                let mut buf = [0u8; 5];
                let s = c.encode_utf8(&mut buf);
                let mut data = vec![0x1bu8];
                data.extend_from_slice(s.as_bytes());
                app.send_bytes_to_active(&data);
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                app.send_bytes_to_active(s.as_bytes());
            }
        }
        KeyCode::Backspace => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                // Alt+Backspace = ESC + DEL (0x1B 0x7F)
                app.send_bytes_to_active(&[0x1b, 0x7f]);
            } else {
                app.send_bytes_to_active(&[0x7f]);
            }
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                // Shift+Tab = CSI Z
                app.send_bytes_to_active(b"\x1b[Z");
            } else {
                app.send_bytes_to_active(&[0x09]);
            }
        }
        KeyCode::BackTab => {
            // Shift+Tab = CSI Z
            app.send_bytes_to_active(b"\x1b[Z");
        }
        KeyCode::Up => {
            send_arrow_key(app, b'A', key.modifiers);
        }
        KeyCode::Down => {
            send_arrow_key(app, b'B', key.modifiers);
        }
        KeyCode::Right => {
            send_arrow_key(app, b'C', key.modifiers);
        }
        KeyCode::Left => {
            send_arrow_key(app, b'D', key.modifiers);
        }
        KeyCode::Home => {
            send_special_key(app, b'H', key.modifiers);
        }
        KeyCode::End => {
            send_special_key(app, b'F', key.modifiers);
        }
        KeyCode::PageUp => {
            app.send_bytes_to_active(b"\x1b[5~");
        }
        KeyCode::PageDown => {
            app.send_bytes_to_active(b"\x1b[6~");
        }
        KeyCode::Delete => {
            app.send_bytes_to_active(b"\x1b[3~");
        }
        KeyCode::F(n) => {
            let seq = f_key_sequence(n);
            app.send_bytes_to_active(seq.as_bytes());
        }
        _ => {}
    }

    // If a send error caused a status message to appear (or disappear),
    // the status bar visibility changed and pane dimensions need updating.
    if app.status_message.is_some() != had_status {
        sync_layout(app);
    }
}

/// Send an arrow key with optional modifiers as an ANSI escape sequence.
/// Arrow keys: A=Up, B=Down, C=Right, D=Left.
fn send_arrow_key(app: &mut App, arrow: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        // Modified arrow: CSI 1 ; <modifier> <arrow>
        let seq = format!("\x1b[1;{modifier_code}{}", arrow as char);
        app.send_bytes_to_active(seq.as_bytes());
    } else {
        // Plain arrow: CSI <arrow>
        app.send_bytes_to_active(&[0x1b, b'[', arrow]);
    }
}

/// Send Home/End with optional modifiers as an ANSI escape sequence.
fn send_special_key(app: &mut App, key_char: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        let seq = format!("\x1b[1;{modifier_code}{}", key_char as char);
        app.send_bytes_to_active(seq.as_bytes());
    } else {
        app.send_bytes_to_active(&[0x1b, b'[', key_char]);
    }
}

/// Compute the xterm modifier parameter for ANSI escape sequences.
/// Returns 1 for no modifiers (caller should omit the parameter).
fn modifier_param(modifiers: KeyModifiers) -> u8 {
    let mut code: u8 = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        code += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 4;
    }
    code
}

/// Return the ANSI escape sequence for a function key F1-F12.
fn f_key_sequence(n: u8) -> String {
    match n {
        1 => "\x1bOP".into(),
        2 => "\x1bOQ".into(),
        3 => "\x1bOR".into(),
        4 => "\x1bOS".into(),
        5 => "\x1b[15~".into(),
        6 => "\x1b[17~".into(),
        7 => "\x1b[18~".into(),
        8 => "\x1b[19~".into(),
        9 => "\x1b[20~".into(),
        10 => "\x1b[21~".into(),
        11 => "\x1b[23~".into(),
        12 => "\x1b[24~".into(),
        _ => String::new(),
    }
}

/// Handle a terminal resize event by updating pane dimensions and resizing PTY.
fn handle_resize(app: &mut App, cols: u16, rows: u16) {
    let has_status = app.status_message.is_some();
    let pl = layout::compute(cols, rows, has_status);
    app.pane_cols = pl.pane_cols;
    app.pane_rows = pl.pane_rows;
    app.resize_pty_panes();
}

/// Recalculate layout from the current terminal size and resize PTY panes.
/// Called when the status bar visibility changes to keep the PTY pane
/// dimensions in sync with the actual display area.
fn sync_layout(app: &mut App) {
    if let Ok((cols, rows)) = terminal::size() {
        handle_resize(app, cols, rows);
    }
}
