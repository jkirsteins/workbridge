use crate::app::App;
use crate::create_dialog::CreateDialogFocus;
use crate::event::layout::sync_layout;
use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

/// Handle key events when the merge strategy prompt is visible.
///
/// 's' or Enter = squash merge, 'm' = normal merge, Esc = cancel.
pub fn handle_merge_prompt(app: &mut App, key: KeyEvent) {
    let had_status = app.has_visible_status_bar();
    match (key.modifiers, key.code) {
        (_, KeyCode::Char('s') | KeyCode::Enter) => {
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
pub fn handle_rework_prompt(app: &mut App, key: KeyEvent) {
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
            let Some(wi_id) = app.rework_prompt_wi.take() else {
                return;
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
            if let Err(e) = app.services.backend.append_activity(&wi_id, &log_entry) {
                app.status_message = Some(format!("Activity log error: {e}"));
            }

            // Complete the retreat from Review to Implementing.
            app.apply_stage_change(
                &wi_id,
                crate::work_item::WorkItemStatus::Review,
                crate::work_item::WorkItemStatus::Implementing,
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
/// whichever gesture opened the dialog - `spawn_session` or `advance_stage`).
/// Esc dismisses without touching the backend. Character input keys and
/// basic cursor navigation are forwarded to the rat-widget
/// `TextInputState`.
pub fn handle_set_branch_dialog(app: &mut App, key: KeyEvent) {
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
pub fn handle_cleanup_prompt(app: &mut App, key: KeyEvent) {
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
pub fn handle_cleanup_reason_input(app: &mut App, key: KeyEvent) {
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
pub fn handle_no_plan_prompt(app: &mut App, key: KeyEvent) {
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
            let Some(wi_id) = app.no_plan_prompt_queue.pop_front() else {
                app.no_plan_prompt_visible = false;
                return;
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
/// `delete_in_progress` becomes true and a separate intercept higher up
/// in `handle_key` swallows further input.
pub fn handle_delete_prompt(app: &mut App, key: KeyEvent) {
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
pub fn handle_create_dialog(app: &mut App, key: KeyEvent) {
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
