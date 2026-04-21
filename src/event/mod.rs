pub mod keyboard;
pub mod layout;
pub mod mouse;
pub mod paste;
pub mod util;

pub use layout::{handle_resize, sync_layout};
pub use mouse::handle_mouse;
pub use paste::handle_paste;

use crate::app::{
    App, FocusPanel, RightPanelTab, SettingsListFocus, SettingsTab, UserActionKey, ViewMode,
};
use crate::event::keyboard::drawer::handle_global_drawer_key;
use crate::event::keyboard::modals::{
    handle_cleanup_prompt, handle_cleanup_reason_input, handle_create_dialog, handle_delete_prompt,
    handle_merge_prompt, handle_no_plan_prompt, handle_rework_prompt, handle_set_branch_dialog,
};
use crate::event::keyboard::{
    handle_key_board, handle_key_dashboard, handle_key_left, handle_key_right,
};
use crate::event::util::is_ctrl_symbol;
use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

/// Handle a key event by dispatching based on focus panel.
/// Called from the rat-salsa event callback in salsa.rs.
///
/// Returns `true` when app state changed and a re-render is needed.
/// Returns `false` when the key was only forwarded to a PTY session
/// (the 8ms timer tick will render the PTY echo within one frame).
pub fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    // Clear the `kk` double-press window on any key that isn't the
    // second `k` for the same work item. `handle_k_press` itself
    // re-arms on the second press, and the per-tick `prune_k_press`
    // expires it independently; this clear catches "user pressed k,
    // then pressed some unrelated key" so the arm cannot survive
    // across contexts. Only a bare `k` on the left panel can arm or
    // trigger the kill, so clearing on any other key is safe.
    if !matches!(
        (key.modifiers, key.code),
        (KeyModifiers::NONE, KeyCode::Char('k'))
    ) {
        app.clear_k_press();
    }

    // During shutdown, only Q triggers force quit. All other keys are ignored.
    // Check this before the create dialog so users cannot create work items
    // while sessions are winding down.
    if app.shell.shutting_down {
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
            app.shell.should_quit = true;
        }
        return true;
    }

    // When the first-run global-harness modal is visible, intercept
    // keys before the usual dispatch so c/x/esc route to the modal
    // and do not trigger work-item or drawer handlers below. This must
    // come before `global_drawer_open` because the modal is shown as
    // a precondition to the drawer opening for the first time.
    if app.first_run_global_harness_modal.is_some() {
        handle_first_run_global_harness_modal(app, key);
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
        if let (_, KeyCode::Enter | KeyCode::Esc) = (key.modifiers, key.code) {
            app.alert_message = None;
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
            if !app.has_any_session() || app.shell.confirm_quit {
                app.shell.should_quit = true;
            } else {
                app.shell.confirm_quit = true;
                app.shell.status_message =
                    Some("Press Q again to quit and kill all sessions".into());
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
                // The outer `if app.branch_gone_prompt.is_some()`
                // guard guarantees `take()` yields `Some`, but an
                // `if let` here avoids a restriction-lint `unwrap()`
                // and degrades to a no-op on the impossible None
                // path rather than panicking the whole TUI.
                let Some((wi_id, _)) = app.branch_gone_prompt.take() else {
                    return true;
                };
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

    // Stale-worktree recovery dialog: user can force-remove + retry, or dismiss.
    if app.stale_worktree_prompt.is_some() {
        // While recovery is in progress, swallow all keys (modal spinner).
        // Q/Ctrl+Q still triggers force-quit so a hung recovery never traps
        // the user.
        if app.stale_recovery_in_progress {
            if matches!(
                (key.modifiers, key.code),
                (
                    KeyModifiers::NONE | KeyModifiers::SHIFT,
                    KeyCode::Char('q' | 'Q')
                ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
            ) {
                if !app.has_any_session() || app.shell.confirm_quit {
                    app.shell.should_quit = true;
                } else {
                    app.shell.confirm_quit = true;
                    app.shell.status_message =
                        Some("Press Q again to quit and kill all sessions".into());
                    sync_layout(app);
                }
            }
            return true;
        }
        match (key.modifiers, key.code) {
            (_, KeyCode::Enter) => {
                // Same guard-pattern as the branch-gone dialog above:
                // the outer `is_some()` makes `take() == Some(_)`
                // structurally, but an `if let` avoids a
                // restriction-lint `unwrap()` and is a no-op on the
                // impossible None path.
                if let Some(prompt) = app.stale_worktree_prompt.take() {
                    app.spawn_stale_worktree_recovery(prompt);
                }
            }
            (_, KeyCode::Esc) => {
                app.stale_worktree_prompt = None;
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
            if !app.has_any_session() || app.shell.confirm_quit {
                app.shell.should_quit = true;
            } else {
                app.shell.confirm_quit = true;
                app.shell.status_message =
                    Some("Press Q again to quit and kill all sessions".into());
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
            if !app.has_any_session() || app.shell.confirm_quit {
                app.shell.should_quit = true;
            } else {
                app.shell.confirm_quit = true;
                app.shell.status_message =
                    Some("Press Q again to quit and kill all sessions".into());
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
                let old_value = app.services.config.defaults.review_skill.clone();
                app.services
                    .config
                    .defaults
                    .review_skill
                    .clone_from(&new_value);
                if let Err(e) = app.services.config_provider.save(&app.services.config) {
                    // Rollback on save failure.
                    app.services.config.defaults.review_skill = old_value;
                    app.shell.status_message = Some(format!("Error saving config: {e}"));
                } else {
                    app.shell.status_message = Some(format!("Review skill set to: {new_value}"));
                }
                app.settings_review_skill_editing = false;
                app.settings_review_skill_input.clear();
            }
            (_, KeyCode::Enter) if app.settings_tab == SettingsTab::ReviewGate => {
                // Start editing with the current config value.
                let current = app.services.config.defaults.review_skill.clone();
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
    let is_quit_confirm = app.shell.confirm_quit
        && matches!(
            (key.modifiers, key.code),
            (
                KeyModifiers::NONE | KeyModifiers::SHIFT,
                KeyCode::Char('q' | 'Q')
            ) | (KeyModifiers::CONTROL, KeyCode::Char('q'))
        );

    let mut state_changed = false;
    let had_status = app.has_visible_status_bar();
    if app.shell.confirm_quit && !is_quit_confirm {
        app.shell.confirm_quit = false;
        app.shell.status_message = None;
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
        // spam. Gating on `activities.pending_fetch_count` (the count of repos
        // whose `FetchStarted` has been observed but whose `RepoData` /
        // `FetcherError` has not) guarantees we never admit a second
        // refresh while the previous cycle's subprocesses are still
        // talking to github.com. The 500ms debounce below still applies
        // as a secondary guard against pre-FetchStarted spam windows.
        if app.activities.pending_fetch_count > 0 {
            app.shell.status_message = Some("Refresh already in progress".into());
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
            app.shell.status_message = Some("Refresh already in progress".into());
        }
        return true;
    }

    // Ctrl+\ - cycle right-panel tab (Claude Code <-> Terminal).
    //
    // Global so the shortcut works from both the left panel (when the
    // user wants to flip the pending view without focusing it) and the
    // right panel (when Claude Code is focused and Tab is being
    // forwarded to the PTY for autocomplete). Does NOT change focus -
    // left panel stays focused if pressed from left, right panel stays
    // focused if pressed from right.
    //
    // The match goes through `is_ctrl_symbol` so we accept both the
    // literal Char('\\') and the Char('4') legacy mapping that some
    // terminals emit for the Ctrl+\ control byte (0x1C). See
    // `is_ctrl_symbol_char` for the full mapping table.
    if is_ctrl_symbol(key, '\\') {
        cycle_right_panel_tab(app);
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

    match app.shell.focus {
        FocusPanel::Left => {
            handle_key_left(app, key);
            true
        }
        FocusPanel::Right => state_changed || handle_key_right(app, key),
    }
}

/// Cycle the right-panel tab between Claude Code and Terminal.
///
/// Bound to the global `Ctrl+\` intercept so it works from either
/// panel. Intentionally does NOT touch `app.shell.focus` or call
/// `sync_layout` - focus is preserved on whichever panel the user was
/// in, and the caller is responsible for triggering a re-render
/// (`handle_key()` returns `true`).
///
/// The worktree guard on the `ClaudeCode -> Terminal` arm is
/// preserved: if the selected work item has no worktree, the
/// transition is a no-op (the terminal session is spawned in the
/// worktree path and has nothing to attach to otherwise).
fn cycle_right_panel_tab(app: &mut App) {
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

/// Route keypresses to the first-run Ctrl+G modal. Only the harnesses
/// listed in `modal.available_harnesses` are accepted; unknown keys are
/// ignored (the modal stays up). See `App::handle_ctrl_g` for the
/// modal-open path and `App::finish_first_run_global_pick` for the
/// persistence path.
fn handle_first_run_global_harness_modal(app: &mut App, key: KeyEvent) {
    // Snapshot the list so we do not hold a borrow while we mutate
    // `app` inside the match arm.
    let available: Vec<crate::agent_backend::AgentBackendKind> = app
        .first_run_global_harness_modal
        .as_ref()
        .map(|m| m.available_harnesses.clone())
        .unwrap_or_default();

    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Esc) => {
            app.cancel_first_run_global_pick();
        }
        (KeyModifiers::NONE, KeyCode::Char(c)) => {
            if let Some(kind) = available.iter().copied().find(|k| k.keybinding() == c) {
                app.finish_first_run_global_pick(kind);
            }
            // Unknown key while the modal is up: ignore. The modal
            // stays visible. This matches the pattern for the alert
            // modal (`alert_message` handling above).
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_dispatch;
