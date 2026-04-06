use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, BOARD_COLUMNS, DisplayEntry, FocusPanel, SettingsListFocus, ViewMode};
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

    // When the global assistant drawer is open, route all keys to it.
    if app.global_drawer_open {
        handle_global_drawer_key(app, key);
        return;
    }

    // When the create dialog is open, route all keys to it.
    if app.create_dialog.visible {
        handle_create_dialog(app, key);
        return;
    }

    // When the rework reason prompt is visible, route keys to it.
    if app.rework_prompt_visible {
        handle_rework_prompt(app, key);
        return;
    }

    // When the no-plan prompt is visible, route keys to it.
    if app.no_plan_prompt_visible {
        handle_no_plan_prompt(app, key);
        return;
    }

    // When the merge strategy prompt is visible, handle it.
    if app.confirm_merge {
        handle_merge_prompt(app, key);
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

    let had_status = app.has_visible_status_bar();
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
    if had_status && !app.has_visible_status_bar() {
        sync_layout(app);
    }

    // Board mode (without drill-down) has its own key handler.
    if app.view_mode == ViewMode::Board && !app.board_drill_down {
        handle_key_board(app, key);
        return;
    }

    match app.focus {
        FocusPanel::Left => handle_key_left(app, key),
        FocusPanel::Right => handle_key_right(app, key),
    }
}

/// Key handling when left panel (work item list) is focused.
/// Key handling for the board (Kanban) view when not drilled down.
fn handle_key_board(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        // Tab - toggle back to flat list view
        (KeyModifiers::NONE, KeyCode::Tab) => {
            app.toggle_view_mode();
        }
        // Left arrow - move to previous column
        (KeyModifiers::NONE, KeyCode::Left) => {
            if app.board_cursor.column > 0 {
                app.board_cursor.column -= 1;
                let items = app.items_for_column(&BOARD_COLUMNS[app.board_cursor.column]);
                app.board_cursor.row = if items.is_empty() {
                    None
                } else {
                    Some(app.board_cursor.row.unwrap_or(0).min(items.len() - 1))
                };
                app.sync_selection_from_board();
            }
        }
        // Right arrow - move to next column
        (KeyModifiers::NONE, KeyCode::Right) => {
            if app.board_cursor.column < BOARD_COLUMNS.len() - 1 {
                app.board_cursor.column += 1;
                let items = app.items_for_column(&BOARD_COLUMNS[app.board_cursor.column]);
                app.board_cursor.row = if items.is_empty() {
                    None
                } else {
                    Some(app.board_cursor.row.unwrap_or(0).min(items.len() - 1))
                };
                app.sync_selection_from_board();
            }
        }
        // Up arrow - previous item in column
        (KeyModifiers::NONE, KeyCode::Up) => {
            if let Some(row) = app.board_cursor.row
                && row > 0
            {
                app.board_cursor.row = Some(row - 1);
                app.sync_selection_from_board();
            }
        }
        // Down arrow - next item in column
        (KeyModifiers::NONE, KeyCode::Down) => {
            let items = app.items_for_column(&BOARD_COLUMNS[app.board_cursor.column]);
            if let Some(row) = app.board_cursor.row
                && row + 1 < items.len()
            {
                app.board_cursor.row = Some(row + 1);
                app.sync_selection_from_board();
            }
        }
        // Shift+Right - advance stage
        (KeyModifiers::SHIFT, KeyCode::Right) => {
            let had_status = app.status_message.is_some();
            app.advance_stage();
            if app.status_message.is_some() != had_status {
                sync_layout(app);
            }
        }
        // Shift+Left - retreat stage
        (KeyModifiers::SHIFT, KeyCode::Left) => {
            let had_status = app.status_message.is_some();
            app.retreat_stage();
            if app.status_message.is_some() != had_status {
                sync_layout(app);
            }
        }
        // Enter - drill down into item's stage (two-panel view)
        (KeyModifiers::NONE, KeyCode::Enter) => {
            if app.board_selected_work_item_id().is_some() {
                let stage = BOARD_COLUMNS[app.board_cursor.column].clone();
                app.board_drill_down = true;
                app.board_drill_stage = Some(stage);
                app.build_display_list();
                app.open_session_for_selected();
                sync_layout(app);
            }
        }
        // Q/q/Ctrl+Q - quit with confirmation
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
        | (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
            if !app.has_any_session() || app.confirm_quit {
                app.should_quit = true;
            } else {
                app.confirm_quit = true;
                app.status_message = Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        // Ctrl+N - open work item creation dialog
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
                app.delete_selected_work_item();
            } else {
                app.confirm_delete = true;
                app.status_message = Some("Press again to delete this work item".into());
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
                let had_status = app.has_visible_status_bar();
                let had_context = app.selected_work_item_context().is_some();
                app.delete_selected_work_item();
                if app.has_visible_status_bar() != had_status
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
            let had_status = app.has_visible_status_bar();
            match entry {
                DisplayEntry::WorkItemEntry(_) => {
                    app.open_session_for_selected();
                    // Status bar visibility may have changed - resize PTY.
                    sync_layout(app);
                }
                DisplayEntry::UnlinkedItem(_) => {
                    app.import_selected_unlinked();
                    if app.has_visible_status_bar() != had_status {
                        sync_layout(app);
                    }
                }
                _ => {}
            }
        }
        // Shift+Right - advance to next workflow stage
        (KeyModifiers::SHIFT, KeyCode::Right) => {
            let had_status = app.has_visible_status_bar();
            app.advance_stage();
            if app.has_visible_status_bar() != had_status {
                sync_layout(app);
            }
        }
        // Shift+Left - retreat to previous workflow stage
        (KeyModifiers::SHIFT, KeyCode::Left) => {
            let had_status = app.has_visible_status_bar();
            app.retreat_stage();
            if app.has_visible_status_bar() != had_status {
                sync_layout(app);
            }
        }
        // Ctrl+G - toggle global assistant drawer
        (KeyModifiers::CONTROL, KeyCode::Char('g')) => {
            app.toggle_global_drawer();
        }
        // Tab - toggle to board view
        (KeyModifiers::NONE, KeyCode::Tab) => {
            app.toggle_view_mode();
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
    let had_status = app.has_visible_status_bar();

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
            // If returning from board drill-down, restore the full board view.
            if app.board_drill_down {
                app.board_drill_down = false;
                app.board_drill_stage = None;
                app.build_display_list();
            }
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
    if app.has_visible_status_bar() != had_status {
        sync_layout(app);
    }
}

/// Key handling when the global assistant drawer is open.
/// Ctrl+G toggles the drawer (closing it, or respawning if the session
/// died). Ctrl+] also closes the drawer. Esc is forwarded to the PTY as
/// \x1b. All other keys are forwarded to the global session PTY using
/// the same encoding as handle_key_right.
fn handle_global_drawer_key(app: &mut App, key: KeyEvent) {
    // Ctrl+G toggles the drawer (handles dead-session respawn internally).
    if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.toggle_global_drawer();
        return;
    }

    // For any other key, check if the global session is alive. If dead,
    // close the drawer rather than forwarding to a defunct PTY.
    if app.global_session.as_ref().is_none_or(|s| !s.alive) {
        app.global_drawer_open = false;
        app.focus = app.pre_drawer_focus;
        app.status_message = Some("Global assistant session ended".into());
        sync_layout(app);
        return;
    }

    match key.code {
        // Ctrl+] closes the drawer.
        KeyCode::Char(']') | KeyCode::Char('5')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.global_drawer_open = false;
            app.focus = app.pre_drawer_focus;
        }
        // Forward all other keys to the global session PTY.
        KeyCode::Esc => {
            app.send_bytes_to_global(b"\x1b");
        }
        KeyCode::Enter => {
            app.send_bytes_to_global(b"\r");
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                if byte <= 26 {
                    app.send_bytes_to_global(&[byte]);
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut buf = [0u8; 5];
                let s = c.encode_utf8(&mut buf);
                let mut data = vec![0x1bu8];
                data.extend_from_slice(s.as_bytes());
                app.send_bytes_to_global(&data);
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                app.send_bytes_to_global(s.as_bytes());
            }
        }
        KeyCode::Backspace => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                app.send_bytes_to_global(&[0x1b, 0x7f]);
            } else {
                app.send_bytes_to_global(&[0x7f]);
            }
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.send_bytes_to_global(b"\x1b[Z");
            } else {
                app.send_bytes_to_global(&[0x09]);
            }
        }
        KeyCode::BackTab => {
            app.send_bytes_to_global(b"\x1b[Z");
        }
        KeyCode::Up => {
            send_global_arrow(app, b'A', key.modifiers);
        }
        KeyCode::Down => {
            send_global_arrow(app, b'B', key.modifiers);
        }
        KeyCode::Right => {
            send_global_arrow(app, b'C', key.modifiers);
        }
        KeyCode::Left => {
            send_global_arrow(app, b'D', key.modifiers);
        }
        KeyCode::Home => {
            send_global_special(app, b'H', key.modifiers);
        }
        KeyCode::End => {
            send_global_special(app, b'F', key.modifiers);
        }
        KeyCode::PageUp => {
            app.send_bytes_to_global(b"\x1b[5~");
        }
        KeyCode::PageDown => {
            app.send_bytes_to_global(b"\x1b[6~");
        }
        KeyCode::Delete => {
            app.send_bytes_to_global(b"\x1b[3~");
        }
        KeyCode::F(n) => {
            let seq = f_key_sequence(n);
            app.send_bytes_to_global(seq.as_bytes());
        }
        _ => {}
    }
}

fn send_global_arrow(app: &mut App, arrow: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        let seq = format!("\x1b[1;{modifier_code}{}", arrow as char);
        app.send_bytes_to_global(seq.as_bytes());
    } else {
        app.send_bytes_to_global(&[0x1b, b'[', arrow]);
    }
}

fn send_global_special(app: &mut App, key_char: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        let seq = format!("\x1b[1;{modifier_code}{}", key_char as char);
        app.send_bytes_to_global(seq.as_bytes());
    } else {
        app.send_bytes_to_global(&[0x1b, b'[', key_char]);
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
    let bottom_rows = u16::from(app.has_visible_status_bar())
        + u16::from(app.selected_work_item_context().is_some());
    let pl = layout::compute(cols, rows, bottom_rows);
    app.pane_cols = pl.pane_cols;
    app.pane_rows = pl.pane_rows;

    // Compute global drawer PTY dimensions via shared helper.
    let dl = layout::compute_drawer(cols, rows);
    app.global_pane_cols = dl.pane_cols;
    app.global_pane_rows = dl.pane_rows;

    app.resize_pty_panes();
}

/// Handle key events when the merge strategy prompt is visible.
///
/// 's' or Enter = squash merge, 'm' = normal merge, Esc = cancel.
fn handle_merge_prompt(app: &mut App, key: KeyEvent) {
    let had_status = app.has_visible_status_bar();
    match (key.modifiers, key.code) {
        (_, KeyCode::Char('s')) | (_, KeyCode::Enter) => {
            app.confirm_merge = false;
            if let Some(wi_id) = app.merge_wi_id.take() {
                app.execute_merge(&wi_id, "squash");
            }
        }
        (_, KeyCode::Char('m')) => {
            app.confirm_merge = false;
            if let Some(wi_id) = app.merge_wi_id.take() {
                app.execute_merge(&wi_id, "merge");
            }
        }
        (_, KeyCode::Esc) => {
            app.confirm_merge = false;
            app.merge_wi_id = None;
            app.status_message = None;
        }
        _ => {
            // Unrecognized key - cancel.
            app.confirm_merge = false;
            app.merge_wi_id = None;
            app.status_message = None;
        }
    }
    if app.has_visible_status_bar() != had_status {
        sync_layout(app);
    }
}

/// Handle key events when the rework reason text input is visible.
///
/// All keys are routed to the text input. Enter submits the reason,
/// Esc cancels and stays in Review.
fn handle_rework_prompt(app: &mut App, key: KeyEvent) {
    let had_status = app.has_visible_status_bar();
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            app.rework_prompt_visible = false;
            app.rework_prompt_input.clear();
            app.rework_prompt_wi = None;
            app.status_message = None;
        }
        (_, KeyCode::Enter) => {
            let reason = app.rework_prompt_input.text().trim().to_string();
            app.rework_prompt_visible = false;
            app.rework_prompt_input.clear();
            let wi_id = match app.rework_prompt_wi.take() {
                Some(id) => id,
                None => return,
            };

            // Store the rework reason for the implementing_rework prompt.
            if !reason.is_empty() {
                app.rework_reasons.insert(wi_id.clone(), reason.clone());
            }

            // Log the rework request to the activity log.
            let log_entry = crate::work_item_backend::ActivityEntry {
                timestamp: crate::app::now_iso8601_pub(),
                event_type: "rework_requested".to_string(),
                payload: serde_json::json!({ "reason": reason }),
            };
            if let Err(e) = app.backend.append_activity(&wi_id, &log_entry) {
                app.status_message = Some(format!("Activity log error: {e}"));
            }

            // Complete the retreat from Review to Implementing.
            app.apply_stage_change(
                &wi_id,
                &crate::work_item::WorkItemStatus::Review,
                &crate::work_item::WorkItemStatus::Implementing,
                "user_rework",
            );
        }
        // Route text input keys to the SimpleTextInput.
        (_, KeyCode::Char(c)) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.rework_prompt_input.insert_char(c);
            app.status_message = Some(format!("Rework reason: {}", app.rework_prompt_input.text()));
        }
        (_, KeyCode::Backspace) => {
            app.rework_prompt_input.backspace();
            app.status_message = Some(format!("Rework reason: {}", app.rework_prompt_input.text()));
        }
        (_, KeyCode::Delete) => {
            app.rework_prompt_input.delete();
            app.status_message = Some(format!("Rework reason: {}", app.rework_prompt_input.text()));
        }
        (_, KeyCode::Left) => {
            app.rework_prompt_input.move_left();
        }
        (_, KeyCode::Right) => {
            app.rework_prompt_input.move_right();
        }
        (_, KeyCode::Home) => {
            app.rework_prompt_input.home();
        }
        (_, KeyCode::End) => {
            app.rework_prompt_input.end();
        }
        _ => {}
    }
    if app.has_visible_status_bar() != had_status {
        sync_layout(app);
    }
}

/// Handle key events when the no-plan prompt is visible.
///
/// [p] retreats the blocked item to Planning for retroactive plan creation.
/// [Esc] dismisses the prompt and keeps the item blocked.
fn handle_no_plan_prompt(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            // Dismiss the current item (stay blocked), advance to next queued.
            app.no_plan_prompt_queue.pop_front();
            if app.no_plan_prompt_queue.is_empty() {
                app.no_plan_prompt_visible = false;
                app.status_message = None;
            } else {
                app.status_message =
                    Some("No plan available. [p] Plan from branch  [Esc] Stay blocked".to_string());
            }
        }
        (KeyModifiers::NONE, KeyCode::Char('p')) => {
            let wi_id = match app.no_plan_prompt_queue.pop_front() {
                Some(id) => id,
                None => {
                    app.no_plan_prompt_visible = false;
                    return;
                }
            };
            app.plan_from_branch(&wi_id);
            // Re-set prompt text AFTER plan_from_branch (which overwrites
            // status_message), or clear prompt if queue is empty.
            if app.no_plan_prompt_queue.is_empty() {
                app.no_plan_prompt_visible = false;
            } else {
                app.status_message =
                    Some("No plan available. [p] Plan from branch  [Esc] Stay blocked".to_string());
            }
        }
        _ => {}
    }
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

        // Tab - cycle focus forward; auto-fill branch when leaving Title.
        (KeyModifiers::NONE, KeyCode::Tab) => {
            let was_title = matches!(app.create_dialog.focus_field, CreateDialogFocus::Title);
            app.create_dialog.focus_next();
            if was_title {
                app.create_dialog.auto_fill_branch();
            }
        }

        // Shift+Tab / BackTab - cycle focus backward
        (KeyModifiers::SHIFT, KeyCode::Tab) | (_, KeyCode::BackTab) => {
            app.create_dialog.focus_prev();
        }

        // Enter - in Description field inserts newline, otherwise validates and creates
        (_, KeyCode::Enter) => {
            if matches!(
                app.create_dialog.focus_field,
                CreateDialogFocus::Description
            ) {
                app.create_dialog.description_input.insert_newline();
                app.create_dialog
                    .description_input
                    .ensure_visible(crate::ui::DESC_TEXTAREA_HEIGHT as usize);
                return;
            }
            match app.create_dialog.validate() {
                Ok((title, description, repos, branch)) => {
                    let had_status = app.has_visible_status_bar();
                    let had_context = app.selected_work_item_context().is_some();
                    match app.create_work_item_with(title, description, repos, branch) {
                        Ok(()) => {
                            app.create_dialog.close();
                            if app.has_visible_status_bar() != had_status
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
            }
        }

        // Keys handled differently depending on focused field
        _ => {
            match app.create_dialog.focus_field {
                CreateDialogFocus::Title | CreateDialogFocus::Branch => {
                    // Forward to the focused text input
                    handle_text_input_key(app, key);
                }
                CreateDialogFocus::Description => {
                    handle_textarea_key(app, key);
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
    let is_branch = matches!(app.create_dialog.focus_field, CreateDialogFocus::Branch);

    let Some(input) = app.create_dialog.focused_input_mut() else {
        return;
    };

    let is_content_key = matches!(
        key.code,
        KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
    ) && !key.modifiers.contains(KeyModifiers::CONTROL);

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

    // Mark branch as user-edited when the user types, deletes, or backspaces
    // in the Branch field. Navigation keys (arrows, Home, End) do not count.
    if is_branch && is_content_key {
        app.create_dialog.branch_user_edited = true;
    }
}

/// Forward a key event to the description textarea in the create dialog.
fn handle_textarea_key(app: &mut App, key: KeyEvent) {
    let ta = &mut app.create_dialog.description_input;
    match (key.modifiers, key.code) {
        (_, KeyCode::Char(c)) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            ta.insert_char(c);
        }
        (_, KeyCode::Backspace) => {
            ta.backspace();
        }
        (_, KeyCode::Delete) => {
            ta.delete();
        }
        (_, KeyCode::Left) => {
            ta.move_left();
        }
        (_, KeyCode::Right) => {
            ta.move_right();
        }
        (_, KeyCode::Up) => {
            ta.move_up();
        }
        (_, KeyCode::Down) => {
            ta.move_down();
        }
        (_, KeyCode::Home) => {
            ta.home();
        }
        (_, KeyCode::End) => {
            ta.end();
        }
        _ => {}
    }
    // Keep cursor visible within the 3-line viewport.
    ta.ensure_visible(crate::ui::DESC_TEXTAREA_HEIGHT as usize);
}

/// Recalculate layout from the current terminal size and resize PTY panes.
/// Called when the status bar visibility changes to keep the PTY pane
/// dimensions in sync with the actual display area.
pub(crate) fn sync_layout(app: &mut App) {
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

    /// Merge prompt: Esc cancels the merge prompt.
    #[test]
    fn merge_prompt_esc_cancels() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));
        app.status_message = Some("Merge prompt".into());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(!app.confirm_merge, "confirm_merge should be cleared");
        assert!(app.merge_wi_id.is_none(), "merge_wi_id should be cleared");
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared",
        );
    }

    /// Merge prompt: unrecognized key also cancels the prompt.
    #[test]
    fn merge_prompt_unknown_key_cancels() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));
        app.status_message = Some("Merge prompt".into());

        let key_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        handle_key(&mut app, key_x);

        assert!(!app.confirm_merge, "confirm_merge should be cleared");
        assert!(app.merge_wi_id.is_none(), "merge_wi_id should be cleared");
    }

    /// Rework prompt: Esc cancels and stays in Review.
    #[test]
    fn rework_prompt_esc_cancels() {
        let mut app = App::new();
        app.rework_prompt_visible = true;
        app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));
        app.status_message = Some("Rework reason: ".into());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(
            !app.rework_prompt_visible,
            "rework_prompt_visible should be cleared",
        );
        assert!(
            app.rework_prompt_wi.is_none(),
            "rework_prompt_wi should be cleared",
        );
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared",
        );
    }

    /// Rework prompt: typing characters updates the status message.
    #[test]
    fn rework_prompt_typing_updates_status() {
        let mut app = App::new();
        app.rework_prompt_visible = true;
        app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));
        app.status_message = Some("Rework reason: ".into());

        // Type 'a'
        let key_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        handle_key(&mut app, key_a);

        assert!(app.rework_prompt_visible, "prompt should still be visible");
        assert_eq!(app.rework_prompt_input.text(), "a");
        assert_eq!(
            app.status_message.as_deref(),
            Some("Rework reason: a"),
            "status should show typed text",
        );
    }

    /// Rework prompt blocks other keys (settings, quit, etc.).
    #[test]
    fn rework_prompt_blocks_other_keys() {
        let mut app = App::new();
        app.rework_prompt_visible = true;
        app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));
        app.status_message = Some("Rework reason: ".into());

        // Press 'q' - should type 'q' into the input, not quit.
        let key_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        handle_key(&mut app, key_q);

        assert!(
            !app.should_quit,
            "should not quit while rework prompt is open"
        );
        assert_eq!(app.rework_prompt_input.text(), "q");
    }

    // -- Feature: no-plan prompt --

    /// No-plan prompt: Esc dismisses and clears state.
    #[test]
    fn no_plan_prompt_esc_dismisses() {
        let mut app = App::new();
        app.no_plan_prompt_visible = true;
        app.no_plan_prompt_queue
            .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
                "/tmp/test.json",
            )));
        app.status_message =
            Some("No plan available. [p] Plan from branch  [Esc] Stay blocked".into());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(
            !app.no_plan_prompt_visible,
            "no_plan_prompt_visible should be cleared",
        );
        assert!(
            app.no_plan_prompt_queue.is_empty(),
            "no_plan_prompt_queue should be empty",
        );
        assert!(
            app.status_message.is_none(),
            "status_message should be cleared",
        );
    }

    /// No-plan prompt: Esc with queued items shows next item.
    #[test]
    fn no_plan_prompt_esc_advances_queue() {
        let mut app = App::new();
        app.no_plan_prompt_visible = true;
        app.no_plan_prompt_queue
            .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
                "/tmp/first.json",
            )));
        app.no_plan_prompt_queue
            .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
                "/tmp/second.json",
            )));
        app.status_message =
            Some("No plan available. [p] Plan from branch  [Esc] Stay blocked".into());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(
            app.no_plan_prompt_visible,
            "prompt should remain visible with queued items",
        );
        assert_eq!(
            app.no_plan_prompt_queue.len(),
            1,
            "first item should be popped, second remains",
        );
        assert!(
            app.status_message.as_deref().unwrap_or("").contains("[p]"),
            "status should show prompt for next item",
        );
    }

    /// No-plan prompt blocks other keys (quit, settings, etc.).
    #[test]
    fn no_plan_prompt_blocks_other_keys() {
        let mut app = App::new();
        app.no_plan_prompt_visible = true;
        app.no_plan_prompt_queue
            .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
                "/tmp/test.json",
            )));
        app.status_message = Some("No plan available.".into());

        // Press 'q' - should not quit.
        let key_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        handle_key(&mut app, key_q);

        assert!(
            !app.should_quit,
            "should not quit while no-plan prompt is open",
        );
        assert!(
            app.no_plan_prompt_visible,
            "prompt should still be visible after unrecognized key",
        );
    }

    /// Merge prompt blocks other keys during shutdown check.
    #[test]
    fn merge_prompt_blocks_during_active() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));

        // Press 'q' - should cancel the merge prompt, not quit.
        let key_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        handle_key(&mut app, key_q);

        assert!(
            !app.should_quit,
            "should not quit while merge prompt is open"
        );
        assert!(
            !app.confirm_merge,
            "merge should be cancelled by unknown key"
        );
    }
}
