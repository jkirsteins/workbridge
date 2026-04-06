use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, DisplayEntry, FocusPanel, SettingsListFocus};
use crate::create_dialog::CreateDialogFocus;
use crate::layout;

/// Handle a key event by dispatching based on focus panel.
/// Called from the rat-salsa event callback in salsa.rs.
pub fn handle_key(app: &mut App, key: KeyEvent) {
    // During shutdown, only Q triggers force quit. All other keys are ignored.
    // Check this before the create dialog so users cannot create work items
    // while sessions are winding down.
    if app.shutting_down {
        // Close the create dialog if it was open when shutdown began.
        if app.create_dialog.visible {
            app.create_dialog.close();
        }
        if matches!(
            (key.modifiers, key.code),
            (
                KeyModifiers::NONE | KeyModifiers::SHIFT,
                KeyCode::Char('q' | 'Q')
            ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        ) {
            app.force_kill_all();
            app.should_quit = true;
        }
        return;
    }

    // When the create dialog is open, route all keys to it.
    if app.create_dialog.visible {
        handle_create_dialog(app, key);
        return;
    }

    // When the settings overlay is open, handle overlay-specific keys.
    if app.show_settings {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) | (_, KeyCode::Esc) => {
                app.show_settings = false;
                app.settings_repo_selected = 0;
                app.settings_available_selected = 0;
                app.settings_list_focus = SettingsListFocus::Managed;
            }
            (_, KeyCode::Up) => match app.settings_list_focus {
                SettingsListFocus::Managed => {
                    app.settings_repo_selected = app.settings_repo_selected.saturating_sub(1);
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
            (
                KeyModifiers::NONE | KeyModifiers::SHIFT,
                KeyCode::Char('q' | 'Q')
            ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
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

/// Key handling when left panel (work item list) is focused.
fn handle_key_left(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        // Q/q (bare) or Ctrl+Q - quit with confirmation
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
        | (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
            if !app.has_any_session() {
                // No live sessions to lose - quit immediately.
                app.should_quit = true;
            } else if app.confirm_quit {
                app.should_quit = true;
            } else {
                app.confirm_quit = true;
                app.status_message = Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        // Ctrl+N - open the work item creation dialog
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
            let active_repos: Vec<std::path::PathBuf> = app
                .active_repo_cache
                .iter()
                .filter(|r| r.git_dir_present)
                .map(|r| r.path.clone())
                .collect();
            let cwd_repo = std::env::current_dir()
                .ok()
                .and_then(|cwd| app.managed_repo_root(&cwd));
            app.create_dialog.open(&active_repos, cwd_repo.as_ref());
        }
        // Ctrl+D or Delete - delete work item with confirmation
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Delete) => {
            if app.selected_work_item_id().is_none() {
                return;
            }
            if app.confirm_delete {
                app.confirm_delete = false;
                let had_status = app.status_message.is_some();
                let had_context = app.selected_work_item_context().is_some();
                app.delete_selected_work_item();
                if app.status_message.is_some() != had_status
                    || app.selected_work_item_context().is_some() != had_context
                {
                    sync_layout(app);
                }
            } else {
                app.confirm_delete = true;
                app.status_message = Some("Press again to delete this work item".into());
                sync_layout(app);
            }
        }
        // Up arrow - previous item (skipping non-selectable entries)
        (_, KeyCode::Up) => {
            let had_context = app.selected_work_item_context().is_some();
            app.select_prev_item();
            if app.selected_work_item_context().is_some() != had_context {
                sync_layout(app);
            }
        }
        // Down arrow - next item (skipping non-selectable entries)
        (_, KeyCode::Down) => {
            let had_context = app.selected_work_item_context().is_some();
            app.select_next_item();
            if app.selected_work_item_context().is_some() != had_context {
                sync_layout(app);
            }
        }
        // Enter - context-dependent action
        (_, KeyCode::Enter) => {
            let Some(idx) = app.selected_item else {
                return;
            };
            let Some(entry) = app.display_list.get(idx).cloned() else {
                return;
            };
            let had_status = app.status_message.is_some();
            match entry {
                DisplayEntry::WorkItemEntry(_) => {
                    app.open_session_for_selected();
                    // Status bar visibility may have changed - resize PTY.
                    sync_layout(app);
                }
                DisplayEntry::UnlinkedItem(_) => {
                    app.import_selected_unlinked();
                    if app.status_message.is_some() != had_status {
                        sync_layout(app);
                    }
                }
                _ => {}
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

    // Check if the active session is dead before forwarding keys. If dead,
    // auto-return focus to the left panel instead of spamming errors.
    if let Some(entry) = app.active_session_entry() {
        if !entry.alive {
            app.focus = FocusPanel::Left;
            app.status_message = Some("Session has ended - returned to work items".into());
            sync_layout(app);
            return;
        }
    } else {
        // No session for this work item - return to left panel.
        app.focus = FocusPanel::Left;
        app.status_message = None;
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
                let byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
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
/// Called from the rat-salsa event callback in salsa.rs.
pub fn handle_resize(app: &mut App, cols: u16, rows: u16) {
    let bottom_rows = u16::from(app.status_message.is_some())
        + u16::from(app.selected_work_item_context().is_some());
    let pl = layout::compute(cols, rows, bottom_rows);
    app.pane_cols = pl.pane_cols;
    app.pane_rows = pl.pane_rows;
    app.resize_pty_panes();
}

/// Handle key events when the create dialog is open.
///
/// Tab/Shift+Tab cycle focus between Title, Repos, and Branch fields.
/// When a text field is focused, character keys go to the text input.
/// When Repos is focused, Up/Down navigate and Space toggles selection.
/// Enter validates and creates the work item. Esc cancels.
fn handle_create_dialog(app: &mut App, key: KeyEvent) {
    // Clear validation error on any keypress (will re-show on Enter if still invalid).
    app.create_dialog.error_message = None;

    match (key.modifiers, key.code) {
        // Esc - cancel the dialog
        (_, KeyCode::Esc) => {
            app.create_dialog.close();
        }

        // Tab - cycle focus forward
        (KeyModifiers::NONE, KeyCode::Tab) => {
            app.create_dialog.focus_next();
        }

        // Shift+Tab / BackTab - cycle focus backward
        (KeyModifiers::SHIFT, KeyCode::Tab) | (_, KeyCode::BackTab) => {
            app.create_dialog.focus_prev();
        }

        // Enter - validate and create
        (_, KeyCode::Enter) => match app.create_dialog.validate() {
            Ok((title, repos, branch)) => {
                let had_status = app.status_message.is_some();
                let had_context = app.selected_work_item_context().is_some();
                match app.create_work_item_with(title, repos, branch) {
                    Ok(()) => {
                        app.create_dialog.close();
                        if app.status_message.is_some() != had_status
                            || app.selected_work_item_context().is_some() != had_context
                        {
                            sync_layout(app);
                        }
                    }
                    Err(msg) => {
                        app.create_dialog.error_message = Some(msg);
                    }
                }
            }
            Err(msg) => {
                app.create_dialog.error_message = Some(msg);
            }
        },

        // Keys handled differently depending on focused field
        _ => {
            match app.create_dialog.focus_field {
                CreateDialogFocus::Title | CreateDialogFocus::Branch => {
                    // Forward to the focused text input
                    handle_text_input_key(app, key);
                }
                CreateDialogFocus::Repos => match (key.modifiers, key.code) {
                    (_, KeyCode::Up) => app.create_dialog.repo_up(),
                    (_, KeyCode::Down) => app.create_dialog.repo_down(),
                    (_, KeyCode::Char(' ')) => app.create_dialog.toggle_repo(),
                    _ => {}
                },
            }
        }
    }
}

/// Forward a key event to the currently focused text input in the create dialog.
fn handle_text_input_key(app: &mut App, key: KeyEvent) {
    let Some(input) = app.create_dialog.focused_input_mut() else {
        return;
    };

    match (key.modifiers, key.code) {
        (_, KeyCode::Char(c)) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.insert_char(c);
        }
        (_, KeyCode::Backspace) => {
            input.backspace();
        }
        (_, KeyCode::Delete) => {
            input.delete();
        }
        (_, KeyCode::Left) => {
            input.move_left();
        }
        (_, KeyCode::Right) => {
            input.move_right();
        }
        (_, KeyCode::Home) => {
            input.home();
        }
        (_, KeyCode::End) => {
            input.end();
        }
        _ => {}
    }
}

/// Recalculate layout from the current terminal size and resize PTY panes.
/// Called when the status bar visibility changes to keep the PTY pane
/// dimensions in sync with the actual display area.
fn sync_layout(app: &mut App) {
    if let Ok((cols, rows)) = ratatui_crossterm::crossterm::terminal::size() {
        handle_resize(app, cols, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    /// F-2: Create dialog is unreachable during shutdown.
    /// When shutting_down is true, handle_key must ignore all keys except
    /// Q (force quit). Even if the create dialog was open when shutdown
    /// began, it should be closed and no input should reach it.
    #[test]
    fn create_dialog_closed_during_shutdown() {
        let mut app = App::new();

        // Open the create dialog.
        app.create_dialog.open(&[PathBuf::from("/repo/a")], None);
        assert!(app.create_dialog.visible, "dialog should be open");

        // Begin shutdown.
        app.shutting_down = true;

        // Send a key event (anything, e.g. Enter). handle_key should close
        // the dialog and ignore the key.
        let enter_key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter_key);

        assert!(
            !app.create_dialog.visible,
            "create dialog should be closed during shutdown",
        );
    }

    /// F-2: Ctrl+N does NOT open the create dialog during shutdown.
    #[test]
    fn ctrl_n_blocked_during_shutdown() {
        let mut app = App::new();
        app.shutting_down = true;

        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        handle_key(&mut app, ctrl_n);

        assert!(
            !app.create_dialog.visible,
            "Ctrl+N should not open create dialog during shutdown",
        );
    }
}
