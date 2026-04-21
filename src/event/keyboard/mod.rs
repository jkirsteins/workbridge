pub mod drawer;
pub mod modals;

use crate::app::{App, BOARD_COLUMNS, DashboardWindow, DisplayEntry, FocusPanel, RightPanelTab};
use crate::event::keyboard::drawer::{buffer_csi_key, f_key_sequence};
use crate::event::layout::sync_layout;
use crate::event::util::is_ctrl_symbol_char;
use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

/// Key handling when left panel (work item list) is focused.
/// Key handling for the board (Kanban) view when not drilled down.
pub fn handle_key_board(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        // Tab - toggle back to flat list view
        (KeyModifiers::NONE, KeyCode::Tab) => {
            app.toggle_view_mode();
        }
        // Left arrow - move to previous column
        (KeyModifiers::NONE, KeyCode::Left) if app.board_cursor.column > 0 => {
            app.board_cursor.column -= 1;
            let items = app.items_for_column(BOARD_COLUMNS[app.board_cursor.column]);
            app.board_cursor.row = if items.is_empty() {
                None
            } else {
                Some(app.board_cursor.row.unwrap_or(0).min(items.len() - 1))
            };
            app.sync_selection_from_board();
        }
        // Right arrow - move to next column
        (KeyModifiers::NONE, KeyCode::Right)
            if app.board_cursor.column < BOARD_COLUMNS.len() - 1 =>
        {
            app.board_cursor.column += 1;
            let items = app.items_for_column(BOARD_COLUMNS[app.board_cursor.column]);
            app.board_cursor.row = if items.is_empty() {
                None
            } else {
                Some(app.board_cursor.row.unwrap_or(0).min(items.len() - 1))
            };
            app.sync_selection_from_board();
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
            let items = app.items_for_column(BOARD_COLUMNS[app.board_cursor.column]);
            if let Some(row) = app.board_cursor.row
                && row + 1 < items.len()
            {
                app.board_cursor.row = Some(row + 1);
                app.sync_selection_from_board();
            }
        }
        // Shift+Right - advance stage
        (KeyModifiers::SHIFT, KeyCode::Right) => {
            let had_status = app.shell.status_message.is_some();
            // Sync selected_work_item so sync_board_cursor can follow the item
            // to its new column after the stage change.
            app.sync_selection_from_board();
            app.advance_stage();
            if app.shell.status_message.is_some() != had_status {
                sync_layout(app);
            }
        }
        // Shift+Left - retreat stage
        (KeyModifiers::SHIFT, KeyCode::Left) => {
            let had_status = app.shell.status_message.is_some();
            app.sync_selection_from_board();
            app.retreat_stage();
            if app.shell.status_message.is_some() != had_status {
                sync_layout(app);
            }
        }
        // Enter - drill down into item's stage (two-panel view)
        (KeyModifiers::NONE, KeyCode::Enter) if app.board_selected_work_item_id().is_some() => {
            let stage = BOARD_COLUMNS[app.board_cursor.column];
            app.board_drill_down = true;
            app.board_drill_stage = Some(stage);
            app.build_display_list();
            app.open_session_for_selected();
            sync_layout(app);
        }
        // Q/q/Ctrl+Q - quit with confirmation
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
        | (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
            if !app.has_any_session() || app.shell.confirm_quit {
                app.shell.should_quit = true;
            } else {
                app.shell.confirm_quit = true;
                app.shell.status_message =
                    Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        // Ctrl+N - quick-start a new session (creates Planning item, spawns Claude immediately)
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => match app.create_quickstart_work_item() {
            Ok(()) => {
                sync_layout(app);
            }
            Err(ref msg) if msg == "MULTIPLE_REPOS" => {
                let active_repos: Vec<std::path::PathBuf> = app
                    .active_repo_cache
                    .iter()
                    .filter(|r| r.git_dir_present)
                    .map(|r| r.path.clone())
                    .collect();
                app.create_dialog.open_quickstart(&active_repos);
                app.shell.status_message =
                    Some("Multiple repos - select one and press Enter".into());
            }
            Err(msg) => {
                app.shell.status_message = Some(msg);
            }
        },
        // Ctrl+B - open the new backlog ticket creation dialog
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
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
        // Ctrl+D or Delete - open the delete confirmation modal.
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Delete) => {
            if app.selected_work_item_id().is_none() {
                return;
            }
            app.open_delete_prompt();
            sync_layout(app);
        }
        // ? - toggle settings overlay
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) => {
            app.show_settings = !app.show_settings;
        }
        _ => {}
    }
}

/// Key handling for the global metrics Dashboard view. Tab cycles to the
/// next view; number keys 1..4 select the rolling time window. All other
/// keys are ignored (no per-item interaction in this view).
pub fn handle_key_dashboard(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Tab) => {
            app.toggle_view_mode();
        }
        (KeyModifiers::NONE, KeyCode::Char('1')) => {
            app.dashboard_window = DashboardWindow::Week;
        }
        (KeyModifiers::NONE, KeyCode::Char('2')) => {
            app.dashboard_window = DashboardWindow::Month;
        }
        (KeyModifiers::NONE, KeyCode::Char('3')) => {
            app.dashboard_window = DashboardWindow::Quarter;
        }
        (KeyModifiers::NONE, KeyCode::Char('4')) => {
            app.dashboard_window = DashboardWindow::Year;
        }
        // Q/q/Ctrl+Q - quit with confirmation (mirrors handle_key_board).
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
        | (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
            if !app.has_any_session() || app.shell.confirm_quit {
                app.shell.should_quit = true;
            } else {
                app.shell.confirm_quit = true;
                app.shell.status_message =
                    Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        // ?/Shift+? - settings overlay toggle (parity with handle_key_board).
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) => {
            app.show_settings = !app.show_settings;
        }
        _ => {}
    }
}

/// Key handling when left panel (work item list) is focused.
pub fn handle_key_left(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        // Q/q (bare) or Ctrl+Q - quit with confirmation
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('q' | 'Q'))
        | (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
            if !app.has_any_session() {
                // No live sessions to lose - quit immediately.
                app.shell.should_quit = true;
            } else if app.shell.confirm_quit {
                app.shell.should_quit = true;
            } else {
                app.shell.confirm_quit = true;
                app.shell.status_message =
                    Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        // Ctrl+N - quick-start a new session (creates Planning item, spawns Claude immediately)
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => match app.create_quickstart_work_item() {
            Ok(()) => {
                sync_layout(app);
            }
            Err(ref msg) if msg == "MULTIPLE_REPOS" => {
                let active_repos: Vec<std::path::PathBuf> = app
                    .active_repo_cache
                    .iter()
                    .filter(|r| r.git_dir_present)
                    .map(|r| r.path.clone())
                    .collect();
                app.create_dialog.open_quickstart(&active_repos);
                app.shell.status_message =
                    Some("Multiple repos - select one and press Enter".into());
            }
            Err(msg) => {
                app.shell.status_message = Some(msg);
            }
        },
        // Ctrl+B - open the new backlog ticket creation dialog
        (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
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
        // Ctrl+D or Delete - delete work item or clean up unlinked item
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Delete) => {
            if app.selected_work_item_id().is_some() {
                // Open the delete confirmation modal. Further keystrokes
                // are routed to handle_delete_prompt via the intercept at
                // the top of handle_key while delete_prompt_visible is
                // true, so there is no per-step state machine here.
                app.open_delete_prompt();
                sync_layout(app);
            } else if let Some(unlinked_idx) =
                app.selected_item
                    .and_then(|idx| match app.display_list.get(idx) {
                        Some(crate::app::DisplayEntry::UnlinkedItem(i)) => Some(*i),
                        _ => None,
                    })
            {
                // Unlinked item selected: show cleanup confirmation prompt.
                if let Some(ul) = app.unlinked_prs.get(unlinked_idx) {
                    let pr_number = ul.pr.number;
                    app.cleanup_unlinked_target =
                        Some((ul.repo_path.clone(), ul.branch.clone(), pr_number));
                    app.cleanup_prompt_visible = true;
                }
            }
        }
        // Up arrow - previous item (skipping non-selectable entries)
        (_, KeyCode::Up) => {
            let had_context = app.selected_work_item_context().is_some();
            app.select_prev_item();
            app.right_panel_tab = RightPanelTab::ClaudeCode;
            if app.selected_work_item_context().is_some() != had_context {
                sync_layout(app);
            }
        }
        // Down arrow - next item (skipping non-selectable entries)
        (_, KeyCode::Down) => {
            let had_context = app.selected_work_item_context().is_some();
            app.select_next_item();
            app.right_panel_tab = RightPanelTab::ClaudeCode;
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
                DisplayEntry::ReviewRequestItem(_) => {
                    app.import_selected_review_request();
                    if app.shell.status_message.is_some() != had_status {
                        sync_layout(app);
                    }
                }
                DisplayEntry::GroupHeader { .. } => {}
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
        // Ctrl+G - toggle global assistant drawer (or show the
        // first-run harness picker modal if no harness has been
        // configured yet). See `docs/UI.md` "First-run Ctrl+G modal".
        (KeyModifiers::CONTROL, KeyCode::Char('g')) => {
            app.handle_ctrl_g();
        }
        // Tab - toggle to board view
        (KeyModifiers::NONE, KeyCode::Tab) => {
            app.toggle_view_mode();
        }
        // ? - toggle settings overlay
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) => {
            app.show_settings = !app.show_settings;
        }
        // c / x - open a session on the selected work item using the
        // chosen harness. `c` is Claude, `x` is Codex. On a work-item
        // row with no live session, each key sets the per-work-item
        // harness and spawns. See `App::open_session_with_harness`.
        // The keybindings are documented in `docs/UI.md`.
        (KeyModifiers::NONE, KeyCode::Char('c')) => {
            let had_status = app.has_visible_status_bar();
            if app.selected_work_item_id().is_some() {
                app.open_session_with_harness(crate::agent_backend::AgentBackendKind::ClaudeCode);
            }
            if app.has_visible_status_bar() != had_status {
                sync_layout(app);
            }
        }
        (KeyModifiers::NONE, KeyCode::Char('x')) => {
            let had_status = app.has_visible_status_bar();
            if app.selected_work_item_id().is_some() {
                app.open_session_with_harness(crate::agent_backend::AgentBackendKind::Codex);
            }
            if app.has_visible_status_bar() != had_status {
                sync_layout(app);
            }
        }
        // k - double-press to end a live session. First press arms a
        // toast hint; a second `k` within ~1.5s SIGTERMs the session.
        // See `App::handle_k_press` for the FSM.
        (KeyModifiers::NONE, KeyCode::Char('k')) => {
            let had_status = app.has_visible_status_bar();
            app.handle_k_press();
            if app.has_visible_status_bar() != had_status {
                sync_layout(app);
            }
        }
        // o - open the selected row's PR in the default browser. Works
        // on work items (first repo association with a PR wins),
        // unlinked PRs, and review requests. Sets a "No PR to open"
        // status message on selections that have no PR. Not bound on
        // the right panel because single keystrokes there forward to
        // the PTY, and hijacking `o` there would break typing into
        // the agent.
        (KeyModifiers::NONE, KeyCode::Char('o')) => {
            let had_status = app.has_visible_status_bar();
            app.open_selected_pr_in_browser();
            if app.has_visible_status_bar() != had_status {
                sync_layout(app);
            }
        }
        // m - rebase the selected work item's branch onto the latest
        // upstream main. Spawns a background thread that runs `git
        // fetch origin <main>` and then a headless harness instance
        // wired to the workbridge MCP, with cwd set to the work item's
        // worktree, to perform the rebase and resolve any conflicts in
        // place. Single-flight via `UserActionKey::RebaseOnMain` (500 ms
        // debounce); a second `m` press while a rebase is in flight is
        // silently coalesced. Not added to `handle_key_right` for the
        // same reason as `o`: single keystrokes in the right panel are
        // forwarded to the PTY.
        (KeyModifiers::NONE, KeyCode::Char('m')) => {
            let had_status = app.has_visible_status_bar();
            app.start_rebase_on_main();
            if app.has_visible_status_bar() != had_status {
                sync_layout(app);
            }
        }
        _ => {}
    }
}

/// Key handling when right panel (PTY session) is focused.
/// Most keys are forwarded to the PTY session as raw bytes.
/// Ctrl+] returns focus to the left panel (standard "escape from session"
/// key, matching telnet/SSH conventions). Escape is forwarded to the PTY
/// so Claude Code can use it.
pub fn handle_key_right(app: &mut App, key: KeyEvent) -> bool {
    // Clear any active text selection on keypress.
    if let Some(entry) = app.active_session_entry_mut() {
        entry.selection = None;
    }
    if let Some(entry) = app.active_terminal_entry_mut() {
        entry.selection = None;
    }
    if let Some(entry) = app.global_session.as_mut() {
        entry.selection = None;
    }

    // Exit scrollback mode on any keypress. The key is still forwarded
    // to the PTY so the user seamlessly resumes typing.
    if app
        .active_session_entry()
        .is_some_and(|e| e.scrollback_offset > 0)
        && let Some(entry) = app.active_session_entry_mut()
    {
        entry.scrollback_offset = 0;
    }

    // Check if the active session/terminal is dead before forwarding keys.
    // Flush any buffered PTY bytes before changing state.
    //
    // No Tab exemption is needed here: the right-panel tab cycler lives on
    // the global `Ctrl+\` intercept in `handle_key()` above, which runs
    // before this function is reached. Plain Tab is just a PTY byte now,
    // so on a dead session it falls through to the standard "return to
    // work items" escape-hatch like every other key.
    match app.right_panel_tab {
        RightPanelTab::ClaudeCode => {
            if let Some(entry) = app.active_session_entry() {
                if !entry.alive {
                    app.flush_pty_buffers();
                    app.shell.focus = FocusPanel::Left;
                    app.shell.status_message =
                        Some("Session has ended - returned to work items".into());
                    sync_layout(app);
                    return true;
                }
            } else {
                // No session for this work item - return to left panel.
                app.flush_pty_buffers();
                app.shell.focus = FocusPanel::Left;
                app.shell.status_message = None;
                sync_layout(app);
                return true;
            }
        }
        RightPanelTab::Terminal => {
            if let Some(entry) = app.active_terminal_entry() {
                if !entry.alive {
                    app.flush_pty_buffers();
                    app.shell.focus = FocusPanel::Left;
                    app.shell.status_message =
                        Some("Terminal session has ended - returned to work items".into());
                    sync_layout(app);
                    return true;
                }
            } else {
                // No terminal session yet - return to left panel.
                app.flush_pty_buffers();
                app.shell.focus = FocusPanel::Left;
                app.shell.status_message = None;
                sync_layout(app);
                return true;
            }
        }
    }

    match key.code {
        // Ctrl+] returns focus to the left panel.
        //
        // The guard goes through `is_ctrl_symbol_char` so we accept
        // both the literal Char(']') and the Char('5') legacy mapping
        // that some terminals emit for the Ctrl+] control byte (0x1D).
        // See `is_ctrl_symbol_char` for the full mapping table.
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && is_ctrl_symbol_char(c, ']') =>
        {
            app.flush_pty_buffers();
            app.shell.focus = FocusPanel::Left;
            app.shell.status_message = None;
            // If returning from board drill-down, restore the full board view.
            if app.board_drill_down {
                app.board_drill_down = false;
                app.board_drill_stage = None;
                app.build_display_list();
            }
            // Status bar visibility changed - resize PTY to match.
            sync_layout(app);
            return true;
        }
        // Forward Escape to PTY.
        KeyCode::Esc => {
            app.buffer_bytes_to_right_panel(b"\x1b");
        }
        // Forward Enter to PTY.
        KeyCode::Enter => {
            app.buffer_bytes_to_right_panel(b"\r");
        }
        // Forward regular characters.
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+A = 0x01, Ctrl+B = 0x02, ..., Ctrl+Z = 0x1A
                let byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                if byte <= 26 {
                    app.buffer_bytes_to_right_panel(&[byte]);
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                // Alt+<char> = ESC byte (0x1B) followed by the character.
                let mut buf = [0u8; 5];
                let s = c.encode_utf8(&mut buf);
                let mut data = vec![0x1bu8];
                data.extend_from_slice(s.as_bytes());
                app.buffer_bytes_to_right_panel(&data);
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                app.buffer_bytes_to_right_panel(s.as_bytes());
            }
        }
        KeyCode::Backspace => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                // Alt+Backspace = ESC + DEL (0x1B 0x7F)
                app.buffer_bytes_to_right_panel(&[0x1b, 0x7f]);
            } else {
                app.buffer_bytes_to_right_panel(&[0x7f]);
            }
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                // Shift+Tab = CSI Z - forward to PTY.
                app.buffer_bytes_to_right_panel(b"\x1b[Z");
            } else {
                // Plain Tab is forwarded to the PTY as a literal tab byte
                // so Claude Code's autocomplete can fire. Right-panel tab
                // cycling lives on Ctrl+\ instead; see the global intercept
                // in `handle_key()`.
                app.buffer_bytes_to_right_panel(b"\t");
            }
        }
        KeyCode::BackTab => {
            // Shift+Tab = CSI Z - forward to PTY.
            app.buffer_bytes_to_right_panel(b"\x1b[Z");
        }
        KeyCode::Up => {
            buffer_csi_key(app, b'A', key.modifiers);
        }
        KeyCode::Down => {
            buffer_csi_key(app, b'B', key.modifiers);
        }
        KeyCode::Right => {
            buffer_csi_key(app, b'C', key.modifiers);
        }
        KeyCode::Left => {
            buffer_csi_key(app, b'D', key.modifiers);
        }
        KeyCode::Home => {
            buffer_csi_key(app, b'H', key.modifiers);
        }
        KeyCode::End => {
            buffer_csi_key(app, b'F', key.modifiers);
        }
        KeyCode::PageUp => {
            app.buffer_bytes_to_right_panel(b"\x1b[5~");
        }
        KeyCode::PageDown => {
            app.buffer_bytes_to_right_panel(b"\x1b[6~");
        }
        KeyCode::Delete => {
            app.buffer_bytes_to_right_panel(b"\x1b[3~");
        }
        KeyCode::F(n) => {
            let seq = f_key_sequence(n);
            app.buffer_bytes_to_right_panel(seq.as_bytes());
        }
        _ => {}
    }

    false
}
