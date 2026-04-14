use crate::salsa::ct::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crate::work_item::SelectionState;

use crate::app::{
    App, BOARD_COLUMNS, DashboardWindow, DisplayEntry, FocusPanel, RightPanelTab,
    SettingsListFocus, SettingsTab, UserActionKey, ViewMode,
};
use crate::create_dialog::CreateDialogFocus;
use crate::layout;

/// Handle a key event by dispatching based on focus panel.
/// Called from the rat-salsa event callback in salsa.rs.
///
/// Returns `true` when app state changed and a re-render is needed.
/// Returns `false` when the key was only forwarded to a PTY session
/// (the 8ms timer tick will render the PTY echo within one frame).
pub fn handle_key(app: &mut App, key: KeyEvent) -> bool {
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
        return true;
    }

    // When the global assistant drawer is open, route all keys to it.
    if app.global_drawer_open {
        return handle_global_drawer_key(app, key);
    }

    // When the create dialog is open, route all keys to it.
    if app.create_dialog.visible {
        handle_create_dialog(app, key);
        return true;
    }

    // When an alert dialog is shown, Enter or Esc dismisses it.
    // This must be checked before other prompts since alerts overlay everything.
    if app.alert_message.is_some() {
        match (key.modifiers, key.code) {
            (_, KeyCode::Enter) | (_, KeyCode::Esc) => {
                app.alert_message = None;
            }
            _ => {}
        }
        return true;
    }

    // "Set branch name" recovery modal. Must come before any handler
    // that might interpret `d`, `q`, Enter, or arrow keys so the user
    // cannot accidentally delete, quit, or advance a work item while
    // trying to type a branch name. The dialog is mutually exclusive
    // with every other prompt below (it is only opened from
    // `spawn_session` / `advance_stage`, both of which refuse to run
    // while a conflicting modal is up), so we do not need to worry
    // about it stacking on top of another dialog.
    if app.set_branch_dialog.is_some() {
        handle_set_branch_dialog(app, key);
        return true;
    }

    // When the rework reason prompt is visible, route keys to it.
    if app.rework_prompt_visible {
        handle_rework_prompt(app, key);
        return true;
    }

    // When the cleanup reason text input is active, route keys to it.
    // This must be checked before cleanup_prompt_visible because both flags
    // are true during text input.
    if app.cleanup_reason_input_active {
        handle_cleanup_reason_input(app, key);
        return true;
    }

    // When the cleanup is in progress (background thread running), swallow
    // most keys - the dialog shows a spinner and cannot be interacted with.
    // Handle Q/Ctrl+Q directly here so the user can force-quit if a subprocess
    // hangs, rather than falling through to cleanup_prompt_visible which would
    // swallow the key in its catch-all arm.
    if app.is_user_action_in_flight(&UserActionKey::UnlinkedCleanup) {
        if matches!(
            (key.modifiers, key.code),
            (
                KeyModifiers::NONE | KeyModifiers::SHIFT,
                KeyCode::Char('q' | 'Q')
            ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        ) {
            if !app.has_any_session() || app.confirm_quit {
                app.should_quit = true;
            } else {
                app.confirm_quit = true;
                app.status_message = Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        return true;
    }

    // When the unlinked cleanup confirmation prompt is visible, route keys.
    if app.cleanup_prompt_visible {
        handle_cleanup_prompt(app, key);
        return true;
    }

    // When the no-plan prompt is visible, route keys to it.
    if app.no_plan_prompt_visible {
        handle_no_plan_prompt(app, key);
        return true;
    }

    // Branch-gone dialog: user can delete the work item or dismiss.
    if app.branch_gone_prompt.is_some() {
        match (key.modifiers, key.code) {
            (_, KeyCode::Char('d')) => {
                let (wi_id, _) = app.branch_gone_prompt.take().unwrap();
                // Target the work item by identity rather than going
                // through selected_work_item_id(), which in Board view
                // reads from board_cursor rather than selected_item. The
                // modal still renders a "Delete '<title>'?" confirmation
                // so a mis-click on [d] in the branch-gone dialog does
                // not destroy the work item without a second keypress.
                // `open_delete_prompt_for` looks up the target by id and
                // surfaces "Work item not found" if the item vanished
                // between the prompt appearing and this keypress, so no
                // outer existence check is needed.
                app.open_delete_prompt_for(wi_id);
            }
            (_, KeyCode::Esc) => {
                app.branch_gone_prompt = None;
            }
            _ => {}
        }
        return true;
    }

    // In-progress guard: while the delete background thread is running,
    // swallow all keys (including Claude session input) so the modal
    // cannot be dismissed and the PTY panel cannot receive keystrokes.
    // Q/Ctrl+Q still triggers force-quit so a hung delete never traps
    // the user. Must come before delete_prompt_visible because both
    // flags are true during in-progress.
    if app.delete_in_progress {
        if matches!(
            (key.modifiers, key.code),
            (
                KeyModifiers::NONE | KeyModifiers::SHIFT,
                KeyCode::Char('q' | 'Q')
            ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        ) {
            if !app.has_any_session() || app.confirm_quit {
                app.should_quit = true;
            } else {
                app.confirm_quit = true;
                app.status_message = Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        return true;
    }

    // Delete confirmation modal: route keys to it while the prompt is
    // visible but the background thread has not yet started.
    if app.delete_prompt_visible {
        handle_delete_prompt(app, key);
        return true;
    }

    // When a merge is in progress (background thread running), swallow
    // most keys - the dialog shows a spinner and cannot be interacted with.
    if app.merge_in_progress {
        if matches!(
            (key.modifiers, key.code),
            (
                KeyModifiers::NONE | KeyModifiers::SHIFT,
                KeyCode::Char('q' | 'Q')
            ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        ) {
            if !app.has_any_session() || app.confirm_quit {
                app.should_quit = true;
            } else {
                app.confirm_quit = true;
                app.status_message = Some("Press Q again to quit and kill all sessions".into());
                sync_layout(app);
            }
        }
        return true;
    }

    // When the merge strategy prompt is visible, handle it.
    if app.confirm_merge {
        handle_merge_prompt(app, key);
        return true;
    }

    // When the settings overlay is open, handle overlay-specific keys.
    if app.show_settings {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) | (_, KeyCode::Esc)
                if !app.settings_review_skill_editing =>
            {
                app.show_settings = false;
                app.settings_tab = SettingsTab::Repos;
                app.settings_repo_selected = 0;
                app.settings_available_selected = 0;
                app.settings_list_focus = SettingsListFocus::Managed;
                app.settings_keybindings_scroll = 0;
                app.settings_review_skill_editing = false;
                app.settings_review_skill_input.clear();
            }
            (_, KeyCode::Tab) if !app.settings_review_skill_editing => {
                app.settings_tab = match app.settings_tab {
                    SettingsTab::Repos => SettingsTab::ReviewGate,
                    SettingsTab::ReviewGate => SettingsTab::Keybindings,
                    SettingsTab::Keybindings => SettingsTab::Repos,
                };
                // Reset editing state when leaving ReviewGate tab.
                app.settings_review_skill_editing = false;
                app.settings_review_skill_input.clear();
            }
            (_, KeyCode::Left) if app.settings_tab == SettingsTab::Repos => {
                app.settings_list_focus = SettingsListFocus::Managed;
            }
            (_, KeyCode::Right) if app.settings_tab == SettingsTab::Repos => {
                app.settings_list_focus = SettingsListFocus::Available;
            }
            // ReviewGate tab: editing mode routes keys to the text input.
            (_, KeyCode::Esc)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                app.settings_review_skill_editing = false;
                app.settings_review_skill_input.clear();
            }
            (_, KeyCode::Enter)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                let new_value = app.settings_review_skill_input.text().trim().to_string();
                let old_value = app.config.defaults.review_skill.clone();
                app.config.defaults.review_skill = new_value.clone();
                if let Err(e) = app.config_provider.save(&app.config) {
                    // Rollback on save failure.
                    app.config.defaults.review_skill = old_value;
                    app.status_message = Some(format!("Error saving config: {e}"));
                } else {
                    app.status_message = Some(format!("Review skill set to: {new_value}"));
                }
                app.settings_review_skill_editing = false;
                app.settings_review_skill_input.clear();
            }
            (_, KeyCode::Enter) if app.settings_tab == SettingsTab::ReviewGate => {
                // Start editing with the current config value.
                let current = app.config.defaults.review_skill.clone();
                app.settings_review_skill_input.set_text(&current);
                app.settings_review_skill_editing = true;
            }
            (_, KeyCode::Char(c))
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                app.settings_review_skill_input.insert_char(c);
            }
            (_, KeyCode::Backspace)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                app.settings_review_skill_input.delete_prev_char();
            }
            (_, KeyCode::Delete)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                app.settings_review_skill_input.delete_next_char();
            }
            (_, KeyCode::Left)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                app.settings_review_skill_input.move_left(false);
            }
            (_, KeyCode::Right)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                app.settings_review_skill_input.move_right(false);
            }
            (_, KeyCode::Home)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                app.settings_review_skill_input.move_to_line_start(false);
            }
            (_, KeyCode::End)
                if app.settings_tab == SettingsTab::ReviewGate
                    && app.settings_review_skill_editing =>
            {
                app.settings_review_skill_input.move_to_line_end(false);
            }
            (_, KeyCode::Up) => match app.settings_tab {
                SettingsTab::Repos => match app.settings_list_focus {
                    SettingsListFocus::Managed => {
                        app.settings_repo_selected = app.settings_repo_selected.saturating_sub(1);
                    }
                    SettingsListFocus::Available => {
                        app.settings_available_selected =
                            app.settings_available_selected.saturating_sub(1);
                    }
                },
                SettingsTab::ReviewGate => {}
                SettingsTab::Keybindings => {
                    app.settings_keybindings_scroll =
                        app.settings_keybindings_scroll.saturating_sub(1);
                }
            },
            (_, KeyCode::Down) => match app.settings_tab {
                SettingsTab::Repos => match app.settings_list_focus {
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
                SettingsTab::ReviewGate => {}
                SettingsTab::Keybindings => {
                    app.settings_keybindings_scroll += 1;
                }
            },
            (_, KeyCode::Enter)
                if app.settings_tab == SettingsTab::Repos
                    && app.settings_list_focus == SettingsListFocus::Managed =>
            {
                app.unmanage_selected_repo();
            }
            (_, KeyCode::Enter)
                if app.settings_tab == SettingsTab::Repos
                    && app.settings_list_focus == SettingsListFocus::Available =>
            {
                app.manage_selected_repo();
            }
            _ => {}
        }
        return true;
    }

    // Any key other than the expected confirmation clears pending confirmations.
    // Track whether any confirmation state was actually cleared so we know
    // if app state changed even when the sub-handler only forwards to a PTY.
    let is_quit_confirm = app.confirm_quit
        && matches!(
            (key.modifiers, key.code),
            (
                KeyModifiers::NONE | KeyModifiers::SHIFT,
                KeyCode::Char('q' | 'Q')
            ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        );

    let mut state_changed = false;
    let had_status = app.has_visible_status_bar();
    if app.confirm_quit && !is_quit_confirm {
        app.confirm_quit = false;
        app.status_message = None;
        state_changed = true;
    }
    // If cancelling a confirmation hid the status bar, resync layout so
    // pane dimensions match the new visible area.
    if had_status && !app.has_visible_status_bar() {
        sync_layout(app);
    }

    // Ctrl+R - force refresh GitHub data (global, works in any view).
    //
    // Gated through the user-action helper with a 500ms debounce so
    // rapid key spam does not dog-pile the fetcher / `gh` subprocess
    // pool. The structural fetcher-restart sites elsewhere in the
    // codebase continue to set `fetcher_repos_changed` directly - they
    // represent "repo set changed", not "user wants fresh data", and
    // must not be debounced.
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('r') {
        // Hard gate: any fetch cycle currently in flight (structural or
        // Ctrl+R-initiated) must drain before a new Ctrl+R is admitted.
        // `fetcher::stop` only flips an atomic flag - it does NOT kill
        // the in-flight `gh` subprocess, so a naive "restart on every
        // press" still accumulates concurrent TLS handshakes under key
        // spam. Gating on `pending_fetch_count` (the count of repos
        // whose `FetchStarted` has been observed but whose `RepoData` /
        // `FetcherError` has not) guarantees we never admit a second
        // refresh while the previous cycle's subprocesses are still
        // talking to github.com. The 500ms debounce below still applies
        // as a secondary guard against pre-FetchStarted spam windows.
        if app.pending_fetch_count > 0 {
            app.status_message = Some("Refresh already in progress".into());
            return true;
        }
        if app
            .try_begin_user_action(
                UserActionKey::GithubRefresh,
                std::time::Duration::from_millis(500),
                "Refreshing GitHub data",
            )
            .is_some()
        {
            app.fetcher_repos_changed = true;
        } else if app.is_user_action_in_flight(&UserActionKey::GithubRefresh) {
            // Only distinguish the "already in flight" case; the
            // debounce rejection is intentionally silent so normal
            // key-spam protection does not pollute the status bar.
            app.status_message = Some("Refresh already in progress".into());
        }
        return true;
    }

    // Board mode (without drill-down) has its own key handler.
    if app.view_mode == ViewMode::Board && !app.board_drill_down {
        handle_key_board(app, key);
        return true;
    }

    // Dashboard mode has its own key handler (number keys for time window,
    // Tab to cycle out).
    if app.view_mode == ViewMode::Dashboard {
        handle_key_dashboard(app, key);
        return true;
    }

    match app.focus {
        FocusPanel::Left => {
            handle_key_left(app, key);
            true
        }
        FocusPanel::Right => state_changed || handle_key_right(app, key),
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
            // Sync selected_work_item so sync_board_cursor can follow the item
            // to its new column after the stage change.
            app.sync_selection_from_board();
            app.advance_stage();
            if app.status_message.is_some() != had_status {
                sync_layout(app);
            }
        }
        // Shift+Left - retreat stage
        (KeyModifiers::SHIFT, KeyCode::Left) => {
            let had_status = app.status_message.is_some();
            app.sync_selection_from_board();
            app.retreat_stage();
            if app.status_message.is_some() != had_status {
                sync_layout(app);
            }
        }
        // Enter - drill down into item's stage (two-panel view)
        (KeyModifiers::NONE, KeyCode::Enter) => {
            if app.board_selected_work_item_id().is_some() {
                let stage = BOARD_COLUMNS[app.board_cursor.column];
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
                app.status_message = Some("Multiple repos - select one and press Enter".into());
            }
            Err(msg) => {
                app.status_message = Some(msg);
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
fn handle_key_dashboard(app: &mut App, key: KeyEvent) {
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
            if !app.has_any_session() || app.confirm_quit {
                app.should_quit = true;
            } else {
                app.confirm_quit = true;
                app.status_message = Some("Press Q again to quit and kill all sessions".into());
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
                app.status_message = Some("Multiple repos - select one and press Enter".into());
            }
            Err(msg) => {
                app.status_message = Some(msg);
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
                    if app.status_message.is_some() != had_status {
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
fn handle_key_right(app: &mut App, key: KeyEvent) -> bool {
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

    // Plain Tab (no modifiers) is exempt from the dead-session early-return
    // so it can reach the Tab-cycle handler below. This matches the on-screen
    // hint "Press Tab to switch back to Claude Code" rendered on the dead
    // terminal placeholder (src/ui.rs) and the docs/UI.md contract that Tab
    // cycles between Claude Code and Terminal tabs. Shift+Tab / BackTab are
    // NOT exempted - they keep the existing dead-session "return to work
    // items" escape-hatch behavior.
    let is_tab_cycle = key.code == KeyCode::Tab && !key.modifiers.contains(KeyModifiers::SHIFT);

    // Check if the active session/terminal is dead before forwarding keys.
    // Flush any buffered PTY bytes before changing state.
    if !is_tab_cycle {
        match app.right_panel_tab {
            RightPanelTab::ClaudeCode => {
                if let Some(entry) = app.active_session_entry() {
                    if !entry.alive {
                        app.flush_pty_buffers();
                        app.focus = FocusPanel::Left;
                        app.status_message =
                            Some("Session has ended - returned to work items".into());
                        sync_layout(app);
                        return true;
                    }
                } else {
                    // No session for this work item - return to left panel.
                    app.flush_pty_buffers();
                    app.focus = FocusPanel::Left;
                    app.status_message = None;
                    sync_layout(app);
                    return true;
                }
            }
            RightPanelTab::Terminal => {
                if let Some(entry) = app.active_terminal_entry() {
                    if !entry.alive {
                        app.flush_pty_buffers();
                        app.focus = FocusPanel::Left;
                        app.status_message =
                            Some("Terminal session has ended - returned to work items".into());
                        sync_layout(app);
                        return true;
                    }
                } else {
                    // No terminal session yet - return to left panel.
                    app.flush_pty_buffers();
                    app.focus = FocusPanel::Left;
                    app.status_message = None;
                    sync_layout(app);
                    return true;
                }
            }
        }
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
            app.flush_pty_buffers();
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
                // Plain Tab: cycle right panel tab (Claude Code <-> Terminal).
                match app.right_panel_tab {
                    RightPanelTab::ClaudeCode => {
                        if app.selected_work_item_has_worktree() {
                            app.right_panel_tab = RightPanelTab::Terminal;
                            app.spawn_terminal_session();
                        }
                    }
                    RightPanelTab::Terminal => {
                        app.right_panel_tab = RightPanelTab::ClaudeCode;
                    }
                }
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
    };

    false
}

/// Key handling when the global assistant drawer is open.
/// Ctrl+G toggles the drawer (closing it, or respawning if the session
/// died). Ctrl+] also closes the drawer. Esc is forwarded to the PTY as
/// \x1b. All other keys are forwarded to the global session PTY using
/// the same encoding as handle_key_right.
fn handle_global_drawer_key(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl+G toggles the drawer (handles dead-session respawn internally).
    if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.toggle_global_drawer();
        return true;
    }

    // Clear any active text selection on keypress.
    if let Some(entry) = app.global_session.as_mut() {
        entry.selection = None;
    }

    // Exit scrollback mode on any keypress. The key is still forwarded
    // to the PTY so the user seamlessly resumes typing.
    if let Some(entry) = app.global_session.as_mut()
        && entry.scrollback_offset > 0
    {
        entry.scrollback_offset = 0;
    }

    // For any other key, check if the global session is alive. If dead,
    // close the drawer rather than forwarding to a defunct PTY.
    if app.global_session.as_ref().is_none_or(|s| !s.alive) {
        app.global_drawer_open = false;
        app.focus = app.pre_drawer_focus;
        app.status_message = Some("Global assistant session ended".into());
        sync_layout(app);
        return true;
    }

    match key.code {
        // Ctrl+] closes the drawer.
        KeyCode::Char(']') | KeyCode::Char('5')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.global_drawer_open = false;
            app.focus = app.pre_drawer_focus;
            return true;
        }
        // Forward all other keys to the global session PTY via buffer.
        KeyCode::Esc => {
            app.buffer_bytes_to_global(b"\x1b");
        }
        KeyCode::Enter => {
            app.buffer_bytes_to_global(b"\r");
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                if byte <= 26 {
                    app.buffer_bytes_to_global(&[byte]);
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut buf = [0u8; 5];
                let s = c.encode_utf8(&mut buf);
                let mut data = vec![0x1bu8];
                data.extend_from_slice(s.as_bytes());
                app.buffer_bytes_to_global(&data);
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                app.buffer_bytes_to_global(s.as_bytes());
            }
        }
        KeyCode::Backspace => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                app.buffer_bytes_to_global(&[0x1b, 0x7f]);
            } else {
                app.buffer_bytes_to_global(&[0x7f]);
            }
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.buffer_bytes_to_global(b"\x1b[Z");
            } else {
                app.buffer_bytes_to_global(&[0x09]);
            }
        }
        KeyCode::BackTab => {
            app.buffer_bytes_to_global(b"\x1b[Z");
        }
        KeyCode::Up => buffer_global_csi_key(app, b'A', key.modifiers),
        KeyCode::Down => buffer_global_csi_key(app, b'B', key.modifiers),
        KeyCode::Right => buffer_global_csi_key(app, b'C', key.modifiers),
        KeyCode::Left => buffer_global_csi_key(app, b'D', key.modifiers),
        KeyCode::Home => buffer_global_csi_key(app, b'H', key.modifiers),
        KeyCode::End => buffer_global_csi_key(app, b'F', key.modifiers),
        KeyCode::PageUp => {
            app.buffer_bytes_to_global(b"\x1b[5~");
        }
        KeyCode::PageDown => {
            app.buffer_bytes_to_global(b"\x1b[6~");
        }
        KeyCode::Delete => {
            app.buffer_bytes_to_global(b"\x1b[3~");
        }
        KeyCode::F(n) => {
            let seq = f_key_sequence(n);
            app.buffer_bytes_to_global(seq.as_bytes());
        }
        _ => {}
    }
    false
}

/// Buffer a CSI key sequence (arrow, Home, End) for the global PTY.
fn buffer_global_csi_key(app: &mut App, key: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        let seq = format!("\x1b[1;{modifier_code}{}", key as char);
        app.buffer_bytes_to_global(seq.as_bytes());
    } else {
        app.buffer_bytes_to_global(&[0x1b, b'[', key]);
    }
}

/// Buffer a CSI key sequence (arrow, Home, End) for the active right-panel PTY.
fn buffer_csi_key(app: &mut App, key: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        let seq = format!("\x1b[1;{modifier_code}{}", key as char);
        app.buffer_bytes_to_right_panel(seq.as_bytes());
    } else {
        app.buffer_bytes_to_right_panel(&[0x1b, b'[', key]);
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

/// Returns `true` when any modal overlay (dialog, confirmation, in-progress
/// operation spinner, etc.) is currently visible. Used by paste/mouse/key
/// handlers to swallow input so stray events cannot reach the underlying
/// PTY or left-panel state while a modal owns the screen.
///
/// Keep this list exhaustive: every new overlay must be added here or the
/// modal will not reliably swallow paste and mouse events.
fn any_modal_visible(app: &App) -> bool {
    app.create_dialog.visible
        || app.show_settings
        || app.rework_prompt_visible
        || app.no_plan_prompt_visible
        || app.branch_gone_prompt.is_some()
        || app.confirm_merge
        || app.cleanup_prompt_visible
        || app.is_user_action_in_flight(&UserActionKey::UnlinkedCleanup)
        || app.merge_in_progress
        || app.delete_prompt_visible
        || app.delete_in_progress
        || app.alert_message.is_some()
}

/// Handle a paste event (e.g. drag-and-drop file path, system clipboard)
/// by forwarding the pasted text to the focused PTY session as a bracketed
/// paste sequence so the receiving application handles it atomically.
pub fn handle_paste(app: &mut App, data: &str) -> bool {
    if app.shutting_down || any_modal_visible(app) {
        return false;
    }
    let bracketed = format!("\x1b[200~{data}\x1b[201~");
    if app.global_drawer_open {
        app.send_bytes_to_global(bracketed.as_bytes());
        return true;
    }
    match app.focus {
        FocusPanel::Right => {
            match app.right_panel_tab {
                RightPanelTab::ClaudeCode => app.send_bytes_to_active(bracketed.as_bytes()),
                RightPanelTab::Terminal => app.send_bytes_to_terminal(bracketed.as_bytes()),
            }
            true
        }
        FocusPanel::Left => false,
    }
}

/// Handle key events when the merge strategy prompt is visible.
///
/// 's' or Enter = squash merge, 'm' = normal merge, Esc = cancel.
fn handle_merge_prompt(app: &mut App, key: KeyEvent) {
    let had_status = app.has_visible_status_bar();
    match (key.modifiers, key.code) {
        (_, KeyCode::Char('s')) | (_, KeyCode::Enter) => {
            if let Some(wi_id) = app.merge_wi_id.clone() {
                app.execute_merge(&wi_id, "squash");
            }
        }
        (_, KeyCode::Char('m')) => {
            if let Some(wi_id) = app.merge_wi_id.clone() {
                app.execute_merge(&wi_id, "merge");
            }
        }
        (_, KeyCode::Char('p')) => {
            app.confirm_merge = false;
            if let Some(wi_id) = app.merge_wi_id.take() {
                app.enter_mergequeue(&wi_id);
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
        // Route text input keys to the rat-widget TextInputState.
        (_, KeyCode::Char(c)) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.rework_prompt_input.insert_char(c);
        }
        (_, KeyCode::Backspace) => {
            app.rework_prompt_input.delete_prev_char();
        }
        (_, KeyCode::Delete) => {
            app.rework_prompt_input.delete_next_char();
        }
        (_, KeyCode::Left) => {
            app.rework_prompt_input.move_left(false);
        }
        (_, KeyCode::Right) => {
            app.rework_prompt_input.move_right(false);
        }
        (_, KeyCode::Home) => {
            app.rework_prompt_input.move_to_line_start(false);
        }
        (_, KeyCode::End) => {
            app.rework_prompt_input.move_to_line_end(false);
        }
        _ => {}
    }
    if app.has_visible_status_bar() != had_status {
        sync_layout(app);
    }
}

/// Handle key events for the "Set branch name" recovery modal.
///
/// Enter confirms (persists the branch via `update_branch` and re-drives
/// whichever gesture opened the dialog - spawn_session or advance_stage).
/// Esc dismisses without touching the backend. Character input keys and
/// basic cursor navigation are forwarded to the rat-widget
/// `TextInputState`.
fn handle_set_branch_dialog(app: &mut App, key: KeyEvent) {
    let had_status = app.has_visible_status_bar();
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            app.cancel_set_branch_dialog();
        }
        (_, KeyCode::Enter) => {
            app.confirm_set_branch_dialog();
        }
        // Route text input keys to the dialog's TextInputState. The
        // dialog intercept above must remain higher priority than any
        // Ctrl+D / `d` / `q` handler so the user can type those
        // characters as part of a branch name.
        (_, KeyCode::Char(c)) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(dlg) = app.set_branch_dialog.as_mut() {
                dlg.input.insert_char(c);
            }
        }
        (_, KeyCode::Backspace) => {
            if let Some(dlg) = app.set_branch_dialog.as_mut() {
                dlg.input.delete_prev_char();
            }
        }
        (_, KeyCode::Delete) => {
            if let Some(dlg) = app.set_branch_dialog.as_mut() {
                dlg.input.delete_next_char();
            }
        }
        (_, KeyCode::Left) => {
            if let Some(dlg) = app.set_branch_dialog.as_mut() {
                dlg.input.move_left(false);
            }
        }
        (_, KeyCode::Right) => {
            if let Some(dlg) = app.set_branch_dialog.as_mut() {
                dlg.input.move_right(false);
            }
        }
        (_, KeyCode::Home) => {
            if let Some(dlg) = app.set_branch_dialog.as_mut() {
                dlg.input.move_to_line_start(false);
            }
        }
        (_, KeyCode::End) => {
            if let Some(dlg) = app.set_branch_dialog.as_mut() {
                dlg.input.move_to_line_end(false);
            }
        }
        _ => {}
    }
    if app.has_visible_status_bar() != had_status {
        sync_layout(app);
    }
}

/// Handle key events for the unlinked item cleanup confirmation prompt.
///
/// [Enter] transitions to the reason text input.
/// [d] closes directly without a reason.
/// [Esc] or any other key cancels.
fn handle_cleanup_prompt(app: &mut App, key: KeyEvent) {
    let had_status = app.has_visible_status_bar();
    match (key.modifiers, key.code) {
        (_, KeyCode::Enter) => {
            // Transition to reason text input.
            app.cleanup_reason_input_active = true;
            app.cleanup_reason_input.clear();
        }
        (_, KeyCode::Char('d')) => {
            // Close directly without a reason.
            app.spawn_unlinked_cleanup(None);
        }
        (_, KeyCode::Esc) => {
            // Cancel on explicit Esc only.
            app.cleanup_prompt_visible = false;
            app.cleanup_unlinked_target = None;
            app.status_message = None;
        }
        _ => {
            // Swallow unrecognized keys (arrows, function keys, etc.).
        }
    }
    if app.has_visible_status_bar() != had_status {
        sync_layout(app);
    }
}

/// Handle key events when the cleanup reason text input is active.
///
/// All printable characters are routed to the text input.
/// [Enter] submits (comments on PR then closes), [Esc] cancels the entire flow.
fn handle_cleanup_reason_input(app: &mut App, key: KeyEvent) {
    let had_status = app.has_visible_status_bar();
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            // Cancel the entire cleanup.
            app.cleanup_prompt_visible = false;
            app.cleanup_reason_input_active = false;
            app.cleanup_reason_input.clear();
            app.cleanup_unlinked_target = None;
            app.status_message = None;
        }
        (_, KeyCode::Enter) => {
            let reason = app.cleanup_reason_input.text().trim().to_string();
            let reason_opt = if reason.is_empty() {
                None
            } else {
                Some(reason.as_str())
            };
            app.spawn_unlinked_cleanup(reason_opt);
        }
        (_, KeyCode::Char(c)) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cleanup_reason_input.insert_char(c);
        }
        (_, KeyCode::Backspace) => {
            app.cleanup_reason_input.delete_prev_char();
        }
        (_, KeyCode::Delete) => {
            app.cleanup_reason_input.delete_next_char();
        }
        (_, KeyCode::Left) => {
            app.cleanup_reason_input.move_left(false);
        }
        (_, KeyCode::Right) => {
            app.cleanup_reason_input.move_right(false);
        }
        (_, KeyCode::Home) => {
            app.cleanup_reason_input.move_to_line_start(false);
        }
        (_, KeyCode::End) => {
            app.cleanup_reason_input.move_to_line_end(false);
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
            }
            // If queue still has items, the dialog stays visible with the
            // next item automatically (no status_message needed).
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
            // Clear prompt if queue is empty; otherwise dialog stays for
            // the next item (plan_from_branch may set status_message - keep it).
            if app.no_plan_prompt_queue.is_empty() {
                app.no_plan_prompt_visible = false;
            }
        }
        _ => {}
    }
}

/// Handle key events while the delete confirmation modal is visible but
/// the background cleanup thread has not started yet. Once confirmed,
/// delete_in_progress becomes true and a separate intercept higher up
/// in handle_key swallows further input.
fn handle_delete_prompt(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) => {
            app.cancel_delete_prompt();
            sync_layout(app);
        }
        (_, KeyCode::Char('y' | 'Y')) => {
            app.confirm_delete_from_prompt();
            sync_layout(app);
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
                // rat-widget's TextAreaState manages its own viewport, so
                // no explicit ensure_visible/scroll call is required - the
                // next render will adjust the scroll offset so the cursor
                // stays visible.
                app.create_dialog.description_input.insert_newline();
                return;
            }
            if app.create_dialog.quickstart_mode {
                // Quick-start mode: only need a selected repo, then create
                // a Planning item and spawn Claude immediately.
                let selected: Vec<std::path::PathBuf> = app
                    .create_dialog
                    .repo_list
                    .iter()
                    .filter(|(_, sel)| *sel)
                    .map(|(p, _)| p.clone())
                    .collect();
                if selected.is_empty() {
                    app.create_dialog.error_message = Some("Select a repo first".into());
                    return;
                }
                let repo = selected[0].clone();
                let had_status = app.has_visible_status_bar();
                let had_context = app.selected_work_item_context().is_some();
                match app.create_quickstart_work_item_for_repo(repo) {
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

/// Forward a key event to the currently focused text input in the create
/// dialog.
///
/// This drives `rat_widget::text_input::TextInputState` methods directly
/// rather than going through `rat_widget::text_input::handle_events` -
/// doing so avoids the crossterm 0.28 / 0.29 version skew documented at
/// `src/salsa.rs:22-26`.
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
            input.delete_prev_char();
        }
        (_, KeyCode::Delete) => {
            input.delete_next_char();
        }
        (_, KeyCode::Left) => {
            input.move_left(false);
        }
        (_, KeyCode::Right) => {
            input.move_right(false);
        }
        (_, KeyCode::Home) => {
            input.move_to_line_start(false);
        }
        (_, KeyCode::End) => {
            input.move_to_line_end(false);
        }
        _ => {}
    }

    // Mark branch as user-edited when the user types, deletes, or backspaces
    // in the Branch field. Navigation keys (arrows, Home, End) do not count.
    if is_branch && is_content_key {
        app.create_dialog.branch_user_edited = true;
    }
}

/// Forward a key event to the description textarea in the create
/// dialog. Drives `rat_widget::textarea::TextAreaState` methods
/// directly - viewport/scroll is managed by the textarea itself on the
/// next render, so there is no explicit `ensure_visible` call.
fn handle_textarea_key(app: &mut App, key: KeyEvent) {
    let ta = &mut app.create_dialog.description_input;
    match (key.modifiers, key.code) {
        (_, KeyCode::Char(c)) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            ta.insert_char(c);
        }
        (_, KeyCode::Backspace) => {
            ta.delete_prev_char();
        }
        (_, KeyCode::Delete) => {
            ta.delete_next_char();
        }
        (_, KeyCode::Left) => {
            ta.move_left(1, false);
        }
        (_, KeyCode::Right) => {
            ta.move_right(1, false);
        }
        (_, KeyCode::Up) => {
            ta.move_up(1, false);
        }
        (_, KeyCode::Down) => {
            ta.move_down(1, false);
        }
        (_, KeyCode::Home) => {
            ta.move_to_line_start(false);
        }
        (_, KeyCode::End) => {
            ta.move_to_line_end(false);
        }
        _ => {}
    }
}

// -- Mouse scroll handling ---------------------------------------------------

/// Which PTY area (if any) the mouse cursor is over.
enum MouseTarget {
    /// Mouse is over the global assistant drawer's inner area.
    GlobalDrawer { local_col: u16, local_row: u16 },
    /// Mouse is over the right panel's inner area.
    RightPanel { local_col: u16, local_row: u16 },
    /// Mouse is not over any PTY area.
    None,
}

/// Determine which PTY area contains the given terminal-absolute coordinates.
///
/// Checks the global drawer first (since it overlays everything), then the
/// right panel. Returns `MouseTarget::None` if outside both areas.
fn mouse_target(app: &App, column: u16, row: u16) -> MouseTarget {
    let Ok((cols, rows)) = ratatui_crossterm::crossterm::terminal::size() else {
        return MouseTarget::None;
    };

    // Check global drawer first (it overlays everything when open).
    if app.global_drawer_open {
        let dl = layout::compute_drawer(cols, rows);
        // Drawer origin matches the render code in ui.rs:
        // drawer_x = 2, drawer_y = rows - drawer_height
        let drawer_x = 2u16;
        let drawer_y = rows.saturating_sub(dl.drawer_height);

        // Inner area is 1 cell inside the border on all sides.
        let inner_x = drawer_x + 1;
        let inner_y = drawer_y + 1;
        let inner_right = drawer_x + dl.drawer_width; // exclusive
        let inner_bottom = drawer_y + dl.drawer_height; // exclusive (border row)

        if column >= inner_x
            && column < inner_right
            && row >= inner_y
            && row < inner_bottom.saturating_sub(1)
        {
            return MouseTarget::GlobalDrawer {
                local_col: column - inner_x,
                local_row: row - inner_y,
            };
        }

        // The drawer is open but the mouse is outside its inner area.
        // Do not fall through to the right panel hit-test since the
        // background is dimmed and should not receive scroll events.
        return MouseTarget::None;
    }

    // Compute right panel geometry. Must mirror draw_to_buffer in ui.rs:
    // the full area is split into a 1-row view-mode header + main_area +
    // optional bottom bars, so layout::compute is called with main_area's
    // height (rows - header - bottom_bars), not the raw terminal height.
    const HEADER_ROWS: u16 = 1;
    let bottom_rows = u16::from(app.has_visible_status_bar())
        + u16::from(app.selected_work_item_context().is_some());
    let main_area_height = rows.saturating_sub(HEADER_ROWS).saturating_sub(bottom_rows);
    let pl = layout::compute(cols, main_area_height, 0);

    // Right panel inner area: past the left panel + its left border column,
    // and past the view-mode header + the right panel's top border row.
    let inner_x = pl.left_width + 1;
    let inner_y = HEADER_ROWS + 1;

    if column >= inner_x
        && column < inner_x + pl.pane_cols
        && row >= inner_y
        && row < inner_y + pl.pane_rows
    {
        return MouseTarget::RightPanel {
            local_col: column - inner_x,
            local_row: row - inner_y,
        };
    }

    MouseTarget::None
}

/// Encode a scroll event as bytes to send to a PTY session.
///
/// When the child has not enabled mouse reporting (mode is `None`), the scroll
/// is converted to arrow key sequences (Up/Down). When the child has enabled
/// mouse reporting, the event is encoded according to the child's chosen
/// encoding (SGR or Default/Utf8).
///
/// Returns `None` if the event cannot be encoded (e.g., Default encoding with
/// coordinates exceeding 222).
fn encode_mouse_scroll(
    scroll_up: bool,
    local_col: u16,
    local_row: u16,
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    if mode == vt100::MouseProtocolMode::None {
        // Child has not enabled mouse reporting - convert to arrow keys.
        // Send 3 lines per scroll tick for usable scroll speed.
        let arrow = if scroll_up { b"\x1b[A" } else { b"\x1b[B" };
        let mut data = Vec::with_capacity(arrow.len() * 3);
        for _ in 0..3 {
            data.extend_from_slice(arrow);
        }
        return Some(data);
    }

    // Button codes: 64 = scroll up, 65 = scroll down.
    let button: u8 = if scroll_up { 64 } else { 65 };

    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            // SGR encoding: ESC [ < button ; col+1 ; row+1 M
            let seq = format!("\x1b[<{};{};{}M", button, local_col + 1, local_row + 1);
            Some(seq.into_bytes())
        }
        vt100::MouseProtocolEncoding::Default | vt100::MouseProtocolEncoding::Utf8 => {
            // X10/Default encoding: ESC [ M <button+32> <col+1+32> <row+1+32>
            // Coordinates > 222 cannot be encoded (would exceed printable byte range).
            let cx = local_col + 1 + 32;
            let cy = local_row + 1 + 32;
            if cx > 255 || cy > 255 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', button + 32, cx as u8, cy as u8])
        }
    }
}

/// Handle a mouse event. Processes scroll events (ScrollUp/ScrollDown) and
/// left-button click/drag/release for text selection.
///
/// Scroll events are hit-tested against the global drawer and right panel
/// areas. If the mouse is over a PTY area, the scroll is encoded and
/// forwarded to the corresponding PTY session.
///
/// Left-button events drive text selection: click starts a selection, drag
/// updates it, and release finalizes it and copies the selected text to the
/// system clipboard. Selection is only intercepted when the child process
/// has NOT enabled mouse reporting (or when in local scrollback mode).
///
/// Returns `true` if the event modified app state, `false` otherwise.
pub fn handle_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    // Categorize the event.
    enum MouseAction {
        Scroll { up: bool },
        SelectDown,
        SelectDrag,
        SelectUp,
    }
    let action = match mouse.kind {
        MouseEventKind::ScrollUp => MouseAction::Scroll { up: true },
        MouseEventKind::ScrollDown => MouseAction::Scroll { up: false },
        MouseEventKind::Down(MouseButton::Left) => MouseAction::SelectDown,
        MouseEventKind::Drag(MouseButton::Left) => MouseAction::SelectDrag,
        MouseEventKind::Up(MouseButton::Left) => MouseAction::SelectUp,
        _ => return false,
    };

    // Ignore during shutdown or when overlays are visible.
    if app.shutting_down || any_modal_visible(app) {
        return false;
    }

    match mouse_target(app, mouse.column, mouse.row) {
        MouseTarget::GlobalDrawer {
            local_col,
            local_row,
        } => match action {
            MouseAction::Scroll { up: scroll_up } => {
                handle_scroll_global(app, scroll_up, local_col, local_row)
            }
            MouseAction::SelectDown => {
                // Check if child wants mouse events and we are NOT in scrollback.
                if child_wants_mouse_global(app) {
                    return false;
                }
                if let Some(entry) = app.global_session.as_mut() {
                    entry.selection = Some(SelectionState {
                        anchor: (local_row, local_col),
                        current: (local_row, local_col),
                        dragging: true,
                    });
                }
                true
            }
            MouseAction::SelectDrag => {
                if let Some(entry) = app.global_session.as_mut()
                    && entry.selection.as_ref().is_some_and(|s| s.dragging)
                {
                    if let Some(sel) = entry.selection.as_mut() {
                        sel.current = (local_row, local_col);
                    }
                    return true;
                }
                false
            }
            MouseAction::SelectUp => handle_selection_up_global(app, local_row, local_col),
        },
        MouseTarget::RightPanel {
            local_col,
            local_row,
        } => match action {
            MouseAction::Scroll { up: scroll_up } => {
                handle_scroll_right(app, scroll_up, local_col, local_row)
            }
            MouseAction::SelectDown => {
                // Check if child wants mouse events and we are NOT in scrollback.
                if child_wants_mouse_right(app) {
                    return false;
                }
                if let Some(entry) = active_session_entry_mut_for_tab(app) {
                    entry.selection = Some(SelectionState {
                        anchor: (local_row, local_col),
                        current: (local_row, local_col),
                        dragging: true,
                    });
                }
                true
            }
            MouseAction::SelectDrag => {
                if let Some(entry) = active_session_entry_mut_for_tab(app)
                    && entry.selection.as_ref().is_some_and(|s| s.dragging)
                {
                    if let Some(sel) = entry.selection.as_mut() {
                        sel.current = (local_row, local_col);
                    }
                    return true;
                }
                false
            }
            MouseAction::SelectUp => handle_selection_up_right(app, local_row, local_col),
        },
        MouseTarget::None => false,
    }
}

/// Check whether the child process in the global drawer has enabled mouse
/// reporting and we are NOT in local scrollback mode. When the child wants
/// mouse events, we should forward them rather than intercepting for selection.
fn child_wants_mouse_global(app: &App) -> bool {
    // In scrollback mode, always intercept for selection.
    if app
        .global_session
        .as_ref()
        .is_some_and(|e| e.scrollback_offset > 0)
    {
        return false;
    }
    app.global_session
        .as_ref()
        .filter(|s| s.alive)
        .and_then(|s| s.parser.lock().ok())
        .is_some_and(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
}

/// Check whether the child process in the right panel has enabled mouse
/// reporting and we are NOT in local scrollback mode.
fn child_wants_mouse_right(app: &App) -> bool {
    // In scrollback mode, always intercept for selection.
    let in_scrollback = match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app
            .active_session_entry()
            .is_some_and(|e| e.scrollback_offset > 0),
        RightPanelTab::Terminal => app
            .active_terminal_entry()
            .is_some_and(|e| e.scrollback_offset > 0),
    };
    if in_scrollback {
        return false;
    }
    let entry_ref = match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app.active_session_entry(),
        RightPanelTab::Terminal => app.active_terminal_entry(),
    };
    entry_ref
        .filter(|s| s.alive)
        .and_then(|s| s.parser.lock().ok())
        .is_some_and(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
}

/// Get a mutable reference to the active session entry based on the current
/// right panel tab.
fn active_session_entry_mut_for_tab(app: &mut App) -> Option<&mut crate::work_item::SessionEntry> {
    match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app.active_session_entry_mut(),
        RightPanelTab::Terminal => app.active_terminal_entry_mut(),
    }
}

/// Handle scroll events for the global drawer.
fn handle_scroll_global(app: &mut App, scroll_up: bool, local_col: u16, local_row: u16) -> bool {
    // Scroll-up always enters/advances local scrollback (never forwarded to PTY).
    // Clamp to the terminal row count because vt100's visible_rows()
    // panics if scrollback_offset > rows (usize underflow).
    if scroll_up {
        if let Some(entry) = app.global_session.as_mut() {
            let max = entry
                .parser
                .lock()
                .ok()
                .map(|p| p.screen().size().0 as usize)
                .unwrap_or(0);
            entry.scrollback_offset = (entry.scrollback_offset + 3).min(max);
        }
        return true;
    }
    // Scroll-down while in scrollback: decrement offset locally.
    if app
        .global_session
        .as_ref()
        .is_some_and(|s| s.scrollback_offset > 0)
    {
        let entry = app.global_session.as_mut().unwrap();
        entry.scrollback_offset = entry.scrollback_offset.saturating_sub(3);
        return true;
    }
    // Scroll-down while NOT in scrollback: forward to PTY as before.
    let proto = app
        .global_session
        .as_ref()
        .filter(|s| s.alive)
        .and_then(|s| {
            let parser = s.parser.lock().ok()?;
            let screen = parser.screen();
            Some((
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            ))
        });
    if let Some((mode, encoding)) = proto
        && let Some(data) = encode_mouse_scroll(scroll_up, local_col, local_row, mode, encoding)
    {
        app.send_bytes_to_global(&data);
        return true;
    }
    false
}

/// Handle scroll events for the right panel.
fn handle_scroll_right(app: &mut App, scroll_up: bool, local_col: u16, local_row: u16) -> bool {
    // Scroll-up always enters/advances local scrollback (never forwarded to PTY).
    // Clamp to the terminal row count because vt100's visible_rows()
    // panics if scrollback_offset > rows (usize underflow).
    if scroll_up {
        if let Some(entry) = app.active_session_entry_mut() {
            let max = entry
                .parser
                .lock()
                .ok()
                .map(|p| p.screen().size().0 as usize)
                .unwrap_or(0);
            entry.scrollback_offset = (entry.scrollback_offset + 3).min(max);
        }
        return true;
    }
    // Scroll-down while in scrollback: decrement offset locally.
    if app
        .active_session_entry()
        .is_some_and(|s| s.scrollback_offset > 0)
    {
        if let Some(entry) = app.active_session_entry_mut() {
            entry.scrollback_offset = entry.scrollback_offset.saturating_sub(3);
        }
        return true;
    }
    // Scroll-down while NOT in scrollback: forward to PTY as before.
    // Extract mouse protocol info from the correct session based on
    // which tab is active. Skip if the session is not alive.
    let entry_ref = match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app.active_session_entry(),
        RightPanelTab::Terminal => app.active_terminal_entry(),
    };
    let proto = entry_ref.filter(|s| s.alive).and_then(|s| {
        let parser = s.parser.lock().ok()?;
        let screen = parser.screen();
        Some((
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        ))
    });
    if let Some((mode, encoding)) = proto
        && let Some(data) = encode_mouse_scroll(scroll_up, local_col, local_row, mode, encoding)
    {
        match app.right_panel_tab {
            RightPanelTab::ClaudeCode => app.send_bytes_to_active(&data),
            RightPanelTab::Terminal => app.send_bytes_to_terminal(&data),
        }
        return true;
    }
    false
}

/// Finalize selection on mouse-up for the global drawer session.
fn handle_selection_up_global(app: &mut App, local_row: u16, local_col: u16) -> bool {
    let entry = match app.global_session.as_mut() {
        Some(e) => e,
        None => return false,
    };
    let sel = match entry.selection.as_mut() {
        Some(s) if s.dragging => s,
        _ => return false,
    };
    sel.current = (local_row, local_col);
    sel.dragging = false;
    // If anchor == current (click with no drag), clear selection.
    if sel.anchor == sel.current {
        entry.selection = None;
        return true;
    }
    copy_selection_to_clipboard(entry);
    true
}

/// Finalize selection on mouse-up for the right panel session.
fn handle_selection_up_right(app: &mut App, local_row: u16, local_col: u16) -> bool {
    let entry = match active_session_entry_mut_for_tab(app) {
        Some(e) => e,
        None => return false,
    };
    let sel = match entry.selection.as_mut() {
        Some(s) if s.dragging => s,
        _ => return false,
    };
    sel.current = (local_row, local_col);
    sel.dragging = false;
    // If anchor == current (click with no drag), clear selection.
    if sel.anchor == sel.current {
        entry.selection = None;
        return true;
    }
    copy_selection_to_clipboard(entry);
    true
}

/// Extract the selected text from a session's terminal and copy it to the
/// system clipboard.
fn copy_selection_to_clipboard(entry: &crate::work_item::SessionEntry) {
    let sel = match entry.selection.as_ref() {
        Some(s) => s,
        None => return,
    };

    let Ok(mut parser) = entry.parser.lock() else {
        return;
    };

    // Set scrollback to match the viewport the user sees.
    let rows = parser.screen().size().0 as usize;
    let clamped = entry.scrollback_offset.min(rows);
    parser.set_scrollback(clamped);

    // Normalize selection range so start is before end.
    let (start_row, start_col, end_row, end_col) = normalize_selection(sel);

    let text = parser
        .screen()
        .contents_between(start_row, start_col, end_row, end_col);

    if text.is_empty() {
        return;
    }

    // Copy to system clipboard.
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(text);
    }
}

/// Normalize a selection so (start_row, start_col) is before (end_row, end_col).
fn normalize_selection(sel: &SelectionState) -> (u16, u16, u16, u16) {
    let (ar, ac) = sel.anchor;
    let (cr, cc) = sel.current;
    if ar < cr || (ar == cr && ac <= cc) {
        (ar, ac, cr, cc)
    } else {
        (cr, cc, ar, ac)
    }
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
    fn rework_prompt_typing_updates_input() {
        let mut app = App::new();
        app.rework_prompt_visible = true;
        app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));

        // Type 'a'
        let key_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        handle_key(&mut app, key_a);

        assert!(app.rework_prompt_visible, "prompt should still be visible");
        assert_eq!(app.rework_prompt_input.text(), "a");
        // Input is shown in the dialog overlay, not the status bar.
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
        // The dialog is now a rendered overlay; status_message is no longer used
        // to show prompt content. Dialog content comes from draw_prompt_dialog().
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

    /// Merge in-progress swallows keys (dialog shows spinner, no interaction).
    #[test]
    fn merge_in_progress_swallows_keys() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_in_progress = true;
        app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(app.confirm_merge, "dialog should stay open during progress");
        assert!(
            app.merge_in_progress,
            "in-progress flag must not clear on Esc"
        );
        assert!(
            app.merge_wi_id.is_some(),
            "merge_wi_id must not clear during progress"
        );
    }

    // -- Right-panel Tab cycling on dead sessions (regression) --

    /// Regression: plain Tab on the Terminal tab when the terminal
    /// session has ended must cycle back to Claude Code, NOT redirect
    /// to the left panel. The on-screen hint "Press Tab to switch back
    /// to Claude Code" (dead-terminal placeholder in `src/ui.rs`) and
    /// the `docs/UI.md` "Right Panel Tabs" contract both rely on this.
    #[test]
    fn tab_on_dead_terminal_cycles_to_claude_code() {
        use crate::work_item::{
            BackendType, RepoAssociation, SessionEntry, WorkItem, WorkItemId, WorkItemKind,
            WorkItemStatus,
        };
        use std::sync::{Arc, Mutex};

        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/tab-dead-terminal.json"));
        app.work_items.push(WorkItem {
            display_id: None,
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: WorkItemKind::Own,
            title: "Tab cycle test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/tmp/repo"),
                branch: Some("feature/test".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        // Install a dead terminal session for this work item.
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        app.terminal_sessions.insert(
            wi_id,
            SessionEntry {
                parser,
                alive: false,
                session: None,
                scrollback_offset: 0,
                selection: None,
            },
        );

        app.right_panel_tab = RightPanelTab::Terminal;
        app.focus = FocusPanel::Right;

        // Plain Tab should cycle to Claude Code, NOT redirect to the left panel.
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        handle_key(&mut app, tab);

        assert!(
            matches!(app.right_panel_tab, RightPanelTab::ClaudeCode),
            "plain Tab must flip the dead-terminal tab to Claude Code",
        );
        assert!(
            matches!(app.focus, FocusPanel::Right),
            "focus must stay on the right panel after the Tab flip",
        );
        let status = app.status_message.as_deref().unwrap_or("");
        assert!(
            !status.contains("returned to work items"),
            "status must not be the dead-session 'returned to work items' message, got: {status}",
        );
    }

    /// Symmetric regression: plain Tab on the Claude Code tab when the
    /// Claude session has ended must cycle to Terminal (when the work
    /// item has a worktree), keeping focus on the right panel. A
    /// pre-installed LIVE terminal session makes
    /// `spawn_terminal_session()` return early so the test does not
    /// fork a real shell.
    #[test]
    fn tab_on_dead_claude_code_cycles_to_terminal() {
        use crate::work_item::{
            BackendType, RepoAssociation, SessionEntry, WorkItem, WorkItemId, WorkItemKind,
            WorkItemStatus,
        };
        use std::sync::{Arc, Mutex};

        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/tab-dead-claude.json"));
        let wt_path = PathBuf::from("/tmp/tab-dead-claude-worktree");
        app.work_items.push(WorkItem {
            display_id: None,
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: WorkItemKind::Own,
            title: "Tab cycle test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/tmp/repo"),
                branch: Some("feature/test".into()),
                worktree_path: Some(wt_path),
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        // Install a dead Claude Code session (keyed by (wi_id, status)).
        let dead_parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        app.sessions.insert(
            (wi_id.clone(), WorkItemStatus::Implementing),
            SessionEntry {
                parser: dead_parser,
                alive: false,
                session: None,
                scrollback_offset: 0,
                selection: None,
            },
        );
        // Pre-install a LIVE terminal session so the Tab-flip's call to
        // spawn_terminal_session() sees the live entry and returns
        // early - it does NOT fork a real shell from inside the test.
        let live_parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        app.terminal_sessions.insert(
            wi_id,
            SessionEntry {
                parser: live_parser,
                alive: true,
                session: None,
                scrollback_offset: 0,
                selection: None,
            },
        );

        app.right_panel_tab = RightPanelTab::ClaudeCode;
        app.focus = FocusPanel::Right;

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        handle_key(&mut app, tab);

        assert!(
            matches!(app.right_panel_tab, RightPanelTab::Terminal),
            "plain Tab must flip the dead Claude Code tab to Terminal",
        );
        assert!(
            matches!(app.focus, FocusPanel::Right),
            "focus must stay on the right panel after the Tab flip",
        );
        let status = app.status_message.as_deref().unwrap_or("");
        assert!(
            !status.contains("returned to work items"),
            "status must not be the dead-session 'returned to work items' message, got: {status}",
        );
    }

    // -- Mouse scroll encoding tests --

    /// SGR encoding produces correct escape sequences for scroll up.
    #[test]
    fn encode_mouse_scroll_sgr_up() {
        let data = encode_mouse_scroll(
            true,
            5,
            10,
            vt100::MouseProtocolMode::PressRelease,
            vt100::MouseProtocolEncoding::Sgr,
        );
        // button=64 (scroll up), col=5+1=6, row=10+1=11
        assert_eq!(data, Some(b"\x1b[<64;6;11M".to_vec()));
    }

    /// SGR encoding produces correct escape sequences for scroll down.
    #[test]
    fn encode_mouse_scroll_sgr_down() {
        let data = encode_mouse_scroll(
            false,
            0,
            0,
            vt100::MouseProtocolMode::AnyMotion,
            vt100::MouseProtocolEncoding::Sgr,
        );
        // button=65 (scroll down), col=0+1=1, row=0+1=1
        assert_eq!(data, Some(b"\x1b[<65;1;1M".to_vec()));
    }

    /// Default (X10) encoding produces correct escape sequences.
    #[test]
    fn encode_mouse_scroll_default() {
        let data = encode_mouse_scroll(
            true,
            2,
            3,
            vt100::MouseProtocolMode::Press,
            vt100::MouseProtocolEncoding::Default,
        );
        // button=64, col=2, row=3
        // bytes: ESC [ M (64+32) (2+1+32) (3+1+32) = ESC [ M 96 35 36
        assert_eq!(data, Some(vec![0x1b, b'[', b'M', 96, 35, 36]));
    }

    /// Default encoding returns None when coordinates exceed the encodable
    /// range (col or row + 1 + 32 > 255).
    #[test]
    fn encode_mouse_scroll_default_overflow() {
        // col = 250: 250 + 1 + 32 = 283 > 255 -> None
        let data = encode_mouse_scroll(
            true,
            250,
            0,
            vt100::MouseProtocolMode::Press,
            vt100::MouseProtocolEncoding::Default,
        );
        assert_eq!(data, None);
    }

    /// When mouse protocol mode is None, scroll converts to arrow key
    /// sequences (3 per tick).
    #[test]
    fn encode_mouse_scroll_no_mode_up() {
        let data = encode_mouse_scroll(
            true,
            0,
            0,
            vt100::MouseProtocolMode::None,
            vt100::MouseProtocolEncoding::Default,
        );
        // 3x Up arrow
        assert_eq!(data, Some(b"\x1b[A\x1b[A\x1b[A".to_vec()));
    }

    /// When mouse protocol mode is None, scroll down converts to Down arrow
    /// sequences (3 per tick).
    #[test]
    fn encode_mouse_scroll_no_mode_down() {
        let data = encode_mouse_scroll(
            false,
            0,
            0,
            vt100::MouseProtocolMode::None,
            vt100::MouseProtocolEncoding::Default,
        );
        // 3x Down arrow
        assert_eq!(data, Some(b"\x1b[B\x1b[B\x1b[B".to_vec()));
    }

    /// Alert dialog: Enter dismisses it.
    #[test]
    fn alert_dialog_enter_dismisses() {
        let mut app = App::new();
        app.alert_message = Some("Some error".into());

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter);

        assert!(app.alert_message.is_none());
    }

    /// Alert dialog: Esc dismisses it.
    #[test]
    fn alert_dialog_esc_dismisses() {
        let mut app = App::new();
        app.alert_message = Some("Some error".into());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(app.alert_message.is_none());
    }

    /// Alert dialog: other keys are swallowed (alert stays visible).
    #[test]
    fn alert_dialog_swallows_other_keys() {
        let mut app = App::new();
        app.alert_message = Some("Some error".into());

        let key_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        handle_key(&mut app, key_n);

        // Alert must still be visible - 'n' was swallowed, not passed to the main handler.
        assert!(app.alert_message.is_some());
    }

    /// Cleanup in-progress: all keys are swallowed, dialog stays open.
    #[test]
    fn cleanup_in_progress_swallows_keys() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        app.try_begin_user_action(
            UserActionKey::UnlinkedCleanup,
            std::time::Duration::ZERO,
            "Cleaning up unlinked PR...",
        )
        .expect("helper admit should succeed");

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(
            app.cleanup_prompt_visible,
            "dialog should stay open during progress"
        );
        assert!(
            app.is_user_action_in_flight(&UserActionKey::UnlinkedCleanup),
            "in-progress guard must not clear on Esc"
        );
    }

    /// Delete prompt: Esc cancels the prompt and clears target state.
    #[test]
    fn delete_prompt_esc_cancels() {
        let mut app = App::new();
        app.delete_prompt_visible = true;
        app.delete_target_title = Some("Test item".into());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);

        assert!(
            !app.delete_prompt_visible,
            "Esc should dismiss the delete prompt"
        );
        assert!(
            app.delete_target_title.is_none(),
            "target title should be cleared on cancel"
        );
    }

    /// Delete prompt: unrelated keys are swallowed so stray keystrokes
    /// cannot accidentally confirm or leak into the Claude session
    /// pane beneath the modal.
    #[test]
    fn delete_prompt_swallows_other_keys() {
        let mut app = App::new();
        app.delete_prompt_visible = true;

        for ch in ['a', 'n', 'q', ' ', '\u{1b}'] {
            if ch == '\u{1b}' {
                continue; // Esc is tested separately.
            }
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            handle_key(&mut app, key);
            assert!(
                app.delete_prompt_visible,
                "prompt should still be visible after pressing '{ch}'"
            );
        }
    }

    /// Delete in-progress: even the modal's own 'y' confirm is swallowed
    /// once the background thread is running. Only Q/Ctrl+Q (force-quit
    /// escape hatch) has any effect.
    #[test]
    fn delete_in_progress_swallows_keys() {
        let mut app = App::new();
        app.delete_prompt_visible = true;
        app.delete_in_progress = true;

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, esc);
        let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        handle_key(&mut app, y);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter);

        assert!(
            app.delete_prompt_visible,
            "dialog must stay open while cleanup is running"
        );
        assert!(
            app.delete_in_progress,
            "in-progress flag must not clear on stray keys"
        );
    }

    /// Ctrl+R first press drives through `handle_key` and flips
    /// `fetcher_repos_changed = true`. This is the happy-path baseline
    /// the two gating tests below depend on (they both assume the
    /// first press is admitted through the real dispatch path).
    #[test]
    fn ctrl_r_first_press_flips_fetcher_repos_changed() {
        let mut app = App::new();
        assert!(!app.fetcher_repos_changed);
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let changed = handle_key(&mut app, ctrl_r);
        assert!(changed, "Ctrl+R must report state changed");
        assert!(
            app.fetcher_repos_changed,
            "Ctrl+R must set fetcher_repos_changed",
        );
        assert!(
            app.is_user_action_in_flight(&UserActionKey::GithubRefresh),
            "Ctrl+R must have admitted the GithubRefresh helper entry",
        );
    }

    /// Ctrl+R second press within the debounce window: `handle_key`
    /// returns `true` but `fetcher_repos_changed` must NOT be flipped a
    /// second time, because the helper entry from the first press is
    /// still in flight. This drives through the real `handle_key`
    /// dispatch so the debounce/in-flight pre-checks are actually
    /// exercised (unlike the unit tests that invoke the helper
    /// directly).
    #[test]
    fn ctrl_r_rapid_double_press_through_handle_key_is_gated() {
        let mut app = App::new();
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);

        // First press admits.
        handle_key(&mut app, ctrl_r);
        assert!(app.fetcher_repos_changed);

        // Simulate the salsa tick consuming the flag (the scheduler
        // reads and resets it once per tick when the restart block
        // runs). We do NOT reset the helper entry - a tight double-
        // press happens BEFORE `drain_fetch_results` has observed any
        // `FetchStarted`, so `pending_fetch_count` is still 0 and the
        // only protection is the helper's in-flight check.
        app.fetcher_repos_changed = false;

        // Second press within the debounce window: the helper's
        // in-flight check rejects, so `fetcher_repos_changed` stays
        // false and a status message is set.
        let changed = handle_key(&mut app, ctrl_r);
        assert!(changed, "handler still returns true on reject path");
        assert!(
            !app.fetcher_repos_changed,
            "second Ctrl+R must not re-flip fetcher_repos_changed",
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("Refresh already in progress"),
            "in-flight rejection must surface the user-visible message",
        );
    }

    /// Ctrl+R while `pending_fetch_count > 0`: the hard gate in
    /// `handle_key` rejects the press regardless of helper state. This
    /// test exercises the exact pre-check added by R1-F-1 - seeding
    /// `pending_fetch_count = 1` without touching the helper entry so
    /// only the count gate can cause the rejection.
    #[test]
    fn ctrl_r_rejected_while_pending_fetch_count_nonzero() {
        let mut app = App::new();
        // Seed as if a prior tick's `drain_fetch_results` had counted
        // one `FetchStarted`. No helper entry is inserted, so the
        // in-flight check alone would admit this press.
        app.pending_fetch_count = 1;
        assert!(!app.is_user_action_in_flight(&UserActionKey::GithubRefresh));

        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let changed = handle_key(&mut app, ctrl_r);
        assert!(changed, "handler returns true even on reject path");
        assert!(
            !app.fetcher_repos_changed,
            "count gate must block the fetcher restart",
        );
        assert!(
            !app.is_user_action_in_flight(&UserActionKey::GithubRefresh),
            "count gate must reject BEFORE the helper is admitted",
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("Refresh already in progress"),
            "count-gate rejection must surface the user-visible message",
        );
    }
}
