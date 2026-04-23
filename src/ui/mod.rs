//! Top-level UI rendering entry and module declarations.
//!
//! The primary entry point `draw_to_buffer` is called from
//! `salsa::render_cb`. Every other piece of the TUI - header, board,
//! work list, dashboard, right pane, overlays, modals - lives in a
//! sibling submodule declared below.

mod board;
mod common;
mod dashboard;
mod detail_pane;
mod header;
mod modals;
mod output_pane;
mod overlays;
mod selection;
mod work_list;

#[cfg(test)]
mod snapshot_tests;

// Public re-exports: only the two items referenced from the outside
// (`crate::salsa` and `crate::event::mouse::selection`) are exposed.
// Every internal helper stays private to the `ui` module tree. The
// `render_selection_overlay` re-export is used only by the PTY-selection
// unit tests in `event::mouse::selection`, hence the `cfg(test)` gate.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Constraint, Direction, Layout, Rect};
use ratatui_core::text::{Line, Span};
use ratatui_core::widgets::Widget;
use ratatui_widgets::paragraph::Paragraph;

use self::board::draw_board_view;
use self::common::{SPINNER_FRAMES, truncate_str};
use self::dashboard::draw_dashboard_view;
use self::header::draw_view_mode_header;
use self::modals::create_dialog::draw_create_dialog;
use self::modals::first_run::draw_first_run_global_harness_modal;
use self::modals::prompt::{PromptDialogKind, draw_prompt_dialog};
use self::modals::toasts::draw_toasts;
use self::output_pane::draw_pane_output;
use self::overlays::context_bar::draw_context_bar;
use self::overlays::drawer::draw_global_drawer;
use self::overlays::settings::draw_settings_overlay;
#[cfg(test)]
pub use self::selection::render_selection_overlay;
use self::work_list::draw_work_item_list;
use crate::app::{App, UserActionKey, ViewMode};
use crate::layout;
use crate::theme::Theme;

/// Render the entire UI: left panel (work item list) and right panel
/// (session output), plus optional context bar and status bar at the bottom.
///
/// Buffer-based rendering entry point. Called by the rat-salsa render
/// callback. All rendering uses `Widget::render(area`, buf) and
/// `StatefulWidget::render(widget`, area, buf, &mut state) directly.
///
/// `app` is `&mut` because stateful widgets owned by `App` (currently the
/// `rat-widget` text fields inside `CreateDialog`) need `&mut State` to
/// render.
pub fn draw_to_buffer(area: Rect, buf: &mut Buffer, app: &mut App, theme: &Theme) {
    // Clear stale click targets from the previous frame before any
    // render pushes. `handle_mouse` never runs during a draw, so
    // this `borrow_mut` never conflicts with a concurrent borrow.
    app.click_tracking.registry.borrow_mut().clear();

    // Vertical split: 1-row view mode header + main area + optional context bar + optional status bar.
    let has_context = app.selected_work_item_context().is_some();
    let has_status = app.has_visible_status_bar();

    let mut constraints = vec![Constraint::Length(1), Constraint::Min(0)];
    if has_context {
        constraints.push(Constraint::Length(1));
    }
    if has_status {
        constraints.push(Constraint::Length(1));
    }
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let header_area = vertical[0];
    let main_area = vertical[1];
    let mut next_slot = 2;

    let context_area = if has_context {
        let a = vertical[next_slot];
        next_slot += 1;
        Some(a)
    } else {
        None
    };

    let status_area = if has_status {
        Some(vertical[next_slot])
    } else {
        None
    };

    // View mode header (segmented tab bar).
    draw_view_mode_header(buf, app, theme, header_area);

    // Branch on view mode.
    if app.view_mode == ViewMode::Board && !app.board_drill_down {
        // Full-width Kanban board (no right panel).
        draw_board_view(buf, app, theme, main_area);
    } else if app.view_mode == ViewMode::Dashboard {
        // Full-width metrics dashboard (no right panel).
        draw_dashboard_view(buf, app, theme, main_area);
    } else {
        // Horizontal split: left panel, right panel.
        let pl = layout::compute(main_area.width, main_area.height, 0);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(pl.left_width), Constraint::Min(0)])
            .split(main_area);

        draw_work_item_list(buf, app, theme, chunks[0]);
        draw_pane_output(buf, app, theme, chunks[1]);
    }

    // Context bar (persistent work-item info).
    if let Some(area) = context_area
        && let Some(ctx) = app.selected_work_item_context()
    {
        draw_context_bar(buf, &ctx, theme, area);
    }

    // Status bar: activity indicator overrides transient messages.
    if let Some(area) = status_area {
        render_status_bar(buf, app, theme, area);
    }

    // Settings overlay (rendered on top of everything).
    if app.settings.visible {
        draw_settings_overlay(buf, app, theme, area);
    }

    // Prompt dialogs: blocking choice/input prompts rendered as centered modal
    // dialogs with dimmed backgrounds. Order matches the handle_key() intercept
    // chain (cleanup_reason_input_active before cleanup_prompt_visible).
    draw_prompt_overlays(buf, app, theme, area);

    // Alert dialog (renders above prompt dialogs, below global drawer and create dialog).
    if let Some(ref msg) = app.alert_message {
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::Alert {
                title: "Error",
                body: msg,
            },
        );
    }

    // First-run Ctrl+G harness picker modal (rendered above alerts so
    // a transient error does not hide the picker, but below the global
    // drawer - which cannot be open at the same time). The modal is
    // opened by `App::handle_ctrl_g` when
    // `config.defaults.global_assistant_harness` is unset, and
    // dismissed by `App::finish_first_run_global_pick` or
    // `App::cancel_first_run_global_pick`. See `docs/UI.md`
    // "First-run Ctrl+G modal".
    if let Some(ref modal) = app.first_run_global_harness_modal {
        draw_first_run_global_harness_modal(buf, modal, theme, area);
    }

    // Global assistant drawer (rendered on top, below create dialog).
    if app.global_drawer.open {
        draw_global_drawer(buf, app, theme, area);
    }

    // Create dialog overlay (rendered on top of everything).
    // Uses `&mut app.create_dialog` because the rat-widget `TextInput` /
    // `TextArea` stateful widgets need `&mut State` to render.
    if app.create_dialog.visible {
        draw_create_dialog(buf, &mut app.create_dialog, theme, area);
    }

    // Top-right toast stack. Rendered LAST so it sits on top of
    // every other overlay including the global drawer and settings.
    draw_toasts(buf, &app.toasts, theme, area);
}

/// Render the bottom status-bar row: shows the current activity and a
/// spinner when any background activity is in flight, otherwise shows
/// the transient `status_message` (coloured red during shutdown).
fn render_status_bar(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    if let Some(activity_msg) = app.activities.current() {
        let spinner = SPINNER_FRAMES[app.activities.spinner_tick % SPINNER_FRAMES.len()];
        let count_suffix = if app.activities.len() > 1 {
            format!(" (+{})", app.activities.len() - 1)
        } else {
            String::new()
        };
        let line = Line::from(vec![
            Span::styled(format!(" {spinner} "), theme.style_activity_spinner()),
            Span::styled(activity_msg, theme.style_activity()),
            Span::styled(count_suffix, theme.style_text_muted()),
        ]);
        Paragraph::new(line).render(area, buf);
    } else if let Some(msg) = &app.shell.status_message {
        let style = if app.shell.shutting_down {
            theme.style_status_shutdown()
        } else {
            theme.style_status()
        };
        Paragraph::new(msg.as_str()).style(style).render(area, buf);
    }
}

/// Render the blocking-prompt overlay chain. The order mirrors the
/// `handle_key` modal-routing chain so the overlay the user sees
/// matches the overlay that captures their keypress.
fn draw_prompt_overlays(buf: &mut Buffer, app: &mut App, theme: &Theme, area: Rect) {
    if app.merge_flow.confirm {
        draw_merge_prompt_overlay(buf, app, theme, area);
    } else if let Some(dlg) = app.set_branch_dialog.as_mut() {
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::TextInput {
                title: "Set Branch Name",
                body: "This work item has no branch. Enter a name to continue.",
                input: &mut dlg.input,
                hint: "Enter: confirm   Esc: cancel",
            },
        );
    } else if app.prompt_flags.rework_visible {
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::TextInput {
                title: "Rework Reason",
                body: "Why is rework needed?",
                input: &mut app.rework_prompt_input,
                hint: "Enter: Submit   Esc: Cancel",
            },
        );
    } else if app.cleanup_flow.reason_input_active {
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::TextInput {
                title: "Close Reason",
                body: "Reason to comment on the PR (optional):",
                input: &mut app.cleanup_reason_input,
                hint: "Enter: Submit   Esc: Cancel",
            },
        );
    } else if app.cleanup_flow.prompt_visible {
        draw_cleanup_prompt_overlay(buf, app, theme, area);
    } else if app.prompt_flags.no_plan_visible {
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "No Plan Available",
                body: "No implementation plan found.",
                options: &[("[p]", "Plan from branch"), ("[Esc]", "Stay blocked")],
            },
        );
    } else if let Some((_, ref error)) = app.branch_gone_prompt {
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Worktree Creation Failed",
                body: error,
                options: &[("[d]", "Delete work item"), ("[Esc]", "Dismiss")],
            },
        );
    } else if app.prompt_flags.stale_recovery_in_progress {
        // Recovery in flight - show spinner, no key options.
        let spinner = SPINNER_FRAMES[app.activities.spinner_tick % SPINNER_FRAMES.len()];
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Recovering Worktree",
                body: &format!("{spinner} Removing stale worktree and recreating..."),
                options: &[],
            },
        );
    } else if let Some(ref prompt) = app.stale_worktree_prompt {
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Stale Worktree",
                body: &prompt.error,
                options: &[
                    (
                        "",
                        "Uncommitted changes in the stale worktree will be lost.",
                    ),
                    ("[Enter]", "Recover worktree"),
                    ("[Esc]", "Dismiss"),
                ],
            },
        );
    } else if app.delete_flow.prompt_visible {
        draw_delete_prompt_overlay(buf, app, theme, area);
    }
}

/// Render the merge strategy prompt. Shows a precheck/merge spinner
/// when the background thread is in flight, or the strategy key
/// options with an optional "live re-check will run" hint below.
fn draw_merge_prompt_overlay(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    if app.merge_flow.in_progress {
        let spinner = SPINNER_FRAMES[app.activities.spinner_tick % SPINNER_FRAMES.len()];
        // While the live merge precheck is in flight we show a
        // "Refreshing remote state..." body so the user knows
        // that workbridge is re-verifying both the local
        // worktree AND the remote PR state (mergeable flag + CI
        // rollup) before shelling out to `gh pr merge`. As soon
        // as `poll_merge_precheck` swaps the helper slot's
        // payload from `PrMergePrecheck` to `PrMerge`, the next
        // render switches to the "Merging pull request..." body
        // without re-laying out the dialog (same `KeyChoice`
        // shape, same spinner).
        let body = if app.is_merge_precheck_phase() {
            format!("{spinner} Refreshing remote state... Please wait.")
        } else {
            format!("{spinner} Merging pull request... Please wait.")
        };
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Merge Strategy",
                body: &body,
                options: &[],
            },
        );
    } else {
        // Soft hint: if the cached repo state already hints that
        // the live precheck may block (worktree dirty / unpushed
        // / PR conflict / CI failing), append a dim reassurance
        // line so the user knows the authoritative check is
        // still ahead. Never refuses at the entry point based on
        // stale cache - see `App::merge_confirm_hint` for the
        // rationale.
        let base = "Merge PR?";
        let hint = app
            .merge_wi_id
            .as_ref()
            .and_then(|wi_id| app.merge_confirm_hint(wi_id));
        let body_owned = hint.map(|h| format!("{base}\n\n{h}"));
        let body_str: &str = body_owned.as_deref().unwrap_or(base);
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Merge Strategy",
                body: body_str,
                options: &[
                    ("[s]", "Squash (default)"),
                    ("[m]", "Merge"),
                    ("[p]", "Poll (mergequeue)"),
                    ("[Esc]", "Cancel"),
                ],
            },
        );
    }
}

/// Render the unlinked-PR cleanup prompt: shows an in-progress
/// spinner when the background thread is running, or the
/// "Close PR and delete branch?" confirmation with key options.
fn draw_cleanup_prompt_overlay(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    if app.is_user_action_in_flight(&UserActionKey::UnlinkedCleanup) {
        let pr_num = app.cleanup_progress_pr_number.unwrap_or(0);
        let spinner = SPINNER_FRAMES[app.activities.spinner_tick % SPINNER_FRAMES.len()];
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Close Unlinked PR",
                body: &format!("{spinner} Closing PR #{pr_num}... Please wait."),
                options: &[],
            },
        );
    } else {
        let pr_num = app.cleanup_unlinked_target.as_ref().map_or(0, |t| t.2);
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Close Unlinked PR",
                body: &format!("Close PR #{pr_num} and delete branch?"),
                options: &[
                    ("[Enter]", "Close with reason"),
                    ("[d]", "Close directly"),
                    ("[Esc]", "Cancel"),
                ],
            },
        );
    }
}

/// Render the work-item delete confirmation prompt. Shows an
/// in-progress spinner when the background delete thread is running,
/// or the "Delete '<title>'?" confirmation with a truncated title so
/// long names do not blow past the 60-column max dialog width.
fn draw_delete_prompt_overlay(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    if app.delete_flow.in_progress {
        let spinner = SPINNER_FRAMES[app.activities.spinner_tick % SPINNER_FRAMES.len()];
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Delete Work Item",
                body: &format!("{spinner} Removing worktree, branches, and open PRs..."),
                options: &[],
            },
        );
    } else {
        // draw_prompt_dialog's KeyChoice variant renders the body as
        // exactly one line. Long titles are elided so the dialog
        // still fits the 60-column max width. The body always warns
        // that uncommitted changes will be lost, because we no
        // longer shell out to `git status --porcelain` on the UI
        // thread to pre-detect dirty worktrees.
        let raw_title = app
            .delete_target_title
            .as_deref()
            .unwrap_or("this work item");
        let short_title = truncate_str(raw_title, 24);
        let body = format!("Delete '{short_title}' (uncommitted changes will be lost)?");
        draw_prompt_dialog(
            buf,
            theme,
            area,
            PromptDialogKind::KeyChoice {
                title: "Delete Work Item",
                body: &body,
                options: &[
                    ("[y]", "Delete (worktree, branch, PR)"),
                    ("[Esc]", "Cancel"),
                ],
            },
        );
    }
}
