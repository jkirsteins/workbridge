use rat_widget::scrolled::Scroll;
use rat_widget::text::TextStyle;
use rat_widget::text_input::TextInput;
use rat_widget::textarea::{TextArea, TextWrap};
use ratatui_core::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{StatefulWidget, Widget},
};
use ratatui_widgets::{
    barchart::{Bar, BarChart, BarGroup},
    block::Block,
    borders::{BorderType, Borders},
    clear::Clear,
    list::{List, ListItem, ListState},
    paragraph::{Paragraph, Wrap},
    scrollbar::{Scrollbar, ScrollbarOrientation, ScrollbarState},
    sparkline::Sparkline,
    tabs::Tabs,
};

use tui_term::widget::PseudoTerminal;
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, BOARD_COLUMNS, DisplayEntry, FirstRunGlobalHarnessModal, FocusPanel, GroupHeaderKind,
    RightPanelTab, SettingsListFocus, SettingsTab, Toast, UserActionKey, ViewMode, WorkItemContext,
    is_selectable,
};
use crate::click_targets::ClickKind;
use crate::config;
use crate::create_dialog::{CreateDialog, CreateDialogFocus};
use crate::layout;
use crate::metrics::{MetricsSnapshot, StuckItem, secs_to_day};
use crate::theme::Theme;
use crate::work_item::{
    BackendType, CheckStatus, MergeableState, PrState, ReviewDecision, SelectionState,
    WorkItemError, WorkItemKind, WorkItemStatus,
};

/// Braille-dot spinner frames for the activity indicator.
/// 10 frames at 200ms per tick = 2-second full rotation.
const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Render the entire UI: left panel (work item list) and right panel
/// (session output), plus optional context bar and status bar at the bottom.
///
/// Buffer-based rendering entry point. Called by the rat-salsa render
/// callback. All rendering uses Widget::render(area, buf) and
/// StatefulWidget::render(widget, area, buf, &mut state) directly.
///
/// `app` is `&mut` because stateful widgets owned by `App` (currently the
/// `rat-widget` text fields inside `CreateDialog`) need `&mut State` to
/// render.
pub fn draw_to_buffer(area: Rect, buf: &mut Buffer, app: &mut App, theme: &Theme) {
    // Clear stale click targets from the previous frame before any
    // render pushes. `handle_mouse` never runs during a draw, so
    // this `borrow_mut` never conflicts with a concurrent borrow.
    app.click_registry.borrow_mut().clear();

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
        if let Some(activity_msg) = app.current_activity() {
            let spinner = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
            let count_suffix = if app.activities.len() > 1 {
                format!(" (+{})", app.activities.len() - 1)
            } else {
                String::new()
            };
            let line = Line::from(vec![
                Span::styled(format!(" {} ", spinner), theme.style_activity_spinner()),
                Span::styled(activity_msg, theme.style_activity()),
                Span::styled(count_suffix, theme.style_text_muted()),
            ]);
            Paragraph::new(line).render(area, buf);
        } else if let Some(msg) = &app.status_message {
            let style = if app.shutting_down {
                theme.style_status_shutdown()
            } else {
                theme.style_status()
            };
            Paragraph::new(msg.as_str()).style(style).render(area, buf);
        }
    }

    // Settings overlay (rendered on top of everything).
    if app.show_settings {
        draw_settings_overlay(buf, app, theme, area);
    }

    // Prompt dialogs: blocking choice/input prompts rendered as centered modal
    // dialogs with dimmed backgrounds. Order matches the handle_key() intercept
    // chain (cleanup_reason_input_active before cleanup_prompt_visible).
    if app.confirm_merge {
        if app.merge_in_progress {
            let spinner = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
            // While the live working-tree precheck is in flight we
            // show a "Checking working tree..." body so the user knows
            // the merge has not yet started shelling out to GitHub. As
            // soon as `poll_merge_precheck` swaps the helper slot's
            // payload from `PrMergePrecheck` to `PrMerge`, the next
            // render switches to the "Merging pull request..." body
            // without re-laying out the dialog (same `KeyChoice`
            // shape, same spinner).
            let body = if app.is_merge_precheck_phase() {
                format!("{spinner} Checking working tree... Please wait.")
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
            draw_prompt_dialog(
                buf,
                theme,
                area,
                PromptDialogKind::KeyChoice {
                    title: "Merge Strategy",
                    body: "Merge PR?",
                    options: &[
                        ("[s]", "Squash (default)"),
                        ("[m]", "Merge"),
                        ("[p]", "Poll (mergequeue)"),
                        ("[Esc]", "Cancel"),
                    ],
                },
            );
        }
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
    } else if app.rework_prompt_visible {
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
    } else if app.cleanup_reason_input_active {
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
    } else if app.cleanup_prompt_visible {
        if app.is_user_action_in_flight(&UserActionKey::UnlinkedCleanup) {
            let pr_num = app.cleanup_progress_pr_number.unwrap_or(0);
            let spinner = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
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
            let pr_num = app
                .cleanup_unlinked_target
                .as_ref()
                .map(|t| t.2)
                .unwrap_or(0);
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
    } else if app.no_plan_prompt_visible {
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
    } else if app.stale_recovery_in_progress {
        // Recovery in flight - show spinner, no key options.
        let spinner = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
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
    } else if app.delete_prompt_visible {
        if app.delete_in_progress {
            let spinner = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
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
            // 60 - len("Delete '' (uncommitted changes will be lost)?") - borders - padding
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
    if app.global_drawer_open {
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

/// Draw the top-right transient toast stack. Each toast is a small
/// bordered block; multiple stack vertically with the newest on top.
/// Toasts whose rect would overflow the frame are skipped (rather
/// than clipped) so a small terminal degrades gracefully.
fn draw_toasts(buf: &mut Buffer, toasts: &[Toast], theme: &Theme, frame_area: Rect) {
    if toasts.is_empty() {
        return;
    }
    const TOAST_HEIGHT: u16 = 3; // bordered block + 1 content row
    const MAX_WIDTH: u16 = 60;
    const MIN_WIDTH: u16 = 16;
    const MARGIN_RIGHT: u16 = 2;
    const MARGIN_TOP: u16 = 1;

    // Newest toast first (visually on top of the stack).
    for (index, toast) in toasts.iter().rev().enumerate() {
        let index_u16 = index as u16;
        // `value.len() + 4` = text + two borders + two pad cells.
        let desired = (UnicodeWidthStr::width(toast.text.as_str()) as u16).saturating_add(4);
        let width = desired.clamp(MIN_WIDTH, MAX_WIDTH);

        // Frame too narrow for even the minimum toast width: bail.
        if width > frame_area.width.saturating_sub(MARGIN_RIGHT) {
            return;
        }

        let y = frame_area
            .y
            .saturating_add(MARGIN_TOP)
            .saturating_add(index_u16.saturating_mul(TOAST_HEIGHT));
        if y.saturating_add(TOAST_HEIGHT) > frame_area.y.saturating_add(frame_area.height) {
            // This toast and every further (older) toast would
            // overflow the bottom. Stop stacking.
            return;
        }

        let x = frame_area
            .x
            .saturating_add(frame_area.width)
            .saturating_sub(width)
            .saturating_sub(MARGIN_RIGHT);

        let rect = Rect {
            x,
            y,
            width,
            height: TOAST_HEIGHT,
        };

        // Clear under the toast so it occludes whatever was drawn
        // previously (status bar, context bar, etc.).
        Clear.render(rect, buf);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.style_text());
        let paragraph = Paragraph::new(Line::from(Span::styled(
            toast.text.clone(),
            theme.style_text(),
        )))
        .block(block);
        paragraph.render(rect, buf);
    }
}

/// Draw the first-run Ctrl+G harness picker modal. Centred, bordered,
/// lists each available harness with its single-letter keybinding. Esc
/// cancels. The key-handling lives in `event.rs`
/// (`handle_first_run_global_harness_modal`).
fn draw_first_run_global_harness_modal(
    buf: &mut Buffer,
    modal: &FirstRunGlobalHarnessModal,
    theme: &Theme,
    frame_area: Rect,
) {
    let body_line_count = 3 + modal.available_harnesses.len() + 2;
    let inner_height = body_line_count.min(12) as u16;
    let height = (inner_height + 2).min(frame_area.height);
    let width: u16 = 64.min(frame_area.width);
    let area = Rect {
        x: frame_area.x + frame_area.width.saturating_sub(width) / 2,
        y: frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    Clear.render(area, buf);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Pick a harness for the global assistant ")
        .border_style(theme.style_text());

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "Press the highlighted key to choose a harness for Ctrl+G.",
            theme.style_text(),
        )),
        Line::from(Span::styled(
            "The pick is saved to config.toml and can be changed via",
            theme.style_text(),
        )),
        Line::from(Span::styled(
            "`workbridge config set global-assistant-harness <name>`.",
            theme.style_text(),
        )),
        Line::from(""),
    ];
    for kind in &modal.available_harnesses {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  [{}]  ", kind.keybinding()),
                theme.style_text().add_modifier(Modifier::BOLD),
            ),
            Span::styled(kind.display_name(), theme.style_text()),
            Span::styled(
                format!("  ({} on PATH)", kind.binary_name()),
                theme.style_text().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Esc to cancel.",
        theme.style_text().add_modifier(Modifier::DIM),
    )));

    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });
    paragraph.render(area, buf);
}

/// Draw the view mode header: a segmented tab bar showing
/// List / Board / Dashboard with contextual keybinding hints on the right.
fn draw_view_mode_header(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let selected = match app.view_mode {
        ViewMode::FlatList => 0,
        ViewMode::Board => {
            if app.board_drill_down {
                0
            } else {
                1
            }
        }
        ViewMode::Dashboard => 2,
    };

    let tabs = Tabs::new(vec![" List ", " Board ", " Dashboard "])
        .select(selected)
        .style(theme.style_view_mode_tab())
        .highlight_style(theme.style_view_mode_tab_active())
        .divider("");

    // Keybinding hints (right-aligned).
    let hints = if app.view_mode == ViewMode::Board && !app.board_drill_down {
        "Tab: switch view | <-/->: columns | Shift+arrow: move item | Enter: drill down"
    } else if app.view_mode == ViewMode::Dashboard {
        "Tab: switch view | 1/2/3/4: 7d / 30d / 90d / 365d window"
    } else {
        "Tab: switch view"
    };

    // Split header: tabs on left, hints on right.
    let hints_width = hints.len() as u16 + 1; // +1 for trailing space
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(hints_width)])
        .split(area);

    tabs.render(cols[0], buf);
    Paragraph::new(Span::styled(hints, theme.style_view_mode_hints())).render(cols[1], buf);
}

/// Render the board (Kanban) view: four vertical columns for Backlog,
/// Planning, Implementing, and Review. Done items are hidden.
fn draw_board_view(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let bl = layout::compute_board(area.width);

    // Split into 4 columns: first 3 fixed width, last gets remainder.
    let constraints = [
        Constraint::Length(bl.column_width),
        Constraint::Length(bl.column_width),
        Constraint::Length(bl.column_width),
        Constraint::Min(0),
    ];
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    for (col_idx, status) in BOARD_COLUMNS.iter().enumerate() {
        let col_area = columns[col_idx];
        let is_selected_col = col_idx == app.board_cursor.column;

        let items = app.items_for_column(status);
        let count = items.len();

        // Column border style.
        let border_style = if is_selected_col {
            theme.style_board_column_focused()
        } else {
            theme.style_board_column_unfocused()
        };

        let col_title = format!(
            " {} ({}) ",
            match status {
                WorkItemStatus::Backlog => "Backlog",
                WorkItemStatus::Planning => "Planning",
                WorkItemStatus::Implementing => "Implementing",
                WorkItemStatus::Review => "Review",
                _ => "",
            },
            count
        );

        let block = Block::default()
            .title(col_title)
            .title_style(theme.style_board_column_header())
            .borders(Borders::ALL)
            .border_style(border_style);

        if items.is_empty() {
            let empty_text = Text::from(vec![Line::from(""), Line::from("  No items")]);
            let paragraph = Paragraph::new(empty_text)
                .block(block)
                .style(theme.style_text_muted());
            paragraph.render(col_area, buf);
            continue;
        }

        // Inner width for text wrapping (column width minus 2 for borders,
        // minus 2 for highlight symbol space).
        let inner_width = col_area.width.saturating_sub(2).saturating_sub(2) as usize;

        let list_items: Vec<ListItem> = items
            .iter()
            .enumerate()
            .map(|(row_idx, &wi_idx)| format_board_item(app, wi_idx, inner_width, theme, row_idx))
            .collect();

        let list = List::new(list_items)
            .block(block)
            .highlight_style(theme.style_board_item_highlight())
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if is_selected_col {
            state.select(app.board_cursor.row);
        }

        StatefulWidget::render(list, col_area, buf, &mut state);
    }
}

/// Render the global metrics Dashboard. All data comes from
/// `App.metrics_snapshot`, populated by the background aggregator thread.
/// This function performs zero file I/O - safe on the UI thread per
/// `docs/UI.md` "Blocking I/O Prohibition".
fn draw_dashboard_view(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .title(format!(
            " Dashboard (window: {}) ",
            app.dashboard_window.label()
        ))
        .border_style(theme.style_border_focused());
    let inner = outer.inner(area);
    outer.render(area, buf);

    let Some(snapshot) = app.metrics_snapshot.as_ref() else {
        Paragraph::new(
            "Computing metrics...\n\nThe background aggregator scans the activity log on first launch. Charts will appear shortly.",
        )
        .style(theme.style_view_mode_hints())
        .wrap(Wrap { trim: true })
        .render(inner, buf);
        return;
    };

    let today = secs_to_day(snapshot.computed_at_secs);
    let days = app.dashboard_window.days();
    let from_day = today - days + 1;

    // Vertical: KPI strip (2 rows: blank + KPIs) + main 2x2 grid.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    draw_dashboard_kpis(buf, snapshot, days, from_day, today, theme, chunks[0]);

    let row_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);
    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(row_chunks[0]);
    let bot_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(row_chunks[1]);

    draw_dashboard_done_vs_merged(buf, snapshot, from_day, today, theme, top_cols[0]);
    draw_dashboard_created(buf, snapshot, from_day, today, theme, top_cols[1]);
    draw_dashboard_backlog(buf, snapshot, from_day, today, theme, bot_cols[0]);
    draw_dashboard_stuck(buf, snapshot, theme, bot_cols[1]);
}

/// KPI strip: throughput, cycle time p50/p90, current backlog (delta from
/// window start), stuck count.
fn draw_dashboard_kpis(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    days: i64,
    from_day: i64,
    today: i64,
    theme: &Theme,
    area: Rect,
) {
    // Throughput: total Done events in the window.
    let throughput: u32 = snapshot
        .done_per_day
        .range(from_day..=today)
        .map(|(_, v)| *v)
        .sum();

    // Cycle time percentiles in days. cycle_times_secs is unsorted; clone
    // and sort locally so we don't mutate the shared snapshot.
    let mut sorted = snapshot.cycle_times_secs.clone();
    sorted.sort_unstable();
    let p50 = percentile_days(&sorted, 50);
    let p90 = percentile_days(&sorted, 90);

    // Backlog now and delta from window start.
    let backlog_now = snapshot
        .backlog_size_per_day
        .get(&today)
        .copied()
        .unwrap_or(0);
    let backlog_then = snapshot
        .backlog_size_per_day
        .get(&from_day)
        .copied()
        .unwrap_or(backlog_now);
    let delta = backlog_now as i32 - backlog_then as i32;
    let delta_str = if delta >= 0 {
        format!("+{delta}")
    } else {
        format!("{delta}")
    };

    let stuck = snapshot.stuck_items.len();

    let line = format!(
        "Throughput {throughput}/{days}d   Cycle p50 {p50}d   Cycle p90 {p90}d   Backlog now {backlog_now} ({delta_str})   Stuck {stuck}"
    );
    Paragraph::new(Text::from(vec![Line::from(""), Line::from(line)]))
        .style(theme.style_view_mode_hints())
        .render(area, buf);
}

/// Compute the p-th percentile of a sorted vector of seconds, returned in
/// whole days (rounded). Returns 0 if the input is empty.
fn percentile_days(sorted_secs: &[i64], pct: u32) -> i64 {
    if sorted_secs.is_empty() {
        return 0;
    }
    let idx = ((pct as f64 / 100.0) * (sorted_secs.len() - 1) as f64).round() as usize;
    let v = sorted_secs[idx.min(sorted_secs.len() - 1)];
    (v + 43_200) / 86_400 // round to nearest whole day
}

/// Color palette for the Dashboard charts. High-contrast pair for the
/// Done-vs-PRs-merged chart (green overlay line on magenta bar) and
/// distinct hues for the two single-series charts.
const DASHBOARD_DONE: Color = Color::LightGreen;
const DASHBOARD_MERGED: Color = Color::LightMagenta;
const DASHBOARD_BACKLOG: Color = Color::Yellow;
const DASHBOARD_CREATED: Color = Color::LightBlue;

/// Build a per-day series sliced to the current dashboard window.
/// Days with no events return zero so each `Vec<usize>` has exactly
/// `to_day - from_day + 1` entries.
fn slice_per_day(
    map: &std::collections::BTreeMap<i64, u32>,
    from_day: i64,
    to_day: i64,
) -> Vec<usize> {
    (from_day..=to_day)
        .map(|d| map.get(&d).copied().unwrap_or(0) as usize)
        .collect()
}

/// Done per day vs PRs merged per day, as a **grouped bar chart**. Each
/// day is one group of two adjacent bars (green = done, magenta =
/// merged) with a small gap separating it from the next day. This makes
/// each day's `(done, merged)` pair read as a local cluster and avoids
/// the problem of a continuous line chart where the eye has to track
/// two shapes at once.
///
/// Longer windows aggregate into coarser buckets so the chart stays
/// legible: 7d and 30d are daily, 90d groups into weeks, 365d groups
/// into ~monthly buckets of 30 days each. Bar width and group gap are
/// chosen per window so the full chart fills most of the panel without
/// overflowing.
fn draw_dashboard_done_vs_merged(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    from_day: i64,
    today: i64,
    _theme: &Theme,
    area: Rect,
) {
    let done = slice_per_day(&snapshot.done_per_day, from_day, today);
    let merged = slice_per_day(&snapshot.prs_merged_per_day, from_day, today);
    let days = (today - from_day + 1).max(1) as usize;

    // Aggregation choice per window. The bar/gap sizing is tuned so the
    // visible chart fills most of a ~90-char-wide inner area without
    // overflowing: `n_buckets * (2 * bar_width + group_gap)` stays under
    // ~90 for all four windows.
    let (bucket_size, bucket_noun, bar_width, group_gap) = if days <= 10 {
        (1_usize, "daily", 4_u16, 2_u16)
    } else if days <= 40 {
        (1_usize, "daily", 1_u16, 1_u16)
    } else if days <= 120 {
        (7_usize, "weekly", 3_u16, 1_u16)
    } else {
        (30_usize, "monthly", 3_u16, 1_u16)
    };

    let bucketed_done = bucket_sum(&done, bucket_size);
    let bucketed_merged = bucket_sum(&merged, bucket_size);
    let n_buckets = bucketed_done.len().max(bucketed_merged.len());

    let max_y: u64 = bucketed_done
        .iter()
        .chain(bucketed_merged.iter())
        .copied()
        .max()
        .unwrap_or(0) as u64;

    let title = Line::from(vec![
        Span::raw(" Done vs PRs merged   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_DONE)),
        Span::raw("done   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_MERGED)),
        Span::raw(format!("merged   ({bucket_noun} buckets) ")),
    ]);
    let days_label = today - from_day + 1;
    let block = Block::default().borders(Borders::ALL).title(title);

    let mut chart = BarChart::default()
        .block(block)
        .bar_width(bar_width)
        .bar_gap(0)
        .group_gap(group_gap)
        .max(max_y.max(1));

    for i in 0..n_buckets {
        let d = bucketed_done.get(i).copied().unwrap_or(0) as u64;
        let m = bucketed_merged.get(i).copied().unwrap_or(0) as u64;
        let group = BarGroup::default().bars(&[
            Bar::default()
                .value(d)
                .style(Style::default().fg(DASHBOARD_DONE)),
            Bar::default()
                .value(m)
                .style(Style::default().fg(DASHBOARD_MERGED)),
        ]);
        chart = chart.data(group);
    }

    chart.render(area, buf);
    draw_bottom_axis_labels(buf, area, days_label);
}

/// Bucket a per-day series by summing every `bucket_size` adjacent days.
/// If `bucket_size <= 1` the input is returned unchanged. Used by the
/// Done-vs-PRs-merged chart to compress long windows into readable
/// weekly / monthly groups.
fn bucket_sum(series: &[usize], bucket_size: usize) -> Vec<usize> {
    if bucket_size <= 1 || series.is_empty() {
        return series.to_vec();
    }
    let n_buckets = series.len().div_ceil(bucket_size);
    let mut out = vec![0_usize; n_buckets];
    for (i, &v) in series.iter().enumerate() {
        out[i / bucket_size] += v;
    }
    out
}

/// Resample a per-day series to exactly `target_width` columns for
/// `ratatui_widgets::Sparkline`, which renders one data point per column
/// (truncating the tail if `data.len() > width` and leaving trailing
/// columns blank if `data.len() < width`). We always want the chart to
/// fill its panel edge-to-edge, so the series is resized to the inner
/// width in both directions:
///
/// - **Wide windows (n > w)**: each output column takes the *max* of
///   the contiguous input range `[col*n/w, (col+1)*n/w)`. "Max" (not
///   "sum") keeps the visible amplitude meaningful for both count-
///   per-day and snapshot-per-day series, and matches the natural
///   reading of a sparkline peak.
/// - **Narrow windows (n <= w)**: each output column picks the value
///   at input index `col * n / w`, which stretches each data point
///   across a contiguous column block of width `w/n`. This matches
///   the block-center math used by `draw_bottom_axis_labels` so
///   labels sit exactly above the block they describe.
///
/// Empty input or `target_width == 0` returns an empty vector.
fn downsample_for_sparkline(series: &[usize], target_width: usize) -> Vec<u64> {
    if target_width == 0 || series.is_empty() {
        return Vec::new();
    }
    let n = series.len();
    if n >= target_width {
        // Downsample: each output column summarizes one or more days.
        (0..target_width)
            .map(|col| {
                let start = col * n / target_width;
                let end = ((col + 1) * n / target_width).max(start + 1);
                series[start..end.min(n)].iter().copied().max().unwrap_or(0) as u64
            })
            .collect()
    } else {
        // Upsample (stretch): replicate each input across `w/n` columns.
        (0..target_width)
            .map(|col| series[(col * n / target_width).min(n - 1)] as u64)
            .collect()
    }
}

#[cfg(test)]
mod downsample_tests {
    use super::downsample_for_sparkline;

    #[test]
    fn empty_input_returns_empty() {
        let out = downsample_for_sparkline(&[], 10);
        assert!(out.is_empty());
    }

    #[test]
    fn zero_target_width_returns_empty() {
        let out = downsample_for_sparkline(&[1, 2, 3], 0);
        assert!(out.is_empty());
    }

    #[test]
    fn identity_when_n_equals_target_width() {
        // n == w falls into the `n >= target_width` branch. Each output
        // column covers exactly one input index, so values round-trip
        // unchanged (cast to u64).
        let input = [0usize, 3, 1, 7, 2];
        let out = downsample_for_sparkline(&input, input.len());
        assert_eq!(out, vec![0u64, 3, 1, 7, 2]);
    }

    #[test]
    fn strict_downsample_takes_block_max() {
        // n=12, w=4: each output column summarizes n/w=3 input values
        // via max. Block boundaries are [0..3), [3..6), [6..9), [9..12).
        let input = [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let out = downsample_for_sparkline(&input, 4);
        assert_eq!(out, vec![3u64, 6, 9, 12]);
    }

    #[test]
    fn strict_downsample_respects_non_uniform_blocks() {
        // n=5, w=3: floor math gives blocks [0..1), [1..3), [3..5) with
        // the `(start+1).max(end)` guard keeping the first block
        // non-empty. Expected maxes: 10, max(3,7)=7, max(2,9)=9.
        let input = [10usize, 3, 7, 2, 9];
        let out = downsample_for_sparkline(&input, 3);
        assert_eq!(out, vec![10u64, 7, 9]);
    }

    #[test]
    fn strict_upsample_replicates_each_input() {
        // n=3, w=9: each input value should appear in ~w/n=3 adjacent
        // output columns via `series[col * n / w]` block-stretch math.
        let input = [5usize, 8, 2];
        let out = downsample_for_sparkline(&input, 9);
        assert_eq!(out, vec![5u64, 5, 5, 8, 8, 8, 2, 2, 2]);
    }

    #[test]
    fn upsample_never_indexes_out_of_bounds() {
        // For the last output column, (col * n / w) can equal n-1 via
        // floor math; the `.min(n - 1)` clamp must keep the read in
        // bounds. This test exercises a width that is not a clean
        // multiple of n.
        let input = [1usize, 2];
        let out = downsample_for_sparkline(&input, 5);
        // col * 2 / 5 -> 0, 0, 0, 1, 1
        assert_eq!(out, vec![1u64, 1, 1, 2, 2]);
    }
}

/// Overlay x-axis labels on the bottom border of a chart block.
///
/// A chart for a `days`-day window plots `days` data points where the
/// leftmost point is `(days - 1)` days ago and the rightmost point is
/// today. The function picks four data indices `[0, days/3, 2*days/3,
/// days-1]`, derives the day-ago label for each, and places the label
/// text centered on the corresponding data block's center column. This
/// matches the `downsample_for_sparkline` block-based stretching
/// (`data_idx = col * n / w`), so every intermediate label sits
/// exactly above the block whose value it represents - no straddling
/// between neighboring days.
///
/// Writes directly into the buffer row at `area.bottom() - 1`,
/// overwriting the border `─` characters. Must be called after the
/// block is rendered so the border is already in place for the labels
/// to sit on top of.
fn draw_bottom_axis_labels(buf: &mut Buffer, area: Rect, days: i64) {
    if area.width < 20 || area.height == 0 || days < 1 {
        return;
    }
    let y = area.bottom() - 1;
    let x_start = area.left() + 1; // skip left corner
    let x_end = area.right() - 1; // exclusive, skip right corner
    if x_end <= x_start {
        return;
    }
    let inner_width = (x_end - x_start) as i64;

    let n = days.max(1); // number of data points = window size
    let max_ago = n - 1; // oldest day-offset shown in the chart

    // Pick four data indices to label: first, 1/3, 2/3, last.
    // `n.saturating_sub(1)` for the last entry avoids overflow for tiny
    // windows; callers always pass `n >= 7` but the guard is cheap.
    let label_indices: [i64; 4] = [0, n / 3, (2 * n) / 3, n - 1];

    for (label_idx, &data_idx) in label_indices.iter().enumerate() {
        // Day-ago value at this data index. Data index 0 = oldest,
        // data index `n-1` = today (0 days ago).
        let days_ago = max_ago - data_idx;
        let label_text = if days_ago == 0 {
            " now ".to_string()
        } else {
            format!(" -{days_ago}d ")
        };
        let label_len = label_text.chars().count() as i64;

        // Block-based widget mapping gives data point `i` the column
        // range `[i*w/n, (i+1)*w/n)`. The center column of that range
        // is `((2i+1) * w) / (2n)`.
        let block_center_rel = ((2 * data_idx + 1) * inner_width) / (2 * n);

        // Anchor the label: left-aligned to the chart start for the
        // leftmost label, right-aligned for `now`, center-aligned on
        // the block center for the intermediate labels.
        let start_x = match label_idx {
            0 => x_start as i64,
            3 => x_end as i64 - label_len,
            _ => (x_start as i64 + block_center_rel) - label_len / 2,
        };
        let clamped = start_x.max(x_start as i64).min(x_end as i64 - label_len);
        for (i, ch) in label_text.chars().enumerate() {
            let cx = (clamped + i as i64) as u16;
            if cx >= x_end {
                break;
            }
            buf[(cx, y)].set_symbol(ch.encode_utf8(&mut [0; 4]));
        }
    }
}

/// Workitems created per day, rendered as a single-series filled
/// sparkline using `ratatui_widgets::sparkline::Sparkline`. The title
/// carries the y-axis max so the implied scale is readable without an
/// axis widget.
fn draw_dashboard_created(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    from_day: i64,
    today: i64,
    _theme: &Theme,
    area: Rect,
) {
    let series = slice_per_day(&snapshot.created_per_day, from_day, today);
    let max_v = series.iter().copied().max().unwrap_or(0);
    let title = Line::from(vec![
        Span::raw(" Created per day   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_CREATED)),
        Span::raw(format!("max {max_v}/day ")),
    ]);
    let days = today - from_day + 1;
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    block.render(area, buf);

    // Downsample to the inner width so long windows (90d/365d) do not
    // get truncated at the tail by the built-in `Sparkline`, which
    // renders exactly one column per data point.
    let data = downsample_for_sparkline(&series, inner.width as usize);
    Sparkline::default()
        .data(&data)
        .style(Style::default().fg(DASHBOARD_CREATED))
        .render(inner, buf);
    draw_bottom_axis_labels(buf, area, days);
}

/// Backlog size over time, rendered as a single-series filled sparkline
/// using `ratatui_widgets::sparkline::Sparkline`. Reconstructed by the
/// aggregator from `stage_change` events; see `metrics::backlog_intervals`.
/// Title shows current and peak values because a flat line at zero is
/// hard to read otherwise.
fn draw_dashboard_backlog(
    buf: &mut Buffer,
    snapshot: &MetricsSnapshot,
    from_day: i64,
    today: i64,
    _theme: &Theme,
    area: Rect,
) {
    let series = slice_per_day(&snapshot.backlog_size_per_day, from_day, today);
    let peak = series.iter().copied().max().unwrap_or(0);
    let now_value = series.last().copied().unwrap_or(0);
    let title = Line::from(vec![
        Span::raw(" Backlog size over time   "),
        Span::styled("█ ", Style::default().fg(DASHBOARD_BACKLOG)),
        Span::raw(format!("now {now_value} / peak {peak} ")),
    ]);
    let days = today - from_day + 1;
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    block.render(area, buf);

    // Downsample to the inner width so long windows (90d/365d) do not
    // get truncated at the tail by the built-in `Sparkline`, which
    // renders exactly one column per data point.
    let data = downsample_for_sparkline(&series, inner.width as usize);
    Sparkline::default()
        .data(&data)
        .style(Style::default().fg(DASHBOARD_BACKLOG))
        .render(inner, buf);
    draw_bottom_axis_labels(buf, area, days);
}

/// Stuck items list: items currently in Blocked or Review beyond their
/// dwell threshold. Threshold values come from the metrics module.
fn draw_dashboard_stuck(buf: &mut Buffer, snapshot: &MetricsSnapshot, theme: &Theme, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Stuck items ");
    let inner = block.inner(area);
    block.render(area, buf);

    if snapshot.stuck_items.is_empty() {
        Paragraph::new("None - everything in Review/Blocked is moving.")
            .style(theme.style_view_mode_hints())
            .wrap(Wrap { trim: true })
            .render(inner, buf);
        return;
    }

    let lines: Vec<Line<'_>> = snapshot
        .stuck_items
        .iter()
        .take(inner.height as usize)
        .map(format_stuck_item_line)
        .collect();
    Paragraph::new(Text::from(lines)).render(inner, buf);
}

fn format_stuck_item_line(item: &StuckItem) -> Line<'static> {
    let days = item.stuck_for_secs / 86_400;
    let hours = (item.stuck_for_secs % 86_400) / 3600;
    let dwell = if days > 0 {
        format!("{days}d{hours}h")
    } else {
        format!("{hours}h")
    };
    let status_label = match item.status {
        WorkItemStatus::Review => "Review ",
        WorkItemStatus::Blocked => "Blocked",
        _ => "       ",
    };
    Line::from(format!("  {status_label}  {dwell:>6}  {}", item.wi_id))
}

/// Format a work item for display inside a board column.
/// Uses wrapping (never truncation) to avoid clipping.
fn format_board_item<'a>(
    app: &App,
    wi_idx: usize,
    max_width: usize,
    theme: &Theme,
    _row_idx: usize,
) -> ListItem<'a> {
    let Some(wi) = app.work_items.get(wi_idx) else {
        return ListItem::new(Line::from("<invalid>"));
    };

    let mut lines: Vec<Line<'a>> = Vec::new();

    // Title line(s) -- wrap, never truncate.
    let title_prefix = if wi.status == WorkItemStatus::Blocked {
        "[BK] "
    } else if wi.status == WorkItemStatus::Mergequeue {
        "[MQ] "
    } else {
        ""
    };
    let title_text = format!("{title_prefix}{}", wi.title);
    let wrapped = wrap_text(&title_text, max_width);
    for (i, wl) in wrapped.into_iter().enumerate() {
        let style = if wi.status == WorkItemStatus::Blocked {
            theme.style_stage_badge(&WorkItemStatus::Blocked)
        } else if wi.status == WorkItemStatus::Mergequeue {
            theme.style_stage_badge(&WorkItemStatus::Mergequeue)
        } else if i == 0 {
            theme.style_text()
        } else {
            theme.style_text_muted()
        };
        lines.push(Line::from(Span::styled(wl, style)));
    }

    // Status indicators on a second line (PR badge, session status).
    let mut indicators: Vec<Span<'a>> = Vec::new();

    // Session activity indicator.
    let has_session = app.session_key_for(&wi.id).is_some();
    let is_working = app.agent_working.contains(&wi.id);
    if is_working {
        let frame = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
        indicators.push(Span::styled(
            frame.to_string(),
            theme.style_badge_session_working(),
        ));
    } else if has_session {
        indicators.push(Span::styled(
            "\u{25CF}".to_string(),
            theme.style_badge_session_idle(),
        ));
    }

    let first_pr = wi.repo_associations.iter().find_map(|a| a.pr.as_ref());
    if let Some(pr) = first_pr {
        // Add space separator if session indicator is already present.
        if !indicators.is_empty() {
            indicators.push(Span::raw(" "));
        }
        let pr_text = format!("PR#{}", pr.number);
        indicators.push(Span::styled(pr_text, theme.style_badge_pr()));
        match &pr.checks {
            CheckStatus::Passing => {
                indicators.push(Span::styled(" ok", theme.style_badge_ci_pass()));
            }
            CheckStatus::Failing => {
                indicators.push(Span::styled(" fail", theme.style_badge_ci_fail()));
            }
            CheckStatus::Pending => {
                indicators.push(Span::styled(" ...", theme.style_badge_ci_pending()));
            }
            CheckStatus::None | CheckStatus::Unknown => {}
        }
        if matches!(pr.mergeable, MergeableState::Conflicting) {
            indicators.push(Span::styled(" !merge", theme.style_badge_merge_conflict()));
        }
    }
    if !indicators.is_empty() {
        lines.push(Line::from(indicators));
    }

    ListItem::new(Text::from(lines))
}

/// Find the index of the group header that applies to the item at `offset`.
/// Walks backwards from `offset` (clamped to the last valid index) and returns
/// the first `GroupHeader` encountered. Returns `None` when there are no
/// headers before or at `offset` (e.g., board drill-down mode or offset 0 with
/// no leading header).
fn find_current_group_header(display_list: &[DisplayEntry], offset: usize) -> Option<usize> {
    if display_list.is_empty() {
        return None;
    }
    let start = offset.min(display_list.len() - 1);
    for i in (0..=start).rev() {
        if matches!(display_list[i], DisplayEntry::GroupHeader { .. }) {
            return Some(i);
        }
    }
    None
}

/// Predict the `ListState::offset()` that ratatui's `List` widget will choose
/// when rendered with the given per-item row heights, previous offset,
/// selection, and available body height.
///
/// This mirrors `ratatui_widgets::list::List::get_items_bounds` for the
/// default `scroll_padding = 0` case, and also mirrors the `ListState::select`
/// side effect that resets `offset` to 0 when the selection is cleared
/// (see `ratatui_widgets::list::state::ListState::select`).
///
/// **No longer used in the render path.** With the decoupled-viewport
/// model (`App::list_scroll_offset` is authoritative, not derived), the
/// renderer no longer needs a ratatui predictor - the offset lives on
/// `App` and is mutated only by mouse wheel events and the
/// recenter-on-selection pass. The predictor is retained under
/// `#[cfg(test)]` so the parallel-render tests in `mod sticky_header_tests`
/// still document what ratatui's own math does, which is useful for
/// sanity-checking the new `recenter_offset` helper against the old
/// auto-scroll behavior.
#[cfg(test)]
fn predict_list_offset(
    item_heights: &[usize],
    prev_offset: usize,
    selected: Option<usize>,
    max_height: usize,
) -> usize {
    if item_heights.is_empty() {
        return 0;
    }

    let last_valid_index = item_heights.len() - 1;
    // Mirror `ListState::select(None)`'s side effect: clearing the
    // selection also resets the offset to 0. This matches what ratatui
    // will actually render when the production call site invokes
    // `state.select(None)` after `with_offset`.
    let effective_prev_offset = match selected {
        Some(_) => prev_offset.min(last_valid_index),
        None => 0,
    };
    if max_height == 0 {
        return effective_prev_offset;
    }
    let mut first_visible_index = effective_prev_offset;
    let mut last_visible_index = first_visible_index;
    let mut height_from_offset: usize = 0;

    // Walk forward from the current offset, summing heights until the next
    // item would overflow the viewport. After this loop `last_visible_index`
    // is the exclusive end of the visible range (i.e. one past the last
    // fully-visible item).
    for h in item_heights.iter().skip(first_visible_index) {
        if height_from_offset + h > max_height {
            break;
        }
        height_from_offset += h;
        last_visible_index += 1;
    }

    // With `scroll_padding = 0` the index we must keep on screen is just the
    // selected item (falling back to the offset when nothing is selected).
    let index_to_display = match selected {
        Some(s) => s.min(last_valid_index),
        None => first_visible_index,
    };

    // If the selected item is past the current viewport, scroll down: add
    // items to the tail and drop items from the head until the selected
    // index is visible.
    while index_to_display >= last_visible_index {
        height_from_offset = height_from_offset.saturating_add(item_heights[last_visible_index]);
        last_visible_index += 1;
        while height_from_offset > max_height && first_visible_index < last_visible_index {
            height_from_offset =
                height_from_offset.saturating_sub(item_heights[first_visible_index]);
            first_visible_index += 1;
        }
    }

    // If the selected item is before the current viewport, scroll up: add
    // items to the head and drop items from the tail.
    while index_to_display < first_visible_index {
        first_visible_index -= 1;
        height_from_offset = height_from_offset.saturating_add(item_heights[first_visible_index]);
        while height_from_offset > max_height && last_visible_index > first_visible_index + 1 {
            last_visible_index -= 1;
            height_from_offset =
                height_from_offset.saturating_sub(item_heights[last_visible_index]);
        }
    }

    first_visible_index
}

fn draw_work_item_list(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    // When the settings overlay is open, dim background panels so the
    // overlay is the clear focal point.
    let border_style = if app.show_settings {
        theme.style_border_unfocused()
    } else if app.focus == FocusPanel::Left {
        theme.style_border_focused()
    } else {
        theme.style_border_unfocused()
    };

    // When drilling down from board view, show the stage name in the title.
    let title = if let Some(ref stage) = app.board_drill_stage {
        let stage_name = match stage {
            WorkItemStatus::Backlog => "Backlog",
            WorkItemStatus::Planning => "Planning",
            WorkItemStatus::Implementing => "Implementing",
            WorkItemStatus::Blocked => "Blocked",
            WorkItemStatus::Review => "Review",
            WorkItemStatus::Mergequeue => "Mergequeue",
            WorkItemStatus::Done => "Done",
        };
        let count = app
            .display_list
            .iter()
            .filter(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .count();
        format!(" {stage_name} ({count}) ")
    } else {
        format!(" Work Items ({}) ", app.work_items.len())
    };

    let block = Block::default()
        .title(title)
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(border_style);

    if app.display_list.is_empty() {
        let text = if app.board_drill_stage.is_some() {
            Text::from(vec![
                Line::from(""),
                Line::from("  No items."),
                Line::from(""),
                Line::from("  Press Ctrl+]"),
                Line::from("  to return."),
            ])
        } else {
            Text::from(vec![
                Line::from(""),
                Line::from("  No work items."),
                Line::from(""),
                Line::from("  Ctrl+N: quick start"),
                Line::from("  Ctrl+B: backlog ticket"),
            ])
        };
        let paragraph = Paragraph::new(text)
            .block(block)
            .style(theme.style_text_muted());
        paragraph.render(area, buf);
        return;
    }

    // Available width inside the block borders. Each item prepends its own
    // 2-char left margin (selection caret or activity indicator).
    let inner_width = area.width.saturating_sub(2) as usize;

    // Build list items. When a row is the selected row, the row's
    // background is painted with `style_tab_highlight_bg` directly on
    // the ListItem so the `List` widget itself no longer owns the
    // selection highlight. `ListState::select` is deliberately NOT
    // called below - decoupling the viewport from the selection means
    // the renderer must not let ratatui's `get_items_bounds` force an
    // auto-scroll. The highlight is a styling concern, the viewport a
    // state concern, and the two must stay independent for the wheel
    // scroll to work without snapping back.
    let items: Vec<ListItem> = app
        .display_list
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_selected = app.selected_item == Some(i);
            let item = match entry {
                DisplayEntry::GroupHeader { label, count, kind } => {
                    let text = format!("{label} ({count})");
                    let style = match kind {
                        GroupHeaderKind::Blocked => theme.style_group_header_blocked(),
                        GroupHeaderKind::Normal => theme.style_group_header(),
                    };
                    ListItem::new(Line::from(vec![Span::raw("  "), Span::styled(text, style)]))
                }
                DisplayEntry::UnlinkedItem(idx) => {
                    format_unlinked_item(app, *idx, inner_width, theme, is_selected)
                }
                DisplayEntry::ReviewRequestItem(idx) => {
                    format_review_request_item(app, *idx, inner_width, theme, is_selected)
                }
                DisplayEntry::WorkItemEntry(idx) => {
                    format_work_item_entry(app, *idx, inner_width, theme, is_selected)
                }
            };
            if is_selected {
                item.style(theme.style_tab_highlight_bg())
            } else {
                item
            }
        })
        .collect();

    // Pre-compute per-item row heights for scrollbar calculations.
    let item_heights: Vec<usize> = items.iter().map(ListItem::height).collect();
    let total_rows: usize = item_heights.iter().sum();

    // Draw the block (borders + title) directly into `area` so we can
    // split the inner area ourselves and hand only a sub-rect to the
    // `List` widget. This lets us reserve a dedicated 1-row slot at the
    // top of the inner area for the sticky group header, guaranteeing
    // the selected work item is never painted over by the sticky row.
    Widget::render(block, area, buf);
    let inner = area.inner(Margin::new(1, 1));

    // The viewport offset (`list_scroll_offset`) is authoritative here:
    // it is mutated only by the wheel-scroll handler, by the recenter
    // pass below (triggered by keyboard selection changes), and by the
    // clamp on list shrink. The renderer reads it directly; no
    // predictor is consulted for the body offset.
    let selected = app.selected_item;
    let drill_down = app.board_drill_stage.is_some();

    // Resolve the pending recenter request first, against a tentative
    // body height that equals `inner.height` (no sticky slot reserved
    // yet). The sticky decision depends on the resolved offset, so we
    // cannot postpone the recenter past it - otherwise the first frame
    // after a keyboard navigation would show the old sticky decision
    // and flicker for one frame.
    let want_recenter = app.recenter_viewport_on_selection.take();
    let tentative_offset = if want_recenter {
        match selected {
            Some(idx) if idx < item_heights.len() => {
                recenter_offset(&item_heights, idx, inner.height as usize)
            }
            _ => app.list_scroll_offset.get(),
        }
    } else {
        app.list_scroll_offset.get()
    };
    let tentative_offset = tentative_offset.min(item_heights.len().saturating_sub(1).max(0));

    // Decide whether to reserve a sticky-header slot this frame. The
    // decision is made against the tentative offset, so a recenter
    // that lands deep in the list still reserves the slot on the same
    // frame.
    let (body_area, sticky_slot) = if drill_down || inner.height < 2 {
        (inner, None)
    } else {
        let sticky_would_fire = find_current_group_header(&app.display_list, tentative_offset)
            .is_some_and(|h| h < tentative_offset);
        if sticky_would_fire {
            let body_height = inner.height - 1;
            let slot = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            };
            let body = Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: body_height,
            };
            (body, Some(slot))
        } else {
            (inner, None)
        }
    };

    let body_height = body_area.height as usize;

    // Reconcile the tentative offset with the final body height. If
    // the sticky slot shrank the body, the recenter may need to shift
    // by one item; if the list shrank below the viewport, the offset
    // clamps to `max_item_offset`.
    let max_item_offset = compute_max_item_offset(&item_heights, body_height);
    let resolved_offset = if want_recenter {
        match selected {
            Some(idx) if idx < item_heights.len() => {
                recenter_offset(&item_heights, idx, body_height).min(max_item_offset)
            }
            _ => tentative_offset.min(max_item_offset),
        }
    } else {
        tentative_offset.min(max_item_offset)
    };
    app.list_scroll_offset.set(resolved_offset);
    app.list_max_item_offset.set(max_item_offset);
    app.work_item_list_body.set(Some(body_area));

    // Per-row click targets: push a `ClickTarget::WorkItemRow` for
    // each row that is at least partially visible so `handle_mouse`
    // can map a left-click at `(x, y)` back to a display-list index
    // without redoing the layout math. Offscreen rows are skipped -
    // the registry hit-test is a linear scan, so keeping it small
    // keeps the mouse path cheap.
    {
        let mut registry = app.click_registry.borrow_mut();
        let mut y = body_area.y;
        let end_y = body_area.y.saturating_add(body_area.height);
        for (i, h) in item_heights.iter().enumerate().skip(resolved_offset) {
            if y >= end_y {
                break;
            }
            let row_height = (*h as u16).min(end_y - y);
            if row_height == 0 {
                break;
            }
            // Only push selectable rows so clicks on group headers
            // (non-selectable) do not accidentally steal a row-click
            // dispatch. The mouse handler falls through to the
            // GlobalDrawer / RightPanel / WorkItemList arms otherwise.
            if is_selectable(&app.display_list[i]) {
                registry.push_work_item_row(
                    Rect {
                        x: body_area.x,
                        y,
                        width: body_area.width,
                        height: row_height,
                    },
                    i,
                );
            }
            y = y.saturating_add(row_height);
        }
    }

    let list = List::new(items);
    let mut state = ListState::default().with_offset(resolved_offset);

    StatefulWidget::render(list, body_area, buf, &mut state);

    // --- Sticky group header ---
    // Paint the reserved slot (if any) using the authoritative offset
    // (the one we wrote to `list_scroll_offset` above). Because the
    // slot was reserved structurally via `Layout`, the `List` body
    // never overlaps it.
    if !drill_down {
        let header_needed = find_current_group_header(&app.display_list, resolved_offset)
            .filter(|&h| h < resolved_offset);

        match (sticky_slot, header_needed) {
            (Some(slot), Some(header_idx)) => {
                if let DisplayEntry::GroupHeader {
                    ref label,
                    count,
                    ref kind,
                } = app.display_list[header_idx]
                {
                    let text = format!("{label} ({count})");
                    let style = match kind {
                        GroupHeaderKind::Blocked => theme.style_sticky_header_blocked(),
                        GroupHeaderKind::Normal => theme.style_sticky_header(),
                    };
                    // Fill the entire row with the sticky background so it
                    // visually separates from the highlighted item below.
                    let bg_style = Style::default().bg(theme.sticky_header_bg);
                    let line = Line::from(vec![
                        Span::styled("  ", bg_style),
                        Span::styled(text, style),
                    ]);
                    Paragraph::new(line).style(bg_style).render(slot, buf);
                }
            }
            (Some(_), None) | (None, Some(_)) => {
                // After the tentative-offset reconciliation above, the
                // sticky decision and the post-render offset should
                // agree. The only legitimate drift is a one-item
                // sticky decision flip caused by the recenter shrinking
                // the body by 1 row, in which case `header_needed`
                // still matches `sticky_slot`. Treat any remaining
                // mismatch as a one-frame visual glitch rather than a
                // hard assertion - the next frame will reconcile.
            }
            (None, None) => {}
        }
    }

    // Scrollbar - only when content overflows the list body. We use
    // `body_area.height` (not `inner.height`) so the scrollbar track
    // matches whichever area the `List` was rendered into.
    let max_row_offset = total_rows.saturating_sub(body_height);
    if total_rows > body_height || resolved_offset > 0 {
        // Convert the item-based offset to a row-based offset so the
        // scrollbar thumb position matches the actual viewport scroll.
        let row_offset: usize = item_heights.iter().take(resolved_offset).sum();

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_style(theme.style_scrollbar_thumb())
            .track_style(theme.style_scrollbar_track());

        // Ratatui's `Scrollbar::part_lengths` requires
        // `position == content_length - 1` for the thumb's lower edge to
        // reach the bottom of the track. The number of distinct row-granular
        // scroll positions is `max_row_offset + 1`, so we size
        // `content_length` accordingly and clamp `row_offset` to guard
        // against variable-height edge cases where the list may reserve
        // blank rows below the last item. `body_height` (not the block's
        // full inner height) is the correct viewport size because the
        // sticky slot reservation shrinks the area the `List` renders into.
        let content_length = max_row_offset + 1;
        let position = row_offset.min(max_row_offset);
        let mut scrollbar_state = ScrollbarState::new(content_length)
            .viewport_content_length(body_height)
            .position(position);

        let scrollbar_area = Rect {
            x: area.x,
            y: body_area.y,
            width: area.width,
            height: body_area.height,
        };
        StatefulWidget::render(scrollbar, scrollbar_area, buf, &mut scrollbar_state);
    }

    // Offscreen-selection marker: when the selected item is outside
    // the visible viewport, paint a single distinct-coloured cell in
    // the scrollbar column at the y-coordinate that corresponds to the
    // selection's position within the full list. This gives the user a
    // visual cue of where their keyboard selection sits relative to
    // the mouse-scrolled viewport. When the selection is visible, no
    // marker is painted - the normal thumb is enough.
    if let Some(idx) = selected
        && idx < item_heights.len()
        && body_area.width > 0
        && body_area.height > 0
    {
        let row_of_selection: usize = item_heights.iter().take(idx).sum();
        let sel_end = row_of_selection + item_heights[idx];
        let visible_start: usize = item_heights.iter().take(resolved_offset).sum();
        let visible_end = visible_start + body_height;
        let onscreen = row_of_selection < visible_end && sel_end > visible_start;
        if !onscreen && total_rows > 0 {
            // Map the selection's top row to a y within the body.
            // `max_row_offset == 0` means the whole list fits (no
            // scrolling possible) - guarded above via `!onscreen` +
            // `total_rows > 0` but keep the divisor non-zero.
            let denom = max_row_offset.max(1);
            let marker_row =
                (row_of_selection.saturating_mul(body_area.height as usize - 1)) / denom;
            let marker_row = marker_row.min(body_area.height as usize - 1);
            let marker_x = area.x + area.width - 1;
            let marker_y = body_area.y + marker_row as u16;
            if marker_x < buf.area.x + buf.area.width && marker_y < buf.area.y + buf.area.height {
                let cell = &mut buf[(marker_x, marker_y)];
                cell.set_symbol("\u{2588}");
                cell.set_style(theme.style_scrollbar_selection_marker());
            }
        }
    }
}

/// Compute the largest item-level viewport offset such that all
/// remaining items fit within `body_height` rows.
///
/// This is the upper bound the wheel-scroll handler uses to prevent
/// scrolling past the end of the list. Walks from the tail backward,
/// accumulating heights, and returns the first index where the sum
/// exceeds the viewport - that index (plus one) is the smallest offset
/// that still fits everything.
fn compute_max_item_offset(item_heights: &[usize], body_height: usize) -> usize {
    let total: usize = item_heights.iter().sum();
    if total <= body_height || item_heights.is_empty() {
        return 0;
    }
    let mut acc = 0usize;
    for (i, h) in item_heights.iter().enumerate().rev() {
        acc = acc.saturating_add(*h);
        if acc > body_height {
            return i + 1;
        }
    }
    0
}

/// Compute the item-level viewport offset that centers `selected` in a
/// `body_height`-row viewport, clamped to `[0, max_item_offset]`.
///
/// The offset is item-aligned (not row-aligned): the viewport starts
/// at an item boundary, never mid-item, so partial-item rows at the
/// top of the viewport are never rendered. The centering target is the
/// row directly above the selection that puts the selection's middle
/// row at the body's middle row, clamped to zero and to the tail.
fn recenter_offset(item_heights: &[usize], selected: usize, body_height: usize) -> usize {
    if item_heights.is_empty() || body_height == 0 || selected >= item_heights.len() {
        return 0;
    }
    let max_offset = compute_max_item_offset(item_heights, body_height);
    // Row at which the selected item begins in the full list.
    let sel_row: usize = item_heights.iter().take(selected).sum();
    let sel_height = item_heights[selected];
    // Target: selected item vertically centred. `sel_center_row` is
    // the absolute row of the selection's midpoint; to centre the
    // viewport on it we start at `sel_center_row - body_height/2`.
    let sel_center = sel_row + sel_height / 2;
    let target_row = sel_center.saturating_sub(body_height / 2);

    // Walk forward, adopting the largest item boundary `j` whose
    // cumulative row count is <= `target_row`. This keeps the offset
    // item-aligned.
    let mut cumulative = 0usize;
    let mut chosen = 0usize;
    for (j, h) in item_heights.iter().enumerate() {
        if cumulative <= target_row {
            chosen = j;
        } else {
            break;
        }
        cumulative = cumulative.saturating_add(*h);
    }
    chosen.min(max_offset)
}

/// Format a review-requested PR entry for the left panel list.
fn format_review_request_item<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
    is_selected: bool,
) -> ListItem<'a> {
    let margin = if is_selected { "> " } else { "  " };
    let content_width = max_width.saturating_sub(2);

    let Some(rr) = app.review_requested_prs.get(idx) else {
        return ListItem::new(Line::from(format!("{margin}R <invalid>")));
    };

    // Right-column text: PR badge + optional draft marker + optional
    // reviewer badge. Assembled into a single String first so the wrap
    // helper can reserve enough width on the first line for the whole
    // stack - the spans are rebuilt below once the title is wrapped.
    let pr_badge = format!("PR#{}", rr.pr.number);
    let draft_suffix = if rr.pr.is_draft { " draft" } else { "" };
    let reviewer_badge = rr.reviewer_badge(app.current_user_login.as_deref());
    let reviewer_suffix = reviewer_badge
        .as_deref()
        .map(|s| format!(" {s}"))
        .unwrap_or_default();
    let right = format!("{pr_badge}{draft_suffix}{reviewer_suffix}");

    let title = &rr.pr.title;

    // Layout mirrors `format_work_item_entry`: the first line shares
    // horizontal space with the right-column stack, continuation lines
    // get the full panel width (minus the "R " prefix indent). The row
    // marker is a fixed 2-column prefix so both widths subtract it.
    let prefix = "R ";
    let first_width = content_width
        .saturating_sub(prefix.width())
        .saturating_sub(right.width())
        .saturating_sub(if right.is_empty() { 0 } else { 1 });
    let rest_width = content_width.saturating_sub(prefix.width());
    let title_lines = wrap_two_widths(title, first_width.max(1), rest_width.max(1));
    let first_title = title_lines.first().cloned().unwrap_or_default();

    let padding =
        content_width.saturating_sub(prefix.width() + first_title.width() + right.width());
    let pad_str: String = " ".repeat(padding);

    // When selected, the List widget only sets the background. We
    // still apply the highlight foreground per-span so the title and
    // badges get the inverted look that work-item rows already use.
    let hl = theme.style_tab_highlight();
    let (margin_style, marker_style, title_style, pr_badge_style, reviewer_badge_style) =
        if is_selected {
            (hl, hl, hl, hl, hl)
        } else {
            (
                ratatui_core::style::Style::default(),
                theme.style_review_request_marker(),
                theme.style_text(),
                theme.style_badge_pr(),
                theme.style_badge_pr(),
            )
        };

    let mut line1_spans = vec![
        Span::styled(margin.to_string(), margin_style),
        Span::styled(prefix.to_string(), marker_style),
        Span::styled(first_title, title_style),
        Span::raw(pad_str),
        Span::styled(format!("{pr_badge}{draft_suffix}"), pr_badge_style),
    ];
    if let Some(badge) = reviewer_badge.as_deref() {
        line1_spans.push(Span::raw(" "));
        line1_spans.push(Span::styled(badge.to_string(), reviewer_badge_style));
    }

    let mut lines = vec![Line::from(line1_spans)];
    // Continuation lines: indent to align with the column after the
    // "R " marker so wrapped title text sits flush with the first
    // line's title start.
    for title_cont in title_lines.iter().skip(1) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(" ".repeat(prefix.width())),
            Span::styled(title_cont.clone(), title_style),
        ]));
    }

    ListItem::new(lines)
}

/// Format an unlinked PR entry for the left panel list.
///
/// Returns a multi-line `ListItem`:
///   Line 1: "? branch-start"   (shares line with right-aligned PR#N [draft] badge)
///   Line 2+: continuation lines of the wrapped branch name (4-space indent)
///   Final line: repo directory name (2-space indent, muted)
///
/// Mirrors the wrap-not-truncate convention used by `format_work_item_entry`.
fn format_unlinked_item<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
    is_selected: bool,
) -> ListItem<'a> {
    let margin = if is_selected { "> " } else { "  " };
    let content_width = max_width.saturating_sub(2);

    let Some(unlinked) = app.unlinked_prs.get(idx) else {
        return ListItem::new(Line::from(format!("{margin}? <invalid>")));
    };

    let pr_badge = format!("PR#{}", unlinked.pr.number);
    let mut draft_suffix = String::new();
    if unlinked.pr.is_draft {
        draft_suffix.push_str(" draft");
    }
    let right = format!("{pr_badge}{draft_suffix}");

    // Layout: "{margin}? branch    PR#N [draft]"
    //         "       continuation-of-branch"
    //         "  repo-dir"
    //
    // First line shares content_width with the "? " prefix, the right-aligned
    // badge, and a 1-col gap. Continuation lines are indented under the branch
    // title (4 spaces: 2 for margin + 2 for "? " prefix). The meta (repo dir)
    // line is indented by 2 spaces to align with the branch title, matching
    // the convention in `format_work_item_entry`.
    //
    // rest_width budgets against `max_width` (not `content_width`): continuation
    // rows carry no margin span, so the full panel width `max_width =
    // content_width + 2` is available, and the 4-space indent consumes 4 of
    // those cols. That leaves `max_width - 4 = content_width - 2` cols for the
    // wrapped text body.
    let prefix = "? ";
    let first_width = content_width
        .saturating_sub(prefix.width() + right.width() + 1)
        .max(1);
    let rest_width = content_width.saturating_sub(2).max(1);

    let branch_lines = wrap_two_widths(&unlinked.branch, first_width, rest_width);
    let first_branch = branch_lines.first().cloned().unwrap_or_default();

    let padding =
        content_width.saturating_sub(prefix.width() + first_branch.width() + right.width());
    let pad_str: String = " ".repeat(padding);

    // Styling: when selected, the `List` widget applies highlight_bg; we set fg
    // per-span so the highlight fg colors render correctly. Meta uses the
    // theme-owned `style_meta_selected` on selection so the highlight bg and
    // its paired meta fg live together in `Theme` and cannot drift apart.
    let (margin_style, marker_style, title_style, badge_style, meta_style) = if is_selected {
        let hl = theme.style_tab_highlight();
        (hl, hl, hl, hl, theme.style_meta_selected())
    } else {
        (
            ratatui_core::style::Style::default(),
            theme.style_unlinked_marker(),
            theme.style_text(),
            theme.style_badge_pr(),
            theme.style_text_muted(),
        )
    };

    let mut lines: Vec<Line> = Vec::new();

    // Line 1: margin + "? " + first branch chunk + pad + right-aligned badge.
    lines.push(Line::from(vec![
        Span::styled(margin, margin_style),
        Span::styled(prefix.to_string(), marker_style),
        Span::styled(first_branch, title_style),
        Span::raw(pad_str),
        Span::styled(right, badge_style),
    ]));

    // Branch continuation lines: 4-space indent so they align under the title.
    for cont in branch_lines.iter().skip(1) {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(cont.clone(), title_style),
        ]));
    }

    // Meta line: repo directory name, 2-space indent, muted. Wrap defensively
    // in case of a very narrow pane or a pathologically long directory name.
    // `wrap_text`'s budget is per-line content width (the prepended "  " is
    // added on top), so pass `content_width` directly to match
    // `format_work_item_entry`'s meta wrap convention exactly.
    let repo_name: String = unlinked
        .repo_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "<unknown repo>".to_string());
    for wrapped in wrap_text(&repo_name, content_width) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(wrapped, meta_style),
        ]));
    }

    ListItem::new(lines)
}

/// Format a work item entry for the left panel list.
///
/// Returns a 2-line ListItem:
///   Line 1: title (+ PR badge + CI badge if present)
///   Line 2: repo-name  branch-name  [no wt] (all muted)
fn format_work_item_entry<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
    is_selected: bool,
) -> ListItem<'a> {
    let Some(wi) = app.work_items.get(idx) else {
        return ListItem::new(Line::from("<invalid>"));
    };

    let content_width = max_width.saturating_sub(2);

    // -- Left margin: activity indicator or selection caret --
    let has_session = app.session_key_for(&wi.id).is_some();
    // Review gate is a transient substate where the item is still
    // `Implementing`/`Blocked` on the model but is running the async
    // PR/CI/adversarial-review checks on a background thread. We surface
    // it both in the spinner (same cyan braille as Claude working) and
    // as an explicit `[RG]` badge alongside the state badge below, so
    // the user can tell at a glance without opening the right panel.
    let at_review_gate = app.review_gates.contains_key(&wi.id);
    let is_working = app.agent_working.contains(&wi.id) || at_review_gate;
    let (margin_text, margin_style): (String, ratatui_core::style::Style) = if is_working {
        let frame = SPINNER_FRAMES[app.spinner_tick % SPINNER_FRAMES.len()];
        // On a highlighted row the list's bg is already Cyan, so a Cyan
        // spinner fg is invisible. Match the selection caret/title styling
        // (Black fg on Cyan bg, BOLD) so the spinner stays readable.
        let style = if is_selected {
            theme.style_tab_highlight()
        } else {
            theme.style_badge_session_working()
        };
        (format!("{frame} "), style)
    } else if has_session {
        ("\u{25CF} ".to_string(), theme.style_badge_session_idle())
    } else if is_selected {
        ("> ".to_string(), theme.style_tab_highlight())
    } else {
        ("  ".to_string(), ratatui_core::style::Style::default())
    };

    // -- Line 1: title + badges --

    // Build the right-side badge string.
    let mut right_parts: Vec<(String, ratatui_core::style::Style)> = Vec::new();

    // PR badge: show first PR if any.
    let first_pr = wi.repo_associations.iter().find_map(|a| a.pr.as_ref());
    if let Some(pr) = first_pr {
        let pr_text = format!("PR#{}", pr.number);
        let pr_style = if pr.state == PrState::Merged {
            theme.style_text_muted()
        } else {
            theme.style_badge_pr()
        };
        right_parts.push((pr_text, pr_style));

        // CI badge.
        match &pr.checks {
            CheckStatus::Passing => {
                right_parts.push((" ok".to_string(), theme.style_badge_ci_pass()));
            }
            CheckStatus::Failing => {
                right_parts.push((" fail".to_string(), theme.style_badge_ci_fail()));
            }
            CheckStatus::Pending => {
                right_parts.push((" ...".to_string(), theme.style_badge_ci_pending()));
            }
            CheckStatus::None | CheckStatus::Unknown => {}
        }
        if matches!(pr.mergeable, MergeableState::Conflicting) {
            right_parts.push((" !merge".to_string(), theme.style_badge_merge_conflict()));
        }
    }

    // Unclean-worktree chip. Rendered whenever ANY repo association has
    // a derived `GitState` whose `dirty` flag is set. `git_state.dirty`
    // is the union of modified-tracked-files and untracked-files (see
    // `GitState` doc comment). The merge guard lives in
    // `App::execute_merge` as a background
    // `WorktreeService::list_worktrees` precheck and distinguishes the
    // variants via `WorktreeCleanliness::from_worktree_info`. The chip
    // render here is a pure cache read and cannot shell out,
    // honouring the "no blocking I/O on the UI thread" invariant.
    //
    // Ahead/behind state is rendered via the dedicated `!pushed` /
    // `!pulled` chips below; `!cl` is exclusively for "uncommitted
    // changes in the worktree" so a clean-but-diverged branch no
    // longer flags as unclean.
    let is_unclean = wi
        .repo_associations
        .iter()
        .any(|a| a.git_state.as_ref().map(|gs| gs.dirty).unwrap_or(false));
    if is_unclean {
        right_parts.push((" !cl".to_string(), theme.style_badge_worktree_unclean()));
    }

    // Needs-push chip: any repo association has unpushed commits.
    // Rendered whenever `git_state.ahead > 0` for at least one
    // association. Always derived from the same fetcher cache as
    // `!cl`, so this is also a pure in-memory read.
    let needs_push = wi
        .repo_associations
        .iter()
        .any(|a| a.git_state.as_ref().map(|gs| gs.ahead > 0).unwrap_or(false));
    if needs_push {
        right_parts.push((" !pushed".to_string(), theme.style_badge_pushed()));
    }

    // Needs-pull chip: any repo association is behind its upstream.
    // Rendered whenever `git_state.behind > 0` for at least one
    // association. Coexists with `!cl` and `!pushed` on a row that is
    // dirty AND diverged in both directions.
    let needs_pull = wi.repo_associations.iter().any(|a| {
        a.git_state
            .as_ref()
            .map(|gs| gs.behind > 0)
            .unwrap_or(false)
    });
    if needs_pull {
        right_parts.push((" !pulled".to_string(), theme.style_badge_pulled()));
    }

    // Multi-repo indicator.
    let repo_count = wi.repo_associations.len();
    if repo_count > 1 {
        right_parts.push((format!(" [{repo_count} repos]"), theme.style_text_muted()));
    }

    // Stage badge + optional [RR] kind indicator + optional [RG]
    // review-gate substate + title. Done items omit the badge since the
    // DONE group header already communicates their status; the review
    // gate is a transient substate and never applies to Done items, so
    // `gate_tag` is empty on that branch by construction.
    let badge = wi.status.badge_text();
    let kind_tag = if wi.kind == WorkItemKind::ReviewRequest {
        "[RR]"
    } else {
        ""
    };
    let gate_tag = if at_review_gate && wi.status != WorkItemStatus::Done {
        "[RG]"
    } else {
        ""
    };
    let prefix = if wi.status == WorkItemStatus::Done {
        if kind_tag.is_empty() {
            String::new()
        } else {
            format!("{kind_tag} ")
        }
    } else if kind_tag.is_empty() {
        format!("{badge}{gate_tag} ")
    } else {
        format!("{kind_tag}{badge}{gate_tag} ")
    };
    // Minimum number of display columns reserved for the title so it never
    // vanishes when badges consume all available width.
    const MIN_TITLE_BUDGET: usize = 5;

    let space_for_content = content_width.saturating_sub(prefix.width());

    // Drop badges from the right until the title gets at least MIN_TITLE_BUDGET
    // columns (or we run out of badges to drop).
    let mut visible_badge_count = right_parts.len();
    let mut right_text: String = right_parts.iter().map(|(s, _)| s.as_str()).collect();
    while visible_badge_count > 0 {
        let title_budget = space_for_content
            .saturating_sub(right_text.width())
            .saturating_sub(1); // gap between title and badges
        if title_budget >= MIN_TITLE_BUDGET {
            break;
        }
        visible_badge_count -= 1;
        right_text = right_parts[..visible_badge_count]
            .iter()
            .map(|(s, _)| s.as_str())
            .collect();
    }

    let available = space_for_content
        .saturating_sub(right_text.width())
        .saturating_sub(if right_text.is_empty() { 0 } else { 1 });

    // When selected, the List widget only sets bg (via style_tab_highlight_bg).
    // We apply fg per-span here so title+badge get the original highlight look
    // (Black + BOLD) while branch metadata uses the theme-owned
    // `style_meta_selected` (paired with `tab_highlight_bg` inside Theme so
    // the fg+bg pair cannot drift when the highlight bg is retuned).
    let hl = theme.style_tab_highlight();
    let (title_style, badge_style, right_badge_style, meta_style) = if is_selected {
        (hl, hl, hl, theme.style_meta_selected())
    } else {
        let ts = if wi.status == WorkItemStatus::Done {
            theme.style_done_item()
        } else {
            theme.style_text()
        };
        (
            ts,
            theme.style_stage_badge(&wi.status),
            ratatui_core::style::Style::default(), // right badges have their own per-part styles
            theme.style_text_muted(),
        )
    };

    // Wrap the title: first line shares space with badge + right badges,
    // continuation lines get the full panel width with no indent.
    let title_lines = wrap_two_widths(&wi.title, available.max(1), content_width);
    let first_title = title_lines.first().cloned().unwrap_or_default();

    let padding =
        content_width.saturating_sub(prefix.width() + first_title.width() + right_text.width());
    let pad_str: String = " ".repeat(padding);

    let mut line1_spans = if wi.status == WorkItemStatus::Done {
        vec![
            Span::styled(margin_text, margin_style),
            Span::styled(first_title, title_style),
            Span::raw(pad_str),
        ]
    } else {
        vec![
            Span::styled(margin_text, margin_style),
            Span::styled(badge.to_string(), badge_style),
            Span::raw(" "),
            Span::styled(first_title, title_style),
            Span::raw(pad_str),
        ]
    };
    // Insert [RR] badge after the margin span for review-request items.
    if wi.kind == WorkItemKind::ReviewRequest {
        line1_spans.insert(
            1,
            Span::styled("[RR]".to_string(), theme.style_badge_review_request_kind()),
        );
    }
    // Insert [RG] badge immediately after the state badge for items
    // currently at a review gate. Mirrors the [RR] pattern above. Never
    // applies to Done items (they have no state badge and can't hold a
    // review gate), so `gate_tag` is empty there by construction.
    //
    // Insertion index depends on whether `[RR]` was already inserted:
    //   - Base non-Done layout: [margin, state_badge, " ", title, pad]
    //     -> state badge at 1, [RG] goes at 2.
    //   - With [RR] inserted at 1: [margin, [RR], state_badge, " ", ...]
    //     -> state badge at 2, [RG] goes at 3.
    if !gate_tag.is_empty() {
        let insert_idx = if wi.kind == WorkItemKind::ReviewRequest {
            3
        } else {
            2
        };
        line1_spans.insert(
            insert_idx,
            Span::styled("[RG]".to_string(), theme.style_badge_review_gate()),
        );
    }
    for (text, style) in &right_parts[..visible_badge_count] {
        let s = if is_selected {
            right_badge_style
        } else {
            *style
        };
        line1_spans.push(Span::styled(text.clone(), s));
    }
    let line1 = Line::from(line1_spans);

    // -- Line 2+: metadata (only if the work item has meaningful context) --
    //
    // A work item can be in different states of completeness:
    // - Has branch + worktree + PR: show branch (repo) with all badges
    // - Has branch but no worktree: show branch (repo) [no wt]
    // - Has no branch (pre-planning): show just repo name, no tags
    // - Has no repo associations: show nothing (shouldn't happen per invariant 1)

    let first_assoc = wi.repo_associations.first();
    let has_branch = first_assoc.is_some_and(|a| a.branch.is_some());
    let has_worktree = first_assoc.is_some_and(|a| a.worktree_path.is_some());

    let mut lines = vec![line1];

    // Title continuation lines (indented to align with content after margin).
    for title_cont in title_lines.iter().skip(1) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(title_cont.clone(), title_style),
        ]));
    }

    // Backend-provided display ID (e.g. `#workbridge-42`).
    //
    // Rendered as a dimmed subtitle line between the title and the
    // branch line, styled with the same `meta_style` as the branch
    // subtitle so selection highlighting flows consistently across
    // both. Records created before this feature landed have
    // `display_id == None` and skip this block entirely - they render
    // exactly as before with no reserved blank line.
    if let Some(display_id) = wi.display_id.as_deref() {
        let id_text = format!("#{display_id}");
        for wrapped in wrap_text(&id_text, content_width) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(wrapped, meta_style),
            ]));
        }
    }

    if has_branch {
        // Branch + [no wt] indicator. Repo is shown in the group header.
        let branch_name = first_assoc.and_then(|a| a.branch.as_deref()).unwrap_or("");
        let wt_indicator = if has_worktree { "" } else { " [no wt]" };

        let meta_content = format!("{branch_name}{wt_indicator}");
        for wrapped_line in wrap_text(&meta_content, content_width) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(wrapped_line, meta_style),
            ]));
        }
    }
    // No repo associations = no line 2 (invariant 1 violation, but render gracefully)

    ListItem::new(lines)
}

/// Word-wrap a string to fit within max_width display columns.
/// Breaks at word boundaries (space, /, -, paren) when possible.
/// Wraps to as many lines as needed - no artificial cap.
/// When `indent` is true, continuation lines are indented with 4 spaces.
/// Every output line is guaranteed to be <= max_width display columns.
fn wrap_text_impl(s: &str, max_width: usize, indent: bool) -> Vec<String> {
    const INDENT_STR: &str = "    ";
    let indent_width = if indent { INDENT_STR.width() } else { 0 };

    if max_width == 0 {
        return vec![];
    }

    if s.width() <= max_width {
        return vec![s.to_string()];
    }

    let mut lines = Vec::new();
    let mut remaining = s;

    while !remaining.is_empty() {
        // Continuation lines have less space due to indent
        let effective_width = if lines.is_empty() {
            max_width
        } else {
            max_width.saturating_sub(indent_width)
        };

        // Guard: if effective_width is 0 (max_width < indent), force at least 1 char
        let effective_width = effective_width.max(1);

        if remaining.width() <= effective_width {
            lines.push(remaining.to_string());
            break;
        }

        // Find the byte index where cumulative display width reaches effective_width
        let byte_limit = remaining
            .char_indices()
            .scan(0usize, |acc, (i, c)| {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                *acc += w;
                Some((i, *acc))
            })
            .take_while(|&(_, cum_w)| cum_w <= effective_width)
            .last()
            .map(|(i, _)| {
                // Advance past this char to get the end byte index
                i + remaining[i..].chars().next().map_or(0, |c| c.len_utf8())
            })
            .unwrap_or_else(|| {
                // First char is already wider than effective_width; take it anyway
                remaining.chars().next().map_or(0, |c| c.len_utf8())
            });

        // Try to break at a word boundary within the limit
        let break_at = remaining[..byte_limit]
            .rfind([' ', '/', '-', '('])
            .map(|i| i + 1)
            .unwrap_or(byte_limit);

        let (line, rest) = remaining.split_at(break_at);
        lines.push(line.to_string());

        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        remaining = trimmed;
    }

    // Prepend indent to continuation lines, but only if max_width can
    // accommodate it without exceeding the width guarantee.
    if indent && max_width > indent_width {
        for line in lines.iter_mut().skip(1) {
            *line = format!("{INDENT_STR}{line}");
        }
    }

    lines
}

/// Word-wrap with 4-space continuation indent (default behavior).
fn wrap_text(s: &str, max_width: usize) -> Vec<String> {
    wrap_text_impl(s, max_width, true)
}

/// Word-wrap with no continuation indent.
fn wrap_text_flat(s: &str, max_width: usize) -> Vec<String> {
    wrap_text_impl(s, max_width, false)
}

/// Word-wrap where the first line has a narrower budget than subsequent lines.
/// Used for titles where line 1 shares space with badge + right badges.
fn wrap_two_widths(s: &str, first_width: usize, rest_width: usize) -> Vec<String> {
    if first_width == 0 || s.is_empty() {
        return vec![];
    }
    // If it fits on the first line, done.
    if s.width() <= first_width {
        return vec![s.to_string()];
    }
    // Break the first line at first_width.
    let first_lines = wrap_text_flat(s, first_width);
    let first = first_lines[0].clone();
    // Reconstruct the remainder from the original string.
    let used_bytes = first.trim_end().len();
    let rest = s[used_bytes..].trim_start();
    if rest.is_empty() {
        return vec![first];
    }
    let mut lines = vec![first];
    lines.extend(wrap_text_flat(rest, rest_width));
    lines
}

/// Truncate a string to fit within max_len display columns.
/// If truncated, appends "..".
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.width() <= max_len {
        s.to_string()
    } else if max_len <= 2 {
        truncate_to_width(s, max_len)
    } else {
        let mut result = truncate_to_width(s, max_len - 2);
        result.push_str("..");
        result
    }
}

/// Take chars from `s` until their cumulative display width reaches `max_cols`.
fn truncate_to_width(s: &str, max_cols: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > max_cols {
            break;
        }
        width += cw;
        result.push(c);
    }
    result
}

/// Format a WorkItemError into a user-facing message and optional suggestion.
fn format_work_item_error(error: &WorkItemError) -> (String, Option<String>) {
    match error {
        WorkItemError::MultiplePrsForBranch {
            repo_path,
            branch,
            count,
        } => (
            format!(
                "{count} open PRs for branch '{branch}' in {}",
                repo_path.display()
            ),
            Some("Close duplicate PRs to resolve.".into()),
        ),
        WorkItemError::DetachedHead {
            repo_path,
            worktree_path,
        } => (
            format!(
                "Detached HEAD at {} ({})",
                worktree_path.display(),
                repo_path.display()
            ),
            Some("Check out a branch in this worktree.".into()),
        ),
        WorkItemError::IssueNotFound {
            repo_path,
            issue_number,
        } => (
            format!("Issue #{issue_number} not found in {}", repo_path.display()),
            Some("The issue may have been deleted or the number is wrong.".into()),
        ),
        WorkItemError::CorruptBackendRecord { reason, backend } => (
            format!("Corrupt {backend:?} record: {reason}"),
            Some("Delete and re-create this work item.".into()),
        ),
        WorkItemError::WorktreeGone {
            repo_path,
            expected_path,
        } => (
            format!(
                "Worktree missing: {} ({})",
                expected_path.display(),
                repo_path.display()
            ),
            Some("The worktree directory was removed from disk.".into()),
        ),
    }
}

/// Draw a structured detail view for a work item with no active session.
///
/// Shows title, status, backend type, repo, branch, worktree, PR, PR URL,
/// issue, and errors, followed by a stage-specific hint. When a
/// mergequeue poll error is supplied, it is rendered below the hint so
/// it survives longer than a transient `status_message`.
fn draw_work_item_detail(
    buf: &mut Buffer,
    app: &App,
    wi: Option<&crate::work_item::WorkItem>,
    theme: &Theme,
    block: Block<'_>,
    area: Rect,
    mergequeue_poll_error: Option<&str>,
) {
    let Some(wi) = wi else {
        let text = Text::from(vec![
            Line::from(""),
            Line::from("  Press c (Claude) or x (Codex)"),
            Line::from("  to start a session."),
        ]);
        let paragraph = Paragraph::new(text)
            .block(block)
            .style(theme.style_text_muted());
        paragraph.render(area, buf);
        return;
    };

    // Inner rect of the bordered block, in absolute frame coordinates.
    // All click-target rects below are computed by adding a line index
    // and a column offset to this origin so `handle_mouse` receives
    // the same coordinates it reads from `MouseEvent`.
    let inner = block.inner(area);
    let mut registry = app.click_registry.borrow_mut();

    let first_assoc = wi.repo_associations.first();
    let label_style = theme.style_heading();
    let none_style = theme.style_text_muted();

    let status_str = match wi.status {
        WorkItemStatus::Backlog => "Backlog",
        WorkItemStatus::Planning => "Planning",
        WorkItemStatus::Implementing => "Implementing",
        WorkItemStatus::Blocked => "Blocked",
        WorkItemStatus::Review => "Review",
        WorkItemStatus::Mergequeue => "Mergequeue",
        WorkItemStatus::Done => "Done",
    };

    let backend_str = match wi.backend_type {
        BackendType::LocalFile => "Local file",
        BackendType::GithubIssue => "GitHub issue",
        BackendType::GithubProject => "GitHub project",
    };

    let repo_str = first_assoc
        .map(|a| a.repo_path.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());

    let branch_str = first_assoc
        .and_then(|a| a.branch.as_deref())
        .unwrap_or("(none)");

    let worktree_str = first_assoc
        .and_then(|a| a.worktree_path.as_ref())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());

    let pr_str = first_assoc
        .and_then(|a| a.pr.as_ref())
        .map(|pr| format!("#{} - {}", pr.number, pr.title))
        .unwrap_or_else(|| "(none)".to_string());

    // PR URL is rendered on its own dedicated line below the field block
    // (not as a regular `label  value` row) so that the URL gets the full
    // inner width of the panel instead of just the few columns left after
    // the label prefix. Long real-world URLs (`/<long-org>/<long-repo>/
    // pull/<n>`) would silently truncate at the panel edge inside the
    // single-line `Paragraph` otherwise.
    let pr_url = first_assoc.and_then(|a| a.pr.as_ref()).map(|pr| &pr.url);

    let issue_str = first_assoc
        .and_then(|a| a.issue.as_ref())
        .map(|issue| format!("#{} - {}", issue.number, issue.title))
        .unwrap_or_else(|| "(none)".to_string());

    let errors_str = if wi.errors.is_empty() {
        "(none)".to_string()
    } else {
        wi.errors
            .iter()
            .map(|e| format_work_item_error(e).0)
            .collect::<Vec<_>>()
            .join("; ")
    };

    // Helper: style a value as muted if it is "(none)", otherwise default.
    let val_style = |s: &str| -> ratatui_core::style::Style {
        if s == "(none)" {
            none_style
        } else {
            theme.style_text()
        }
    };

    // Interactive labels (title, repo path, branch, PR URL) render
    // with the `theme.style_interactive()` accent + underline and are
    // registered in the per-frame `ClickRegistry` so `handle_mouse`
    // can fire a click-to-copy action. See docs/UI.md "Interactive
    // labels" for the convention.
    //
    // Rects are computed from `inner.x` / `inner.y` + the current line
    // index + the column offset of the value span inside its line.
    // All coordinates are absolute frame coordinates so they can be
    // compared directly to `MouseEvent::column` / `row`.
    //
    // "(none)" placeholders are NOT registered as click targets (the
    // underline would be misleading - there is nothing to copy) and
    // keep the existing muted style.
    const LABEL_INDENT: u16 = 2; // "  " indent before every row.
    const LABEL_WIDTH: u16 = 12; // Padded label column width.

    let mut lines: Vec<Line<'static>> = Vec::new();

    // lines[0]: blank
    lines.push(Line::from(""));

    // lines[1]: "  <title>". Split into a leading pad span and a
    // styled title span so the click rect covers only the title
    // glyphs - clicking on the pad should not count as a hit.
    if wi.title.is_empty() {
        lines.push(Line::from(Span::styled(
            "  ".to_string(),
            theme.style_text(),
        )));
    } else {
        let title_value = wi.title.clone();
        let title_width = UnicodeWidthStr::width(title_value.as_str()) as u16;
        registry.push_copy(
            Rect {
                x: inner.x.saturating_add(LABEL_INDENT),
                y: inner.y.saturating_add(1),
                width: title_width,
                height: 1,
            },
            ClickKind::Title,
            title_value.clone(),
        );
        lines.push(Line::from(vec![
            Span::styled("  ".to_string(), theme.style_text()),
            Span::styled(title_value, theme.style_interactive()),
        ]));
    }

    // lines[2]: blank separator
    lines.push(Line::from(""));

    // Render detail rows in the historical order. Repo and Branch
    // are the two interactive rows; everything else is rendered as a
    // non-interactive "  Label       value" row.
    let plain_row = |label: &str, value: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {label:<12}"), label_style),
            Span::styled(value.to_string(), val_style(value)),
        ])
    };

    lines.push(plain_row("Status", status_str));
    lines.push(plain_row("Backend", backend_str));

    // Repo row (interactive).
    {
        let line_index = lines.len() as u16;
        if repo_str == "(none)" {
            lines.push(plain_row("Repo", &repo_str));
        } else {
            let value_width = UnicodeWidthStr::width(repo_str.as_str()) as u16;
            registry.push_copy(
                Rect {
                    x: inner.x.saturating_add(LABEL_INDENT + LABEL_WIDTH),
                    y: inner.y.saturating_add(line_index),
                    width: value_width,
                    height: 1,
                },
                ClickKind::RepoPath,
                repo_str.clone(),
            );
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<12}", "Repo"), label_style),
                Span::styled(repo_str.clone(), theme.style_interactive()),
            ]));
        }
    }

    // Branch row (interactive).
    {
        let line_index = lines.len() as u16;
        if branch_str == "(none)" {
            lines.push(plain_row("Branch", branch_str));
        } else {
            let value_width = UnicodeWidthStr::width(branch_str) as u16;
            registry.push_copy(
                Rect {
                    x: inner.x.saturating_add(LABEL_INDENT + LABEL_WIDTH),
                    y: inner.y.saturating_add(line_index),
                    width: value_width,
                    height: 1,
                },
                ClickKind::Branch,
                branch_str.to_string(),
            );
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<12}", "Branch"), label_style),
                Span::styled(branch_str.to_string(), theme.style_interactive()),
            ]));
        }
    }

    lines.push(plain_row("Worktree", &worktree_str));
    lines.push(plain_row("PR", &pr_str));
    lines.push(plain_row("Issue", &issue_str));
    lines.push(plain_row("Errors", &errors_str));

    // PR URL on its own line so it gets the full inner width.
    if let Some(url) = pr_url {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  PR URL", label_style)));
        let line_index = lines.len() as u16;
        let url_value = url.to_string();
        let url_width = UnicodeWidthStr::width(url_value.as_str()) as u16;
        registry.push_copy(
            Rect {
                x: inner.x.saturating_add(LABEL_INDENT),
                y: inner.y.saturating_add(line_index),
                width: url_width,
                height: 1,
            },
            ClickKind::PrUrl,
            url_value.clone(),
        );
        lines.push(Line::from(vec![
            Span::styled("  ".to_string(), theme.style_text()),
            Span::styled(url_value, theme.style_interactive()),
        ]));
    }

    lines.push(Line::from(""));
    let hint_lines: &[&str] = match wi.status {
        WorkItemStatus::Backlog => &["  Press c (Claude) or x (Codex) to", "  begin planning."],
        WorkItemStatus::Done => &["  Done."],
        WorkItemStatus::Mergequeue => &[
            "  Waiting for PR to be merged.",
            "  Polling GitHub every 30s.",
            "  Shift+Left to move back to Review and stop polling.",
        ],
        WorkItemStatus::Planning
        | WorkItemStatus::Implementing
        | WorkItemStatus::Blocked
        | WorkItemStatus::Review => {
            let has_stale = wi
                .repo_associations
                .iter()
                .any(|a| a.stale_worktree_path.is_some());
            if has_stale {
                &["  Press Enter to recover worktree."]
            } else {
                &["  Press c (Claude) or x (Codex) to start a session."]
            }
        }
    };
    for hint in hint_lines {
        lines.push(Line::from(Span::styled(*hint, none_style)));
    }

    if let Some(err) = mergequeue_poll_error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  Last poll error: {err}"),
            theme.style_error(),
        )));
    }

    // Drop the registry borrow before rendering. Rendering does not
    // touch the registry, but explicitly ending the borrow here keeps
    // the lifetime obvious and guards against a future render path
    // that might try to borrow it again.
    drop(registry);

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text).block(block);
    paragraph.render(area, buf);
}

/// Detail fields for rendering an importable PR in the right panel.
struct ImportablePrDetail<'a> {
    pr: &'a crate::work_item::PrInfo,
    repo_path: &'a std::path::Path,
    branch: &'a str,
    hint: &'a str,
    /// Optional authoritative reviewer-identity list for review-request
    /// detail panels. Each element is a display string ready to join
    /// with ", " ("you", "team-core", etc.). `None` for unlinked-PR
    /// detail panels where the field is irrelevant; `Some` with at
    /// least one entry for review-request detail panels. Names are
    /// never truncated - the detail panel wraps naturally.
    requested_from: Option<Vec<String>>,
}

/// Draw a structured detail view for an importable PR (unlinked or review request).
///
/// Shows PR title, number/URL, repo, branch, state, draft status,
/// review decision, and CI checks, followed by a contextual hint.
fn draw_importable_pr_detail(
    buf: &mut Buffer,
    detail: &ImportablePrDetail<'_>,
    theme: &Theme,
    block: Block<'_>,
    area: Rect,
) {
    let pr = detail.pr;
    let label_style = theme.style_heading();
    let none_style = theme.style_text_muted();

    let val_style = |s: &str| -> ratatui_core::style::Style {
        if s == "(none)" {
            none_style
        } else {
            theme.style_text()
        }
    };

    let pr_str = format!("#{} {}", pr.number, pr.url);
    let repo_str = detail.repo_path.display().to_string();

    let state_str = match pr.state {
        PrState::Open => "Open",
        PrState::Closed => "Closed",
        PrState::Merged => "Merged",
    };

    let draft_str = if pr.is_draft { "Yes" } else { "No" };

    let review_str = match pr.review_decision {
        ReviewDecision::Approved => "Approved",
        ReviewDecision::ChangesRequested => "Changes requested",
        ReviewDecision::Pending => "Pending",
        ReviewDecision::None => "(none)",
    };

    let checks_str = match pr.checks {
        CheckStatus::Passing => "Passing",
        CheckStatus::Failing => "Failing",
        CheckStatus::Pending => "Pending",
        CheckStatus::Unknown => "Unknown",
        CheckStatus::None => "(none)",
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(format!("  {}", pr.title), theme.style_text())),
        Line::from(""),
    ];

    // `Requested from:` joins the reviewer-identity list unmodified
    // (no truncation). Built up-front so the `fields` slice can borrow
    // a &str pointing into the owned String below.
    let requested_from_joined = detail
        .requested_from
        .as_ref()
        .filter(|v| !v.is_empty())
        .map(|v| v.join(", "));

    let mut fields: Vec<(&str, &str)> = vec![
        ("PR", &pr_str),
        ("Repo", &repo_str),
        ("Branch", detail.branch),
        ("State", state_str),
        ("Draft", draft_str),
        ("Review", review_str),
        ("Checks", checks_str),
    ];
    if let Some(ref joined) = requested_from_joined {
        fields.push(("Requested from", joined.as_str()));
    }

    // Labels up to the historical 12-column width get the legacy
    // fixed-width padding so the unlinked-PR detail panel (which has
    // no long labels) renders byte-identically. "Requested from" and
    // any future wider label fall back to a single trailing space.
    for (label, value) in &fields {
        let label_str = if label.width() <= 12 {
            format!("  {label:<12}")
        } else {
            format!("  {label} ")
        };
        lines.push(Line::from(vec![
            Span::styled(label_str, label_style),
            Span::styled(value.to_string(), val_style(value)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  {}", detail.hint),
        none_style,
    )));

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text).block(block);
    paragraph.render(area, buf);
}

/// Draw the right panel showing captured PTY output.
/// Uses vt100::Parser + tui-term PseudoTerminal for full ANSI color rendering.
///
/// The active session is determined by the currently selected work item:
/// - If selected item is a work item with a session -> render PseudoTerminal
/// - If selected item is a work item without a session -> prompt to start
/// - If selected item is an unlinked PR -> prompt to import
/// - If nothing selected -> show welcome message
fn draw_pane_output(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    // When the settings overlay is open, dim background panels.
    let border_style = if app.show_settings {
        theme.style_border_unfocused()
    } else if app.focus == FocusPanel::Right {
        theme.style_border_input()
    } else {
        theme.style_border_default()
    };

    let has_worktree = app.selected_work_item_has_worktree();

    let in_scrollback = app
        .active_session_entry()
        .is_some_and(|e| e.scrollback_offset > 0);

    let input_suffix = if in_scrollback {
        " [SCROLLBACK] "
    } else if app.focus == FocusPanel::Right {
        " [INPUT] "
    } else {
        " "
    };

    let title_line: Line = if has_worktree {
        let (cc_style, term_style) = match app.right_panel_tab {
            RightPanelTab::ClaudeCode => (
                theme.style_view_mode_tab_active(),
                theme.style_view_mode_tab(),
            ),
            RightPanelTab::Terminal => (
                theme.style_view_mode_tab(),
                theme.style_view_mode_tab_active(),
            ),
        };
        let backend_tab = format!(" {} ", app.agent_backend_display_name());
        Line::from(vec![
            Span::raw(" "),
            Span::styled(backend_tab, cc_style),
            Span::styled(" | ", theme.style_title()),
            Span::styled(" Terminal ", term_style),
            Span::styled(input_suffix, theme.style_title()),
        ])
    } else {
        let title_text = format!(" {}{input_suffix}", app.agent_backend_display_name());
        Line::from(Span::styled(title_text, theme.style_title()))
    };

    let block = Block::default()
        .title(title_line)
        .borders(Borders::ALL)
        .border_style(border_style);

    // Determine what to show based on the selected display list entry.
    let selected_entry = app.selected_item.and_then(|idx| app.display_list.get(idx));

    // If the Terminal tab is active, render the terminal PTY instead of Claude Code.
    if app.right_panel_tab == RightPanelTab::Terminal {
        if let Some(entry) = app.active_terminal_entry() {
            if entry.alive {
                if let Ok(parser) = entry.parser.lock() {
                    let pseudo_term = PseudoTerminal::new(parser.screen()).block(block);
                    pseudo_term.render(area, buf);
                    if let Some(ref sel) = entry.selection {
                        let inner = area.inner(Margin::new(1, 1));
                        render_selection_overlay(buf, inner, sel);
                    }
                } else {
                    let text = Text::from(vec![Line::from(""), Line::from("  [render error]")]);
                    let paragraph = Paragraph::new(text).block(block).style(theme.style_error());
                    paragraph.render(area, buf);
                }
            } else {
                let text = Text::from(vec![
                    Line::from(""),
                    Line::from("  Terminal session has ended."),
                    Line::from(""),
                    Line::from(format!(
                        "  Press Ctrl+\\ to switch back to {}.",
                        app.agent_backend_display_name()
                    )),
                ]);
                let paragraph = Paragraph::new(text).block(block).style(theme.style_error());
                paragraph.render(area, buf);
            }
        } else {
            let text = Text::from(vec![Line::from(""), Line::from("  Starting terminal...")]);
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(theme.style_text_muted());
            paragraph.render(area, buf);
        }
        return;
    }

    match selected_entry {
        Some(DisplayEntry::WorkItemEntry(wi_idx)) => {
            // Check if the rebase gate is running for this work item.
            // Rebase gate render takes precedence over the review gate
            // by explicit ordering; in practice they cannot both be in
            // flight (the rebase gate goes through
            // `UserActionKey::RebaseOnMain` and is started from the
            // left panel only) but the deterministic order keeps the
            // render path predictable.
            let rebase_gate_active = app
                .work_items
                .get(*wi_idx)
                .map(|wi| app.rebase_gates.contains_key(&wi.id))
                .unwrap_or(false);

            if rebase_gate_active {
                let spinner_chars = [b'|', b'/', b'-', b'\\'];
                let frame = app.spinner_tick % spinner_chars.len();
                let spinner = spinner_chars[frame] as char;
                let progress_text = app
                    .work_items
                    .get(*wi_idx)
                    .and_then(|wi| app.rebase_gates.get(&wi.id))
                    .and_then(|gate| gate.progress.as_deref())
                    .unwrap_or("Rebasing onto upstream main...");
                let text = Text::from(vec![
                    Line::from(""),
                    Line::from(format!("  {spinner} Running rebase gate...")),
                    Line::from(""),
                    Line::from(format!("  {progress_text}")),
                ]);
                let paragraph = Paragraph::new(text)
                    .block(block)
                    .style(theme.style_text_muted());
                paragraph.render(area, buf);
                return;
            }

            // Check if the review gate is running for this work item.
            let review_gate_active = app
                .work_items
                .get(*wi_idx)
                .map(|wi| app.review_gates.contains_key(&wi.id))
                .unwrap_or(false);

            if review_gate_active {
                let spinner_chars = [b'|', b'/', b'-', b'\\'];
                let frame = app.spinner_tick % spinner_chars.len();
                let spinner = spinner_chars[frame] as char;
                let progress_text = app
                    .work_items
                    .get(*wi_idx)
                    .and_then(|wi| app.review_gates.get(&wi.id))
                    .and_then(|gate| gate.progress.as_deref())
                    .unwrap_or("Checking implementation against plan.");
                let text = Text::from(vec![
                    Line::from(""),
                    Line::from(format!("  {spinner} Running review gate...")),
                    Line::from(""),
                    Line::from(format!("  {progress_text}")),
                ]);
                let paragraph = Paragraph::new(text)
                    .block(block)
                    .style(theme.style_text_muted());
                paragraph.render(area, buf);
                return;
            }

            let session_key = app
                .work_items
                .get(*wi_idx)
                .and_then(|wi| app.session_key_for(&wi.id));
            let session_entry = session_key.as_ref().and_then(|key| app.sessions.get(key));

            match session_entry {
                Some(entry) if !entry.alive => {
                    let text = Text::from(vec![
                        Line::from(""),
                        Line::from("  Session has ended."),
                        Line::from(""),
                        Line::from("  Press c (Claude) or x (Codex)"),
                        Line::from("  to start a new session."),
                    ]);
                    let paragraph = Paragraph::new(text).block(block).style(theme.style_error());
                    paragraph.render(area, buf);
                }
                Some(entry) => {
                    // Lock the shared parser to get the current screen state.
                    if let Ok(mut parser) = entry.parser.lock() {
                        // vt100's visible_rows() computes
                        // `rows_len - scrollback_offset` which is a usize
                        // subtraction that panics on underflow when
                        // scrollback_offset > terminal rows. Clamp to the
                        // terminal height to prevent this.
                        let rows = parser.screen().size().0 as usize;
                        let clamped = entry.scrollback_offset.min(rows);
                        parser.set_scrollback(clamped);
                        let pseudo_term = PseudoTerminal::new(parser.screen()).block(block);
                        pseudo_term.render(area, buf);
                        if let Some(ref sel) = entry.selection {
                            let inner = area.inner(Margin::new(1, 1));
                            render_selection_overlay(buf, inner, sel);
                        }
                    } else {
                        // Parser lock poisoned - show a fallback message.
                        let text = Text::from(vec![Line::from(""), Line::from("  [render error]")]);
                        let paragraph =
                            Paragraph::new(text).block(block).style(theme.style_error());
                        paragraph.render(area, buf);
                    }
                }
                None => {
                    // If worktree creation is in flight for this work item,
                    // show a spinner instead of the "Press Enter to start a
                    // session." hint - Enter is a no-op while the background
                    // thread is running, so the hint would be misleading.
                    let worktree_creating = app
                        .work_items
                        .get(*wi_idx)
                        .map(|wi| {
                            app.user_action_work_item(&UserActionKey::WorktreeCreate)
                                == Some(&wi.id)
                        })
                        .unwrap_or(false);

                    if worktree_creating {
                        let spinner_chars = [b'|', b'/', b'-', b'\\'];
                        let frame = app.spinner_tick % spinner_chars.len();
                        let spinner = spinner_chars[frame] as char;
                        let text = Text::from(vec![
                            Line::from(""),
                            Line::from(format!("  {spinner} Creating worktree...")),
                            Line::from(""),
                            Line::from(Span::styled(
                                "  Fetching branch and setting up workspace.",
                                theme.style_text_muted(),
                            )),
                        ]);
                        let paragraph = Paragraph::new(text)
                            .block(block)
                            .style(theme.style_text_muted());
                        paragraph.render(area, buf);
                        return;
                    }

                    let wi = app.work_items.get(*wi_idx);
                    let errors = wi.map(|w| &w.errors);
                    let has_errors = errors.is_some_and(|e| !e.is_empty());

                    if has_errors {
                        let mut lines = vec![
                            Line::from(""),
                            Line::from(Span::styled("  Errors:", theme.style_error())),
                        ];
                        for error in errors.unwrap() {
                            lines.push(Line::from(""));
                            let (msg, suggestion) = format_work_item_error(error);
                            lines.push(Line::from(Span::styled(
                                format!("  - {msg}"),
                                theme.style_error(),
                            )));
                            if let Some(hint) = suggestion {
                                lines.push(Line::from(Span::styled(
                                    format!("    {hint}"),
                                    theme.style_text_muted(),
                                )));
                            }
                        }
                        lines.push(Line::from(""));
                        let hint_lines: &[&str] = match wi.map(|w| &w.status) {
                            Some(WorkItemStatus::Backlog) => {
                                &["  Press c (Claude) or x (Codex) to begin planning."]
                            }
                            Some(WorkItemStatus::Done) => &["  Done."],
                            Some(WorkItemStatus::Mergequeue) => &[
                                "  Waiting for PR to be merged.",
                                "  Polling GitHub every 30s.",
                                "  Shift+Left to move back to Review and stop polling.",
                            ],
                            _ => &["  Press c (Claude) or x (Codex) to start a session."],
                        };
                        for hint in hint_lines {
                            lines.push(Line::from(Span::styled(*hint, theme.style_text_muted())));
                        }
                        let text = Text::from(lines);
                        let paragraph = Paragraph::new(text).block(block);
                        paragraph.render(area, buf);
                    } else {
                        // A work item can hit at most one of these
                        // maps at a time (Mergequeue status vs. a
                        // ReviewRequest in Review), so a simple
                        // fallback covers both without a second
                        // parameter on `draw_work_item_detail`. The
                        // rendered line is "Last poll error: ..." in
                        // both cases.
                        let poll_error = wi
                            .and_then(|w| {
                                app.mergequeue_poll_errors
                                    .get(&w.id)
                                    .or_else(|| app.review_request_merge_poll_errors.get(&w.id))
                            })
                            .map(String::as_str);
                        draw_work_item_detail(buf, app, wi, theme, block, area, poll_error);
                    }
                }
            }
        }
        Some(DisplayEntry::UnlinkedItem(ul_idx)) => {
            if let Some(unlinked) = app.unlinked_prs.get(*ul_idx) {
                draw_importable_pr_detail(
                    buf,
                    &ImportablePrDetail {
                        pr: &unlinked.pr,
                        repo_path: &unlinked.repo_path,
                        branch: &unlinked.branch,
                        hint: "Press Enter to import this PR as a work item.  Ctrl+D to close PR and delete branch.",
                        requested_from: None,
                    },
                    theme,
                    block,
                    area,
                );
            } else {
                let text = Text::from(vec![
                    Line::from(""),
                    Line::from("  Press Enter to import"),
                    Line::from("  this PR as a work item."),
                ]);
                let paragraph = Paragraph::new(text)
                    .block(block)
                    .style(theme.style_text_muted());
                paragraph.render(area, buf);
            }
        }
        Some(DisplayEntry::ReviewRequestItem(rr_idx)) => {
            if let Some(rr) = app.review_requested_prs.get(*rr_idx) {
                // Build the authoritative reviewer-identity list for
                // the detail panel. "you" goes first when the current
                // user is directly requested; every other directly-
                // requested user follows (rendered as their literal
                // login); then every team slug, prefixed with "team "
                // so the row reads naturally even when the team name
                // does not start with "team-". A PR can request
                // multiple direct reviewers (e.g. `alice` + `bob` +
                // you), so we must iterate `requested_reviewer_logins`
                // rather than only emit "you" - otherwise alice and
                // bob silently vanish from the list. When no
                // reviewers are known (degenerate fetch data) pass
                // None so the detail panel omits the row entirely
                // rather than rendering an empty list.
                let login = app.current_user_login.as_deref();
                let mut requested_from: Vec<String> = Vec::new();
                if rr.is_direct_request(login) {
                    requested_from.push("you".to_string());
                }
                for reviewer_login in &rr.requested_reviewer_logins {
                    // Skip the current user - already added as "you"
                    // above. Every other directly-requested user is
                    // rendered as their literal login.
                    if login.is_some_and(|l| l == reviewer_login) {
                        continue;
                    }
                    requested_from.push(reviewer_login.clone());
                }
                for slug in &rr.requested_team_slugs {
                    requested_from.push(format!("team {slug}"));
                }
                let requested_from = if requested_from.is_empty() {
                    None
                } else {
                    Some(requested_from)
                };
                draw_importable_pr_detail(
                    buf,
                    &ImportablePrDetail {
                        pr: &rr.pr,
                        repo_path: &rr.repo_path,
                        branch: &rr.branch,
                        hint: "Press Enter to import this review request as a work item.",
                        requested_from,
                    },
                    theme,
                    block,
                    area,
                );
            } else {
                let text = Text::from(vec![
                    Line::from(""),
                    Line::from("  Press Enter to import this"),
                    Line::from("  review request as a work item."),
                ]);
                let paragraph = Paragraph::new(text)
                    .block(block)
                    .style(theme.style_text_muted());
                paragraph.render(area, buf);
            }
        }
        _ => {
            // Nothing selected or non-selectable entry.
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Welcome to workbridge"),
                Line::from(""),
                Line::from("  Ctrl+N    - Quick start session"),
                Line::from("  Ctrl+B    - New backlog ticket"),
                Line::from("  Ctrl+R    - Refresh GitHub data"),
                Line::from("  Up/Down   - Navigate items"),
                Line::from("  Enter     - Open session / Import"),
                Line::from("  o         - Open PR in browser"),
                Line::from("  Ctrl+]    - Return to item list"),
                Line::from("  Ctrl+G    - Global assistant"),
                Line::from("  Ctrl+D    - Delete work item"),
                Line::from("  ?         - Settings"),
                Line::from("  Q/Ctrl+Q  - Quit"),
            ]);
            let paragraph = Paragraph::new(text)
                .block(block)
                .style(theme.style_text_muted());
            paragraph.render(area, buf);
        }
    }
}

/// Draw the global assistant bottom drawer with dimmed background.
///
/// The drawer is anchored to the bottom of the screen, inset 2 columns on
/// each side to create a floating-sheet effect. Everything behind it is
/// dimmed to give visual depth.
fn draw_global_drawer(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    // 1. Dim every cell in the buffer to push the background behind the drawer.
    dim_background(buf, area);

    // 2. Compute drawer rect via shared helper (overflow-safe).
    let dl = crate::layout::compute_drawer(area.width, area.height);
    let drawer_width = dl.drawer_width;
    let drawer_height = dl.drawer_height;
    let drawer_x = area.x + 2;
    let drawer_y = area.y + area.height.saturating_sub(drawer_height);
    let drawer_rect = Rect::new(drawer_x, drawer_y, drawer_width, drawer_height);

    // 3. Clear the drawer area and draw the border.
    Clear.render(drawer_rect, buf);

    let drawer_in_scrollback = app
        .global_session
        .as_ref()
        .is_some_and(|e| e.scrollback_offset > 0);
    let drawer_title = if drawer_in_scrollback {
        " Global Assistant [SCROLLBACK] (Ctrl+G to close) "
    } else {
        " Global Assistant (Ctrl+G to close) "
    };

    let block = Block::default()
        .title(drawer_title)
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());
    let inner = block.inner(drawer_rect);
    block.render(drawer_rect, buf);

    // 4. Render the global session PTY or a placeholder.
    match &app.global_session {
        Some(entry) if entry.alive => {
            if let Ok(mut parser) = entry.parser.lock() {
                // Same clamp as draw_pane_output - see comment there.
                let rows = parser.screen().size().0 as usize;
                let clamped = entry.scrollback_offset.min(rows);
                parser.set_scrollback(clamped);
                let pseudo_term = PseudoTerminal::new(parser.screen());
                pseudo_term.render(inner, buf);
                if let Some(ref sel) = entry.selection {
                    render_selection_overlay(buf, inner, sel);
                }
            } else {
                let text = Text::from(vec![Line::from(""), Line::from("  [render error]")]);
                let paragraph = Paragraph::new(text).style(theme.style_error());
                paragraph.render(inner, buf);
            }
        }
        Some(_) => {
            // Session is dead.
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Global assistant session ended."),
                Line::from("  Press Ctrl+G to restart."),
            ]);
            let paragraph = Paragraph::new(text).style(theme.style_text_muted());
            paragraph.render(inner, buf);
        }
        None => {
            let text = Text::from(vec![
                Line::from(""),
                Line::from("  Starting global assistant..."),
            ]);
            let paragraph = Paragraph::new(text).style(theme.style_text_muted());
            paragraph.render(inner, buf);
        }
    }
}

/// Render a selection highlight overlay on top of already-rendered terminal content.
///
/// For each cell in the selection range, the style modifier is set to
/// `Modifier::REVERSED` which inverts fg/bg to show the selection, matching
/// standard terminal emulator highlighting.
///
/// The inclusive `(start, end)` bounds come from
/// `SelectionState::normalized_bounds`, which is also the normalization
/// used by `selection_to_vt100_bounds` in `src/event.rs` when the same
/// selection is copied to the clipboard. Sharing that helper is what
/// keeps the visible highlight and the copied text covering the same
/// range of cells; see the regression test
/// `event::selection_clipboard_tests::highlight_cell_count_matches_clipboard_chars`.
pub(crate) fn render_selection_overlay(
    buf: &mut Buffer,
    inner_area: Rect,
    selection: &SelectionState,
) {
    let (start_row, start_col, end_row, end_col) = selection.normalized_bounds();

    let max_col = inner_area.width;

    for row in start_row..=end_row {
        if row >= inner_area.height {
            break;
        }

        let col_start = if row == start_row { start_col } else { 0 };
        let col_end = if row == end_row {
            end_col
        } else {
            max_col.saturating_sub(1)
        };
        // Single-row selection: start_col to end_col.
        // (Already handled by the above logic since start_row == end_row.)

        for col in col_start..=col_end {
            if col >= max_col {
                break;
            }
            let x = inner_area.x + col;
            let y = inner_area.y + row;
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

/// Draw the work-item context bar showing title, stage, repo name, and labels.
fn draw_context_bar(buf: &mut Buffer, ctx: &WorkItemContext, theme: &Theme, area: Rect) {
    let labels_part = if ctx.labels.is_empty() {
        String::new()
    } else {
        format!(" | {}", ctx.labels.join(", "))
    };

    let full = format!(
        "{} | [{}] | {}{}",
        ctx.title, ctx.stage, ctx.repo_name, labels_part
    );

    // Truncate to fit width. Use char-based indexing for multi-byte safety.
    let width = area.width as usize;
    let display = if full.chars().count() > width {
        if width > 3 {
            let truncated: String = full.chars().take(width - 3).collect();
            format!("{truncated}...")
        } else {
            full.chars().take(width).collect()
        }
    } else {
        full
    };

    let paragraph = Paragraph::new(display).style(theme.style_context());
    paragraph.render(area, buf);
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
fn draw_settings_overlay(buf: &mut Buffer, app: &mut App, theme: &Theme, area: Rect) {
    // Dim the background so the overlay is the clear focal point.
    dim_background(buf, area);

    let popup = centered_rect(70, 80, area);
    Clear.render(popup, buf);

    let block = Block::default()
        .title(" Settings (press ? or Esc to close) ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());

    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // Add 1-cell padding inside the overlay border on all sides.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };

    // Top-level layout: tab bar (1 row) + body (rest).
    let [tab_bar_area, body_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .areas(inner);

    // Tab bar.
    let tab_selected = match app.settings_tab {
        SettingsTab::Repos => 0,
        SettingsTab::ReviewGate => 1,
        SettingsTab::Keybindings => 2,
    };
    let tabs = Tabs::new(vec![" Repos ", " Review Gate ", " Keybindings "])
        .select(tab_selected)
        .style(theme.style_text_muted())
        .highlight_style(theme.style_view_mode_tab_active())
        .divider("|");
    tabs.render(tab_bar_area, buf);

    // Body layout: content (fills) + hint line (1 row).
    let [content_area, hint_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .areas(body_area);

    match app.settings_tab {
        SettingsTab::Keybindings => {
            draw_settings_keybindings_tab(buf, app, theme, content_area);
            let hint = Line::styled(
                "Tab: switch tab   Up/Down: scroll   ?: close",
                theme.style_text_muted(),
            );
            Paragraph::new(hint).render(hint_area, buf);
        }
        SettingsTab::ReviewGate => {
            draw_settings_review_gate_tab(buf, app, theme, content_area);
            let hint = if app.settings_review_skill_editing {
                Line::styled("Enter: save   Esc: cancel", theme.style_text_muted())
            } else {
                Line::styled(
                    "Tab: switch tab   Enter: edit   ?: close",
                    theme.style_text_muted(),
                )
            };
            Paragraph::new(hint).render(hint_area, buf);
        }
        SettingsTab::Repos => {
            draw_settings_repos_tab(buf, app, theme, content_area);
            let hint = Line::styled(
                "Tab: switch tab   Left/Right: switch column   Enter: move repo   Up/Down: navigate",
                theme.style_text_muted(),
            );
            Paragraph::new(hint).render(hint_area, buf);
        }
    }
}

fn draw_settings_repos_tab(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
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
            ListItem::new(format!(
                " {marker} {} ({source_label})",
                entry.path.display()
            ))
            .style(theme.style_text()),
        );
    }

    // Build available repo items (discovered but not managed).
    let available_entries = app.available_repos();
    let mut available_items: Vec<ListItem<'_>> = Vec::new();
    for entry in &available_entries {
        let marker = if entry.git_dir_present { "+" } else { "-" };
        available_items.push(
            ListItem::new(format!(" {marker} {}", entry.path.display())).style(theme.style_text()),
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

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(source_height),
            Constraint::Length(base_dirs_height),
            Constraint::Length(repos_section_height),
            Constraint::Length(1), // blank
            Constraint::Length(defaults_height),
            Constraint::Min(0), // absorb remaining space
        ])
        .split(area);

    // Section 0: Config source.
    let source_text = Text::from(vec![
        Line::styled("Config source:", theme.style_heading()),
        Line::from(format!("  {}", app.config.source)),
    ]);
    Paragraph::new(source_text).render(sections[0], buf);

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
    Paragraph::new(Text::from(base_lines)).render(sections[1], buf);

    // Section 2: Repos - horizontal split of Managed and Available lists.
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
    let managed_title = format!(" Managed repos ({managed_count}) ");
    let managed_block = Block::default()
        .title(managed_title.as_str())
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(managed_border);

    if managed_items.is_empty() {
        let empty =
            Paragraph::new(Line::styled("  (none)", theme.style_text_muted())).block(managed_block);
        empty.render(repo_cols[0], buf);
    } else {
        let list = List::new(managed_items)
            .block(managed_block)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if app.settings_list_focus == SettingsListFocus::Managed {
            state.select(Some(
                app.settings_repo_selected
                    .min(managed_count.saturating_sub(1)),
            ));
        }
        StatefulWidget::render(list, repo_cols[0], buf, &mut state);
    }

    // Available repos list (right).
    let available_border = if app.settings_list_focus == SettingsListFocus::Available {
        theme.style_border_focused()
    } else {
        theme.style_border_subtle()
    };
    let available_title = format!(" Available ({available_count}) ");
    let available_block = Block::default()
        .title(available_title.as_str())
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(available_border);

    if available_items.is_empty() {
        let empty = Paragraph::new(Line::styled("  (none)", theme.style_text_muted()))
            .block(available_block);
        empty.render(repo_cols[1], buf);
    } else {
        let list = List::new(available_items)
            .block(available_block)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if app.settings_list_focus == SettingsListFocus::Available {
            state.select(Some(
                app.settings_available_selected
                    .min(available_count.saturating_sub(1)),
            ));
        }
        StatefulWidget::render(list, repo_cols[1], buf, &mut state);
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
    Paragraph::new(defaults_text).render(sections[4], buf);
}

fn draw_settings_review_gate_tab(buf: &mut Buffer, app: &mut App, theme: &Theme, area: Rect) {
    // Layout: heading (1) + blank (1) + label (1) + input (1) + blank (1) + description.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // heading
            Constraint::Length(1), // blank
            Constraint::Length(1), // label
            Constraint::Length(1), // value / input field
            Constraint::Length(1), // blank
            Constraint::Min(0),    // description
        ])
        .split(area);

    let heading = Line::styled("Review Gate Skill", theme.style_heading());
    Paragraph::new(heading).render(rows[0], buf);

    let label = Line::styled("Skill (slash command):", theme.style_text());
    Paragraph::new(label).render(rows[2], buf);

    if app.settings_review_skill_editing {
        // Render with rat-widget's TextInput so the caret is drawn by the
        // same stateful widget used by the Create Work Item dialog.
        app.settings_review_skill_input.focus.set(true);
        StatefulWidget::render(
            TextInput::new().styles(create_dialog_text_style(theme)),
            rows[3],
            buf,
            &mut app.settings_review_skill_input,
        );
    } else {
        // Show the current value; mirror the unfocused single-line style.
        let value = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                app.config.defaults.review_skill.as_str(),
                theme.style_text(),
            ),
        ]);
        Paragraph::new(value).render(rows[3], buf);
    }

    let desc = Text::from(vec![
        Line::styled(
            "The slash command passed to `claude --print -p` during the review gate.",
            theme.style_text_muted(),
        ),
        Line::styled(
            "Default: /claude-adversarial-review",
            theme.style_text_muted(),
        ),
    ]);
    Paragraph::new(desc).render(rows[5], buf);
}

fn draw_settings_keybindings_tab(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    let h = theme.style_heading();
    let k = theme.style_text(); // key name style
    let d = theme.style_text_muted(); // description style

    let binding = |key: &'static str, desc: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {key:<26}"), k),
            Span::styled(desc, d),
        ])
    };

    let lines: Vec<Line<'_>> = vec![
        Line::styled("Global", h),
        binding("Ctrl+N", "Quick-start session"),
        binding("Ctrl+B", "New backlog ticket"),
        binding("Ctrl+G", "Global assistant"),
        binding("Ctrl+R", "Refresh GitHub data"),
        binding("Ctrl+\\", "Cycle Session <-> Terminal tab"),
        binding("?", "Settings / keybindings (this overlay)"),
        binding("Q / Ctrl+Q", "Quit"),
        Line::from(""),
        Line::styled("List focused", h),
        binding("Up / Down", "Navigate items"),
        binding("Enter", "Open session / Import"),
        binding("Shift+Right", "Advance stage"),
        binding("Shift+Left", "Retreat stage"),
        binding("Ctrl+D / Delete", "Delete work item"),
        binding("o", "Open PR in default browser"),
        binding("Ctrl+]", "Focus session panel"),
        Line::from(""),
        Line::styled("Board view", h),
        binding("Left / Right", "Move between columns"),
        binding("Shift+Left / Shift+Right", "Move item to adjacent column"),
        binding("Up / Down", "Navigate within column"),
        binding("Enter", "Open drill-down / session"),
        Line::from(""),
        Line::styled("Session active (right panel)", h),
        binding("Ctrl+]", "Return to item list"),
        Line::from(vec![Span::styled(
            "  (all other keys forwarded to the session)",
            d,
        )]),
        Line::from(""),
        Line::styled("Creation dialog  (Ctrl+B)", h),
        binding("Tab / Shift+Tab", "Cycle fields"),
        binding("Enter", "Create  (newline in description)"),
        binding("Space", "Toggle repo selection"),
        binding("Esc", "Cancel"),
        Line::from(""),
        Line::styled("Settings overlay  (?)", h),
        binding("Tab", "Switch tab (Repos / Keybindings)"),
        binding("Left / Right", "Switch column focus  (Repos tab)"),
        binding("Up / Down", "Navigate / scroll"),
        binding("Enter", "Move repo in or out of managed"),
        binding("? / Esc", "Close"),
    ];

    Paragraph::new(Text::from(lines))
        .scroll((app.settings_keybindings_scroll, 0))
        .render(area, buf);
}

/// Draw the work item creation dialog as a centered popup overlay.
///
/// Layout:
///   +-- Create Work Item ---------------------------------+
///   |                                                     |
///   |  Title:                                             |
///   |  [_______________________________________________]  |
///   |                                                     |
///   |  Repos:                                             |
///   |  [x] /path/to/repo-a                                |
///   |  [ ] /path/to/repo-b                                |
///   |                                                     |
///   |  Branch (optional):                                 |
///   |  [_______________________________________________]  |
///   |                                                     |
///   |  [error message if any]                             |
///   |  Enter: Create  |  Esc: Cancel  |  Tab: Next field  |
///   +-----------------------------------------------------+
/// Height of the description text area (visible lines).
///
/// Six rows gives enough vertical room to show wrapped multi-line
/// descriptions without immediately scrolling on the first few lines of
/// typing. When content exceeds this height the underlying
/// `rat_widget::textarea::TextArea` scrolls vertically (a scrollbar is
/// wired through `Scroll::new`), so long descriptions are still fully
/// editable.
pub const DESC_TEXTAREA_HEIGHT: u16 = 6;

fn draw_create_dialog(buf: &mut Buffer, dialog: &mut CreateDialog, theme: &Theme, area: Rect) {
    // Dim the background so the dialog is the clear focal point.
    dim_background(buf, area);

    if dialog.quickstart_mode {
        draw_quickstart_dialog(buf, dialog, theme, area);
        return;
    }

    // Compute dialog height based on content.
    // Rows: border(1) + blank(1) + "Title:" label(1) + input(1) + blank(1)
    //   + "Description:" label(1) + textarea(DESC_TEXTAREA_HEIGHT) + blank(1)
    //   + "Repos:" label(1) + repo_lines(max 6) + blank(1)
    //   + "Branch:" label(1) + input(1) + blank(1)
    //   + error_line(1) + hint(1) + border(1)
    let repo_lines = dialog.repo_list.len().clamp(1, 6) as u16;
    let dialog_height =
        2 + 2 + 1 + 1 + DESC_TEXTAREA_HEIGHT + 1 + 1 + repo_lines + 1 + 2 + 1 + 2 + 2;
    let dialog_width = (area.width * 60 / 100).max(40).min(area.width);

    let popup = centered_rect_fixed(dialog_width, dialog_height, area);
    Clear.render(popup, buf);

    let block = Block::default()
        .title(" Create Work Item ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());

    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // Inner area with 1-cell padding.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                    // [0] Title label
            Constraint::Length(1),                    // [1] Title input
            Constraint::Length(1),                    // [2] blank
            Constraint::Length(1),                    // [3] Description label
            Constraint::Length(DESC_TEXTAREA_HEIGHT), // [4] Description textarea
            Constraint::Length(1),                    // [5] blank
            Constraint::Length(1),                    // [6] Repos label
            Constraint::Length(repo_lines),           // [7] Repos list
            Constraint::Length(1),                    // [8] blank
            Constraint::Length(1),                    // [9] Branch label
            Constraint::Length(1),                    // [10] Branch input
            Constraint::Length(1),                    // [11] blank
            Constraint::Length(1),                    // [12] error / blank
            Constraint::Length(1),                    // [13] hint line
            Constraint::Min(0),                       // [14] absorb remaining
        ])
        .split(inner);

    // Title label
    let title_label_style = if dialog.focus_field == CreateDialogFocus::Title {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Title:", title_label_style)).render(sections[0], buf);

    // Title input (rat_widget::text_input::TextInput).
    // Sync focus flag to dialog focus state before rendering.
    dialog
        .title_input
        .focus
        .set(dialog.focus_field == CreateDialogFocus::Title);
    StatefulWidget::render(
        TextInput::new().styles(create_dialog_text_style(theme)),
        sections[1],
        buf,
        &mut dialog.title_input,
    );

    // Description label
    let desc_label_style = if dialog.focus_field == CreateDialogFocus::Description {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Description (optional):", desc_label_style))
        .render(sections[3], buf);

    // Description textarea (rat_widget::textarea::TextArea).
    // - `TextWrap::Word(2)` wraps long descriptions at word boundaries,
    //   preferring breaks in the last two columns before the right margin.
    // - `Scroll::new()` on the vertical axis wires a scrollbar and lets
    //   the textarea scroll when content exceeds DESC_TEXTAREA_HEIGHT.
    dialog
        .description_input
        .focus
        .set(dialog.focus_field == CreateDialogFocus::Description);
    StatefulWidget::render(
        TextArea::new()
            .text_wrap(TextWrap::Word(2))
            .vscroll(Scroll::new())
            .styles(create_dialog_text_style(theme)),
        sections[4],
        buf,
        &mut dialog.description_input,
    );

    // Repos label
    let repos_label_style = if dialog.focus_field == CreateDialogFocus::Repos {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Repos:", repos_label_style)).render(sections[6], buf);

    // Repos list
    if dialog.repo_list.is_empty() {
        let msg = Line::styled("  (no repos configured)", theme.style_text_muted());
        Paragraph::new(msg).render(sections[7], buf);
    } else {
        let items: Vec<ListItem<'_>> = dialog
            .repo_list
            .iter()
            .map(|(path, selected)| {
                let marker = if *selected { "[x]" } else { "[ ]" };
                let line = format!(" {marker} {}", path.display());
                ListItem::new(Line::from(line)).style(theme.style_text())
            })
            .collect();

        let list = List::new(items)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");

        let mut state = ListState::default();
        if dialog.focus_field == CreateDialogFocus::Repos {
            state.select(Some(dialog.repo_cursor));
        }

        StatefulWidget::render(list, sections[7], buf, &mut state);
    }

    // Branch label
    let branch_label_style = if dialog.focus_field == CreateDialogFocus::Branch {
        theme.style_heading()
    } else {
        theme.style_text()
    };
    Paragraph::new(Line::styled("Branch (optional):", branch_label_style)).render(sections[9], buf);

    // Branch input (rat_widget::text_input::TextInput).
    dialog
        .branch_input
        .focus
        .set(dialog.focus_field == CreateDialogFocus::Branch);
    StatefulWidget::render(
        TextInput::new().styles(create_dialog_text_style(theme)),
        sections[10],
        buf,
        &mut dialog.branch_input,
    );

    // Error message (if any)
    if let Some(ref err) = dialog.error_message {
        Paragraph::new(Line::styled(err.as_str(), theme.style_error())).render(sections[12], buf);
    }

    // Hint line
    let hint = Line::styled(
        "Enter: Create | Esc: Cancel | Tab: Next field | Space: Toggle repo",
        theme.style_text_muted(),
    );
    Paragraph::new(hint).render(sections[13], buf);
}

/// Render the compact "Quick start - select repo" dialog opened by Ctrl+N
/// when more than one managed repo is configured.
///
/// Unlike the full create dialog (Ctrl+B), this view shows only the repo
/// list - no Title, Description, or Branch fields. The work item's title is
/// hardcoded to `QUICKSTART_TITLE` and its branch is auto-generated by
/// `App::create_quickstart_work_item_for_repo`; the agent later renames the
/// title via `workbridge_set_title`.
fn draw_quickstart_dialog(buf: &mut Buffer, dialog: &mut CreateDialog, theme: &Theme, area: Rect) {
    // Compute dialog height: border(1) + padding(1) + Repos label(1)
    //   + repo_lines + blank(1) + error(1) + hint(1) + padding(1) + border(1).
    // Allow up to 8 visible repo rows (the dialog is otherwise small).
    let repo_lines = dialog.repo_list.len().clamp(1, 8) as u16;
    let dialog_height = 1 + 1 + 1 + repo_lines + 1 + 1 + 1 + 1 + 1;
    let dialog_width = (area.width * 60 / 100).max(40).min(area.width);

    let popup = centered_rect_fixed(dialog_width, dialog_height, area);
    Clear.render(popup, buf);

    let block = Block::default()
        .title(" Quick start - select repo ")
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_style(theme.style_border_overlay());

    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // Inner area with 1-cell padding on each side.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),          // [0] Repos label
            Constraint::Length(repo_lines), // [1] Repos list
            Constraint::Length(1),          // [2] blank
            Constraint::Length(1),          // [3] error / blank
            Constraint::Length(1),          // [4] hint line
            Constraint::Min(0),             // [5] absorb remaining
        ])
        .split(inner);

    // Repos label - always rendered as the focused-style heading because
    // the repo list is the only focusable field in quick-start mode.
    Paragraph::new(Line::styled("Repos:", theme.style_heading())).render(sections[0], buf);

    // Repos list
    if dialog.repo_list.is_empty() {
        let msg = Line::styled("  (no repos configured)", theme.style_text_muted());
        Paragraph::new(msg).render(sections[1], buf);
    } else {
        let items: Vec<ListItem<'_>> = dialog
            .repo_list
            .iter()
            .map(|(path, selected)| {
                let marker = if *selected { "[x]" } else { "[ ]" };
                let line = format!(" {marker} {}", path.display());
                ListItem::new(Line::from(line)).style(theme.style_text())
            })
            .collect();

        let list = List::new(items)
            .highlight_style(theme.style_tab_highlight())
            .highlight_symbol("> ");

        let mut state = ListState::default();
        state.select(Some(dialog.repo_cursor));

        StatefulWidget::render(list, sections[1], buf, &mut state);
    }

    // Error message (if any)
    if let Some(ref err) = dialog.error_message {
        Paragraph::new(Line::styled(err.as_str(), theme.style_error())).render(sections[3], buf);
    }

    // Hint line - quickstart-specific, no Tab/Title/Description guidance.
    let hint = Line::styled(
        "Enter: Create | Esc: Cancel | Up/Down: Move | Space: Select repo",
        theme.style_text_muted(),
    );
    Paragraph::new(hint).render(sections[4], buf);
}

/// Build the shared `TextStyle` used by the Create Work Item dialog's
/// text fields (`TextInput` for Title / Branch, `TextArea` for
/// Description).
///
/// - `style` is the base text color (plain, not dimmed).
/// - `focus` is left at the base style so focused fields don't visually
///   change the run of text itself; the adjacent label (e.g. `Title:`)
///   already switches to the heading color when the field has focus.
/// - `cursor` uses the tab-highlight foreground/background so the caret
///   block is visible against the terminal's default background. This
///   is only honoured when the rat-text cursor type is
///   `RenderedCursor`; see [`ensure_rendered_cursor`].
fn create_dialog_text_style(theme: &Theme) -> TextStyle {
    ensure_rendered_cursor();
    let base = theme.style_text();
    let cursor = ratatui_core::style::Style::default()
        .fg(theme.tab_highlight_fg)
        .bg(theme.tab_highlight_bg);
    TextStyle {
        style: base,
        focus: Some(base),
        cursor: Some(cursor),
        ..Default::default()
    }
}

/// Configure rat-text to render the cursor into the ratatui `Buffer`
/// instead of driving the terminal cursor. Called from the text-style
/// helper so it is applied before the first dialog render - after that
/// the atomic store is a no-op. Keeps tests deterministic (the
/// `TestBackend` does not have a real terminal cursor).
fn ensure_rendered_cursor() {
    use rat_widget::text::cursor::{CursorType, set_cursor_type};
    set_cursor_type(CursorType::RenderedCursor);
}

/// Return a centered rect with fixed width and height within the outer rect.
fn centered_rect_fixed(width: u16, height: u16, outer: Rect) -> Rect {
    let w = width.min(outer.width);
    let h = height.min(outer.height);
    let x = outer.x + (outer.width.saturating_sub(w)) / 2;
    let y = outer.y + (outer.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Dim every cell in the buffer area to visually push content behind an overlay.
///
/// Applies `Modifier::DIM` and overrides foreground to `Color::DarkGray`. The
/// dual approach is necessary because DIM alone does not reliably dim borders
/// and colored elements on all terminals.
fn dim_background(buf: &mut Buffer, area: Rect) {
    let dim_fg = ratatui_core::style::Color::DarkGray;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                let style = cell.style().add_modifier(Modifier::DIM).fg(dim_fg);
                cell.set_style(style);
            }
        }
    }
}

/// Content variants for prompt dialogs.
///
/// `KeyChoice` presents a question with labelled key options.
/// `TextInput` presents a text field with a hint line.
enum PromptDialogKind<'a> {
    KeyChoice {
        title: &'a str,
        body: &'a str,
        options: &'a [(&'a str, &'a str)],
    },
    TextInput {
        title: &'a str,
        body: &'a str,
        input: &'a mut rat_widget::text_input::TextInputState,
        hint: &'a str,
    },
    /// Red-bordered alert for errors/warnings. Dismissed with Enter or Esc.
    Alert { title: &'a str, body: &'a str },
}

/// Draw a modal prompt dialog centered on screen with a dimmed background.
///
/// Prompt dialogs use `BorderType::Rounded` to be visually distinct from
/// other overlays (settings, create dialog) which use plain borders.
fn draw_prompt_dialog(buf: &mut Buffer, theme: &Theme, area: Rect, kind: PromptDialogKind<'_>) {
    // 1. Dim the entire background so the dialog is the clear focal point.
    dim_background(buf, area);

    // 2. Compute dialog dimensions.
    let (title, body, inner_height) = match &kind {
        PromptDialogKind::KeyChoice {
            title,
            body,
            options,
        } => {
            // body(1) + blank(1) + options(N) + blank(1)
            let h = 1u16 + 1 + options.len() as u16 + 1;
            (*title, *body, h)
        }
        PromptDialogKind::TextInput {
            title, body, hint, ..
        } => {
            // body(1) + blank(1) + input(1) + blank(1) + hint(1)
            let _ = hint;
            let h = 1u16 + 1 + 1 + 1 + 1;
            (*title, *body, h)
        }
        PromptDialogKind::Alert { title, body } => {
            // Height is computed after dialog_width is known (body may wrap).
            // Use 0 as placeholder; overridden below for Alert.
            (*title, *body, 0u16)
        }
    };

    // Minimum width: longest line + 2 (padding) + 2 (border).
    // Clamp between 40 and 60, further clamped to terminal width.
    let min_content_width = body.len().max(title.len() + 4) as u16;
    let dialog_width = (min_content_width + 4).clamp(40, 60).min(area.width);

    // For Alert dialogs, compute body line count based on actual word-wrapping.
    let inner_height = if matches!(kind, PromptDialogKind::Alert { .. }) {
        // Usable content width: dialog - 2 (border) - 2 (padding).
        let content_width = dialog_width.saturating_sub(4).max(1) as usize;
        let body_lines = if body.is_empty() {
            1u16
        } else {
            // Use word-wrap simulation to get accurate line count.
            // wrap_text_flat breaks at word boundaries like ratatui's Wrap.
            (wrap_text_flat(body, content_width).len() as u16).max(1)
        };
        // body(N) + blank(1) + hint(1) + blank(1)
        body_lines + 1 + 1 + 1
    } else {
        inner_height
    };
    // Height: border(2) + blank(1) + inner_height + blank(1) = inner_height + 4.
    let dialog_height = (inner_height + 4).min(area.height);

    // 3. Center and clear the popup area.
    let popup = centered_rect_fixed(dialog_width, dialog_height, area);
    Clear.render(popup, buf);

    // 4. Draw rounded-border block. Alert dialogs use a red border;
    //    all other prompt dialogs use the standard cyan overlay border.
    let border_style = match &kind {
        PromptDialogKind::Alert { .. } => theme.style_border_alert(),
        _ => theme.style_border_overlay(),
    };
    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(theme.style_title())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style);
    let block_inner = block.inner(popup);
    block.render(popup, buf);

    // 5. 1-cell padding inside the border.
    let inner = Rect {
        x: block_inner.x + 1,
        y: block_inner.y + 1,
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // 6. Render content rows using a vertical layout.
    match kind {
        PromptDialogKind::KeyChoice { body, options, .. } => {
            // Rows: body, blank, option*N, blank.
            let mut constraints = vec![
                Constraint::Length(1), // body
                Constraint::Length(1), // blank
            ];
            for _ in options {
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Min(0)); // remaining space

            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(inner);

            Paragraph::new(body)
                .style(theme.style_text())
                .render(rows[0], buf);
            // rows[1] is blank.
            for (i, (key_label, description)) in options.iter().enumerate() {
                let line = Line::from(vec![
                    Span::styled(*key_label, theme.style_heading()),
                    Span::raw("  "),
                    Span::styled(*description, theme.style_text()),
                ]);
                Paragraph::new(line).render(rows[2 + i], buf);
            }
        }
        PromptDialogKind::TextInput {
            body, input, hint, ..
        } => {
            // Rows: body, blank, input, blank, hint.
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // body
                    Constraint::Length(1), // blank
                    Constraint::Length(1), // input field
                    Constraint::Length(1), // blank
                    Constraint::Length(1), // hint
                    Constraint::Min(0),    // remaining
                ])
                .split(inner);

            Paragraph::new(body)
                .style(theme.style_text())
                .render(rows[0], buf);
            // rows[1] is blank.
            // Focused prompt input: render with rat-widget's TextInput so
            // the caret is drawn by the same stateful widget used by the
            // Create Work Item dialog (no custom single-line widget).
            input.focus.set(true);
            StatefulWidget::render(
                TextInput::new().styles(create_dialog_text_style(theme)),
                rows[2],
                buf,
                input,
            );
            // rows[3] is blank.
            Paragraph::new(hint)
                .style(theme.style_text_muted())
                .render(rows[4], buf);
        }
        PromptDialogKind::Alert { body, .. } => {
            // Compute wrapped body line count for layout.
            let content_w = inner.width.max(1) as usize;
            let body_lines = if body.is_empty() {
                1u16
            } else {
                // Use word-wrap simulation for accurate line count.
                (wrap_text_flat(body, content_w).len() as u16).max(1)
            };
            // Rows: body (may wrap to multiple lines), blank, hint.
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(body_lines), // body
                    Constraint::Length(1),          // blank
                    Constraint::Length(1),          // hint
                    Constraint::Min(0),             // remaining
                ])
                .split(inner);

            Paragraph::new(body)
                .style(theme.style_error())
                .wrap(Wrap { trim: false })
                .render(rows[0], buf);
            // rows[1] is blank.
            let hint_line = Line::from(vec![
                Span::styled("[Enter/Esc]", theme.style_heading()),
                Span::raw("  "),
                Span::styled("OK", theme.style_text()),
            ]);
            Paragraph::new(hint_line).render(rows[2], buf);
        }
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_text;

    /// Every output line must fit within max_width (measured in display columns).
    fn assert_all_lines_fit(lines: &[String], max_width: usize) {
        use unicode_width::UnicodeWidthStr;
        for (i, line) in lines.iter().enumerate() {
            let display_width = line.width();
            assert!(
                display_width <= max_width,
                "line {i} is {display_width} cols but max_width is {max_width}: {:?}",
                line,
            );
        }
    }

    #[test]
    fn short_string_no_wrap() {
        let result = wrap_text("hello", 20);
        assert_eq!(result, vec!["hello"]);
        assert_all_lines_fit(&result, 20);
    }

    #[test]
    fn exact_fit_no_wrap() {
        let s = "exactly twenty chars";
        assert_eq!(s.len(), 20);
        let result = wrap_text(s, 20);
        assert_eq!(result.len(), 1);
        assert_all_lines_fit(&result, 20);
    }

    #[test]
    fn wraps_at_word_boundary() {
        let result = wrap_text("  hello world foo bar", 14);
        assert_all_lines_fit(&result, 14);
        assert!(result.len() >= 2, "should wrap: {:?}", result);
    }

    #[test]
    fn wraps_at_slash_boundary() {
        // Simulates a branch like "janiskirsteins/agent-specific-labels"
        let result = wrap_text("  janiskirsteins/agent-specific-labels (walleyboard)", 25);
        assert_all_lines_fit(&result, 25);
        assert!(result.len() >= 2, "should wrap: {:?}", result);
    }

    #[test]
    fn continuation_lines_indented() {
        let result = wrap_text("  long content that must wrap to next line", 20);
        assert_all_lines_fit(&result, 20);
        if result.len() > 1 {
            assert!(
                result[1].starts_with("    "),
                "continuation should be indented: {:?}",
                result[1]
            );
        }
    }

    #[test]
    fn all_content_preserved() {
        let input = "  [no branch] (workbridge) [no wt]";
        let result = wrap_text(input, 20);
        assert_all_lines_fit(&result, 20);
        // All key content words must appear somewhere in the output.
        // Words may be split across lines by the wrapper.
        let flat: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["no", "branch", "workbridge", "wt"] {
            assert!(flat.contains(word), "missing '{word}': {flat}");
        }
    }

    #[test]
    fn very_narrow_width() {
        let result = wrap_text("  hello world", 8);
        assert_all_lines_fit(&result, 8);
        assert!(!result.is_empty());
    }

    #[test]
    fn empty_string() {
        let result = wrap_text("", 20);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn zero_width() {
        let result = wrap_text("hello", 0);
        assert!(result.is_empty());
    }

    #[test]
    fn realistic_workitem_narrow_panel() {
        // 23 chars inner width (100 col terminal, 25% left panel)
        let input = "  [no branch] (workbridge) [no wt]";
        let result = wrap_text(input, 23);
        assert_all_lines_fit(&result, 23);
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("[no wt]"), "must not clip [no wt]");
    }

    #[test]
    fn realistic_branch_narrow_panel() {
        let input = "  janiskirsteins/agent-specific-labels (walleyboard)";
        let result = wrap_text(input, 23);
        assert_all_lines_fit(&result, 23);
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("walleyboard"), "must not clip repo name");
    }

    #[test]
    fn multibyte_utf8_no_panic() {
        // Accented chars (2 bytes each in UTF-8), must not panic on slice
        let input = "  feature/korrektur-andern-loschen (projekt)";
        let result = wrap_text(input, 20);
        assert_all_lines_fit(&result, 20);
        assert!(!result.is_empty());
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("projekt"), "must preserve repo name");
    }

    #[test]
    fn wide_cjk_characters_respect_display_width() {
        use unicode_width::UnicodeWidthStr;
        // CJK ideographs: each is 2 display columns, 1 char, 3 bytes
        // \u{4e16}\u{754c} = 2 chars, 4 display columns
        let input = "  \u{4e16}\u{754c}/test (repo)";
        assert!(
            input.width() > input.chars().count(),
            "CJK should be wider than char count: width={}, chars={}",
            input.width(),
            input.chars().count()
        );
        let result = wrap_text(input, 12);
        assert_all_lines_fit(&result, 12);
        assert!(!result.is_empty());
    }

    #[test]
    fn emoji_display_width() {
        // \u{1f600} = grinning face, 2 display columns, 1 char, 4 bytes
        let input = "  fix/\u{1f600}bug (my-repo)";
        let result = wrap_text(input, 14);
        assert_all_lines_fit(&result, 14);
        assert!(!result.is_empty());
    }
}

#[cfg(test)]
mod wrap_variant_tests {
    use super::{wrap_text_flat, wrap_two_widths};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wrap_text_flat_no_indent_on_continuation() {
        let result = wrap_text_flat("hello world foo bar baz", 12);
        assert_eq!(result[0], "hello world ");
        // Continuation has no leading spaces
        assert!(!result[1].starts_with(' '));
    }

    #[test]
    fn wrap_two_widths_first_line_narrow() {
        // First line budget 10, rest budget 25
        let result = wrap_two_widths(
            "Add Kanban board view with column-based work item organization",
            10,
            25,
        );
        // First line fits within 10 columns
        assert!(
            result[0].width() <= 10,
            "first line too wide: {:?}",
            result[0]
        );
        // Continuation lines use the wider budget
        for line in result.iter().skip(1) {
            assert!(line.width() <= 25, "continuation too wide: {:?}", line);
        }
        // All words present
        let joined: String = result.join(" ");
        assert!(joined.contains("organization"));
    }

    #[test]
    fn wrap_two_widths_fits_first_line() {
        let result = wrap_two_widths("Short title", 20, 40);
        assert_eq!(result, vec!["Short title"]);
    }

    #[test]
    fn wrap_two_widths_empty() {
        let result = wrap_two_widths("", 10, 20);
        assert!(result.is_empty());
    }
}

#[cfg(test)]
mod format_entry_tests {
    use super::format_work_item_entry;
    use crate::app::{App, StubBackend};
    use crate::theme::Theme;
    use crate::work_item::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use unicode_width::UnicodeWidthStr;

    /// Render a ListItem to a string by putting it in a List widget and
    /// rendering to a buffer.
    fn render_list_item_to_string(
        item: ratatui_widgets::list::ListItem<'_>,
        width: usize,
    ) -> String {
        use ratatui_core::buffer::Buffer;
        use ratatui_core::layout::Rect;
        use ratatui_core::widgets::Widget;
        let height = item.height() as u16;
        let area = Rect::new(0, 0, width as u16, height);
        let list = ratatui_widgets::list::List::new(vec![item]);
        let mut buf = Buffer::empty(area);
        list.render(area, &mut buf);
        let mut lines = Vec::new();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width as u16 {
                if let Some(cell) = buf.cell((x, y)) {
                    line.push_str(cell.symbol());
                }
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// Render a `ListItem` into a `Buffer` through the same `StatefulWidget`
    /// path the real left-panel list uses, so tests can inspect per-cell
    /// `fg`/`bg`. Unlike `render_list_item_to_string`, this applies the
    /// `highlight_style` (`style_tab_highlight_bg`) that the actual list
    /// renderer uses at `ui.rs` `style_tab_highlight_bg` call site, which is
    /// what makes a selected row's background Cyan - exactly the context
    /// needed to catch the Cyan-on-Cyan spinner regression.
    fn render_list_item_to_buffer(
        item: ratatui_widgets::list::ListItem<'_>,
        width: usize,
        selected: bool,
    ) -> ratatui_core::buffer::Buffer {
        use ratatui_core::buffer::Buffer;
        use ratatui_core::layout::Rect;
        use ratatui_core::widgets::StatefulWidget;
        let height = item.height() as u16;
        let area = Rect::new(0, 0, width as u16, height);
        let list = ratatui_widgets::list::List::new(vec![item])
            .highlight_style(Theme::default_theme().style_tab_highlight_bg());
        let mut buf = Buffer::empty(area);
        let mut state = ratatui_widgets::list::ListState::default();
        if selected {
            state.select(Some(0));
        }
        StatefulWidget::render(list, area, &mut buf, &mut state);
        buf
    }

    fn make_app_with_work_item(wi: WorkItem) -> App {
        let mut app = App::with_config(crate::config::Config::default(), Arc::new(StubBackend));
        app.work_items = vec![wi];
        app
    }

    /// Pre-planning items (no branch, no worktree) should NOT show
    /// [no branch] or [no wt] tags. They just show the repo name.
    #[test]
    fn pre_planning_item_no_tags() {
        let wi = WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Fix auth bug".to_string(),
            description: None,
            status: WorkItemStatus::Backlog,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: None,
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 40, &theme, false);
        let text = render_list_item_to_string(item, 40);

        assert!(
            !text.contains("[no branch]"),
            "should not show [no branch] tag: {text}"
        );
        assert!(
            !text.contains("[no wt]"),
            "should not show [no wt] tag: {text}"
        );
        // Repo name is now in the group header, not per-item.
        assert!(
            !text.contains("myrepo"),
            "repo should be in group header, not item: {text}"
        );
    }

    /// Work items with a branch should show branch on line 2.
    #[test]
    fn item_with_branch_shows_metadata() {
        let wi = WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Fix auth bug".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: Some("42-fix-auth".to_string()),
                worktree_path: Some(PathBuf::from("/Projects/myrepo/.worktrees/42-fix-auth")),
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 40, &theme, false);
        let text = render_list_item_to_string(item, 40);

        assert!(text.contains("42-fix-auth"), "should show branch: {text}");
        // Repo name is now in the group header, not per-item.
        assert!(!text.contains("[no wt]"), "has worktree, no tag: {text}");
    }

    /// Work items with branch but no worktree show [no wt].
    #[test]
    fn item_with_branch_no_worktree_shows_no_wt() {
        let wi = WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Planned work".to_string(),
            description: None,
            status: WorkItemStatus::Backlog,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: Some("feature-x".to_string()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 40, &theme, false);
        let text = render_list_item_to_string(item, 40);

        assert!(text.contains("feature-x"), "should show branch: {text}");
        assert!(text.contains("[no wt]"), "should show [no wt]: {text}");
    }

    /// Work items with a backend-provided `display_id` should render
    /// a dimmed `#display_id` subtitle line between the title and the
    /// branch line. Items without a `display_id` (legacy records) must
    /// NOT render any such line.
    #[test]
    fn format_entry_renders_display_id_line() {
        let wi = WorkItem {
            display_id: Some("workbridge-7".to_string()),
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Ship a thing".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/workbridge"),
                branch: Some("janiskirsteins/ship-a-thing".to_string()),
                worktree_path: Some(PathBuf::from(
                    "/Projects/workbridge/.worktrees/ship-a-thing",
                )),
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 80, &theme, false);
        let text = render_list_item_to_string(item, 80);

        // The ID line is rendered as `#workbridge-7` (octothorp +
        // slug-N form), not the bare slug.
        assert!(
            text.contains("#workbridge-7"),
            "rendered output should contain the display_id line: {text}"
        );

        // Order: title is on line 0, ID line sits above the branch
        // line, branch is below both. Split the render into lines
        // and verify the positions.
        let lines: Vec<&str> = text.lines().collect();
        let title_idx = lines
            .iter()
            .position(|l| l.contains("Ship a thing"))
            .expect("title line");
        let id_idx = lines
            .iter()
            .position(|l| l.contains("#workbridge-7"))
            .expect("id line");
        let branch_idx = lines
            .iter()
            .position(|l| l.contains("janiskirsteins/ship-a-thing"))
            .expect("branch line");
        assert!(
            title_idx < id_idx && id_idx < branch_idx,
            "order should be title ({title_idx}) -> id ({id_idx}) -> branch ({branch_idx}): {text}"
        );
    }

    #[test]
    fn format_entry_omits_id_line_when_none() {
        let wi = WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Legacy item".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/workbridge"),
                branch: Some("main".to_string()),
                worktree_path: Some(PathBuf::from("/Projects/workbridge")),
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let item = format_work_item_entry(&app, 0, 80, &theme, false);
        let text = render_list_item_to_string(item, 80);

        // Legacy items must not render any `#` line. Checking for the
        // bare `#` character is strict enough because no other UI
        // element in this entry uses it.
        assert!(
            !text.contains('#'),
            "legacy item (display_id=None) should not render any `#` line: {text}"
        );
    }

    /// Every line in the rendered item must fit within max_width.
    #[test]
    fn all_lines_fit_within_max_width() {
        let wi = WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "A very long title that should be truncated properly".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/walleyboard"),
                branch: Some("janiskirsteins/agent-specific-labels".to_string()),
                worktree_path: Some(PathBuf::from("/Projects/walleyboard")),
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let app = make_app_with_work_item(wi);
        let theme = Theme::default_theme();
        let max_width = 21; // Narrow panel (100 col terminal)
        let item = format_work_item_entry(&app, 0, max_width, &theme, false);
        let text = render_list_item_to_string(item, max_width);
        for (i, line) in text.lines().enumerate() {
            let line_width = line.width();
            assert!(
                line_width <= max_width,
                "line {i} is {line_width} cols but max is {max_width}: {:?}",
                line,
            );
        }
    }

    /// When a work item is actively working AND selected, the left-margin
    /// spinner must render in the highlight style (Black fg on Cyan bg)
    /// rather than its default Cyan fg. Otherwise the spinner is Cyan-on-Cyan
    /// and invisible on the highlight bar.
    #[test]
    fn working_spinner_is_visible_on_highlighted_row() {
        use ratatui_core::style::Color;
        let wi = WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Working item".to_string(),
            display_id: None,
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: Some("feature".to_string()),
                worktree_path: Some(PathBuf::from("/Projects/myrepo/.worktrees/feature")),
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let id = wi.id.clone();
        let mut app = make_app_with_work_item(wi);
        app.agent_working.insert(id);
        let theme = Theme::default_theme();

        let item = format_work_item_entry(&app, 0, 40, &theme, true);
        let buf = render_list_item_to_buffer(item, 40, true);
        let cell = buf.cell((0, 0)).expect("cell (0,0) exists");
        assert_eq!(
            cell.fg, theme.tab_highlight_fg,
            "spinner fg on highlighted row should be tab_highlight_fg, got {:?}",
            cell.fg
        );
        assert_eq!(
            cell.bg, theme.tab_highlight_bg,
            "spinner bg on highlighted row should be tab_highlight_bg, got {:?}",
            cell.bg
        );
        assert_ne!(
            cell.fg,
            Color::Cyan,
            "regression: spinner on highlighted row must not be Cyan (cyan-on-cyan is invisible)"
        );
    }

    /// On a non-highlighted row, the spinner keeps its default Cyan fg and
    /// no forced background.
    #[test]
    fn working_spinner_keeps_default_style_on_unselected_row() {
        let wi = WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Working item".to_string(),
            display_id: None,
            description: None,
            status: WorkItemStatus::Implementing,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/Projects/myrepo"),
                branch: Some("feature".to_string()),
                worktree_path: Some(PathBuf::from("/Projects/myrepo/.worktrees/feature")),
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            status_derived: false,
            errors: vec![],
        };
        let id = wi.id.clone();
        let mut app = make_app_with_work_item(wi);
        app.agent_working.insert(id);
        let theme = Theme::default_theme();

        let item = format_work_item_entry(&app, 0, 40, &theme, false);
        let buf = render_list_item_to_buffer(item, 40, false);
        let cell = buf.cell((0, 0)).expect("cell (0,0) exists");
        assert_eq!(
            cell.fg, theme.badge_session_working,
            "spinner fg on unselected row should remain badge_session_working"
        );
    }
}

#[cfg(test)]
mod sticky_header_tests {
    use super::find_current_group_header;
    use crate::app::{DisplayEntry, GroupHeaderKind};

    fn make_display_list() -> Vec<DisplayEntry> {
        vec![
            DisplayEntry::GroupHeader {
                label: "ACTIVE (repo)".into(),
                count: 2,
                kind: GroupHeaderKind::Normal,
            },
            DisplayEntry::WorkItemEntry(0),
            DisplayEntry::WorkItemEntry(1),
            DisplayEntry::GroupHeader {
                label: "BACKLOGGED (repo)".into(),
                count: 1,
                kind: GroupHeaderKind::Normal,
            },
            DisplayEntry::WorkItemEntry(2),
        ]
    }

    #[test]
    fn header_at_offset_zero() {
        let list = make_display_list();
        // Offset 0 is the ACTIVE header itself.
        assert_eq!(find_current_group_header(&list, 0), Some(0));
    }

    #[test]
    fn header_for_first_group_item() {
        let list = make_display_list();
        // Offset 1 is the first item under ACTIVE - header is at 0.
        assert_eq!(find_current_group_header(&list, 1), Some(0));
    }

    #[test]
    fn header_for_second_group_item() {
        let list = make_display_list();
        // Offset 2 is the second item under ACTIVE - header still at 0.
        assert_eq!(find_current_group_header(&list, 2), Some(0));
    }

    #[test]
    fn header_switches_at_second_group() {
        let list = make_display_list();
        // Offset 3 is the BACKLOGGED header - returns itself.
        assert_eq!(find_current_group_header(&list, 3), Some(3));
    }

    #[test]
    fn header_for_item_in_second_group() {
        let list = make_display_list();
        // Offset 4 is the item under BACKLOGGED - header is at 3.
        assert_eq!(find_current_group_header(&list, 4), Some(3));
    }

    #[test]
    fn empty_display_list() {
        let list: Vec<DisplayEntry> = vec![];
        assert_eq!(find_current_group_header(&list, 0), None);
    }

    #[test]
    fn no_headers_at_all() {
        let list = vec![
            DisplayEntry::WorkItemEntry(0),
            DisplayEntry::WorkItemEntry(1),
        ];
        assert_eq!(find_current_group_header(&list, 0), None);
        assert_eq!(find_current_group_header(&list, 1), None);
    }

    #[test]
    fn offset_beyond_list_length() {
        let list = make_display_list();
        // Offset far beyond the list - clamps to last valid index, finds
        // the BACKLOGGED header at index 3.
        assert_eq!(find_current_group_header(&list, 100), Some(3));
    }

    #[test]
    fn consecutive_headers_returns_nearest() {
        // Two consecutive headers (empty first group).
        let list = vec![
            DisplayEntry::GroupHeader {
                label: "EMPTY GROUP".into(),
                count: 0,
                kind: GroupHeaderKind::Normal,
            },
            DisplayEntry::GroupHeader {
                label: "POPULATED GROUP".into(),
                count: 1,
                kind: GroupHeaderKind::Normal,
            },
            DisplayEntry::WorkItemEntry(0),
        ];
        // Offset 0: finds the first header.
        assert_eq!(find_current_group_header(&list, 0), Some(0));
        // Offset 1: finds the second header (closest).
        assert_eq!(find_current_group_header(&list, 1), Some(1));
        // Offset 2: item in second group, header at 1.
        assert_eq!(find_current_group_header(&list, 2), Some(1));
    }

    #[test]
    fn blocked_header_kind_preserved() {
        let list = vec![
            DisplayEntry::GroupHeader {
                label: "BLOCKED (repo)".into(),
                count: 1,
                kind: GroupHeaderKind::Blocked,
            },
            DisplayEntry::WorkItemEntry(0),
        ];
        let idx = find_current_group_header(&list, 1).unwrap();
        assert_eq!(idx, 0);
        // Verify it's the blocked header (the caller can inspect the kind).
        match &list[idx] {
            DisplayEntry::GroupHeader { kind, .. } => {
                assert_eq!(*kind, GroupHeaderKind::Blocked);
            }
            _ => panic!("expected GroupHeader"),
        }
    }

    #[test]
    fn three_groups_scrolled_to_middle() {
        let list = vec![
            DisplayEntry::GroupHeader {
                label: "GROUP A".into(),
                count: 1,
                kind: GroupHeaderKind::Normal,
            },
            DisplayEntry::WorkItemEntry(0),
            DisplayEntry::GroupHeader {
                label: "GROUP B".into(),
                count: 2,
                kind: GroupHeaderKind::Normal,
            },
            DisplayEntry::WorkItemEntry(1),
            DisplayEntry::WorkItemEntry(2),
            DisplayEntry::GroupHeader {
                label: "GROUP C".into(),
                count: 1,
                kind: GroupHeaderKind::Normal,
            },
            DisplayEntry::WorkItemEntry(3),
        ];
        // Scrolled to second item of GROUP B (index 4).
        assert_eq!(find_current_group_header(&list, 4), Some(2));
        // Scrolled to GROUP C header (index 5).
        assert_eq!(find_current_group_header(&list, 5), Some(5));
        // Scrolled to item in GROUP C (index 6).
        assert_eq!(find_current_group_header(&list, 6), Some(5));
    }

    // -- predict_list_offset tests --
    //
    // The predictor must match `ratatui_widgets::list::List::get_items_bounds`
    // for the default `scroll_padding = 0` case. The parallel-render helper
    // below builds a real `List` widget from the same item heights and
    // renders it into a `TestBackend`, then compares `state.offset()`
    // against the predictor's output. This catches any drift between our
    // simulation and ratatui's actual math (e.g. if a future ratatui
    // version changes the algorithm).

    use super::predict_list_offset;
    use ratatui_core::{
        backend::TestBackend,
        layout::Rect,
        terminal::Terminal,
        text::{Line, Text},
        widgets::StatefulWidget,
    };
    use ratatui_widgets::list::{List, ListItem, ListState};

    /// Build a `Vec<ListItem>` where item `i` has `item_heights[i]` rows.
    /// Each row is a short, unique placeholder line so ratatui sees the
    /// heights we specified via `ListItem::height()`.
    fn items_with_heights(item_heights: &[usize]) -> Vec<ListItem<'static>> {
        item_heights
            .iter()
            .enumerate()
            .map(|(i, &h)| {
                let lines: Vec<Line<'static>> =
                    (0..h).map(|r| Line::from(format!("i{i}r{r}"))).collect();
                ListItem::new(Text::from(lines))
            })
            .collect()
    }

    /// Render a real `List` through a `TestBackend` with the given heights,
    /// prev offset, selection, and viewport height, and return the offset
    /// that ratatui chose. This is the ground truth the predictor must
    /// match.
    fn ratatui_offset(
        item_heights: &[usize],
        prev_offset: usize,
        selected: Option<usize>,
        max_height: u16,
    ) -> usize {
        // Width is arbitrary; item_heights is authoritative.
        let backend = TestBackend::new(20, max_height.max(1));
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = ListState::default().with_offset(prev_offset);
        state.select(selected);
        let items = items_with_heights(item_heights);
        terminal
            .draw(|frame| {
                let list = List::new(items);
                let area = Rect {
                    x: 0,
                    y: 0,
                    width: 20,
                    height: max_height,
                };
                StatefulWidget::render(list, area, frame.buffer_mut(), &mut state);
            })
            .unwrap();
        state.offset()
    }

    /// Assert the predictor matches ratatui's actual offset for a given case.
    fn assert_predictor_matches(
        item_heights: &[usize],
        prev_offset: usize,
        selected: Option<usize>,
        max_height: u16,
        case: &str,
    ) {
        let actual = ratatui_offset(item_heights, prev_offset, selected, max_height);
        let predicted =
            predict_list_offset(item_heights, prev_offset, selected, max_height as usize);
        assert_eq!(
            predicted, actual,
            "predictor disagreed with ratatui for case `{case}`: \
             heights={item_heights:?} prev_offset={prev_offset} \
             selected={selected:?} max_height={max_height}"
        );
    }

    #[test]
    fn predict_empty_list() {
        assert_eq!(predict_list_offset(&[], 0, None, 10), 0);
        assert_eq!(predict_list_offset(&[], 0, Some(5), 10), 0);
        assert_eq!(predict_list_offset(&[], 7, Some(0), 10), 0);
    }

    #[test]
    fn predict_zero_max_height() {
        // Degenerate: zero rows available. The predictor returns the
        // clamped prev_offset without touching ratatui (which would
        // panic). This is a defensive path; production callers never
        // pass max_height=0 because the inner area is always >= 1 row
        // when this function is called.
        assert_eq!(predict_list_offset(&[1, 2, 3], 1, Some(2), 0), 1);
    }

    #[test]
    fn predict_no_scroll_needed() {
        // Everything fits, selected item is first.
        let heights = vec![1, 2, 2, 2];
        assert_predictor_matches(&heights, 0, Some(0), 10, "no scroll, select first");
    }

    #[test]
    fn predict_selection_below_viewport_scrolls_down() {
        // Items don't fit; the selected index is past the tail, so the
        // list must scroll down.
        let heights = vec![1, 2, 2, 2, 1, 2, 2, 2, 2, 2];
        assert_predictor_matches(&heights, 0, Some(9), 8, "scroll down to last");
    }

    #[test]
    fn predict_selection_above_viewport_scrolls_up() {
        // prev_offset is deep into the list but the selection is above
        // it - must scroll back up.
        let heights = vec![2, 2, 2, 2, 2, 2];
        assert_predictor_matches(&heights, 4, Some(0), 6, "scroll up to first");
    }

    #[test]
    fn predict_variable_heights_mixed() {
        // Headers (1 row) interleaved with work items (2 rows) - the
        // bug case from the user's screenshot.
        let heights = vec![1, 2, 2, 2, 1, 2, 2, 2, 2, 2];
        assert_predictor_matches(&heights, 0, Some(9), 7, "sticky-bug layout");
        assert_predictor_matches(&heights, 0, Some(8), 8, "sticky-bug layout deeper");
    }

    #[test]
    fn predict_selection_is_first_item_of_second_group() {
        // Display list with two 1-row headers at indices 0 and 4, and
        // 2-row items elsewhere. Select the first item under the second
        // header - exactly the scenario from the regression test.
        let heights = vec![1, 2, 2, 2, 1, 2, 2];
        assert_predictor_matches(&heights, 0, Some(5), 8, "first item of second group, fits");
        // Same list with a shorter viewport.
        assert_predictor_matches(
            &heights,
            0,
            Some(5),
            5,
            "first item of second group, short viewport",
        );
    }

    #[test]
    fn predict_last_index_from_offset_zero() {
        let heights = vec![3, 3, 3, 3, 3];
        assert_predictor_matches(&heights, 0, Some(4), 6, "last item, tight");
    }

    #[test]
    fn predict_selection_none_resets_offset_like_ratatui() {
        // ratatui's `ListState::select(None)` also resets the offset to 0.
        // The production call path renders with select() after with_offset,
        // so when no item is selected the effective offset is 0 regardless
        // of what prev_offset we pass in. The predictor must match.
        let heights = vec![1, 1, 1, 1, 1, 1, 1, 1];
        assert_predictor_matches(&heights, 3, None, 4, "no selection, reset to 0");
        let predicted = predict_list_offset(&heights, 3, None, 4);
        assert_eq!(predicted, 0);
    }

    #[test]
    fn predict_single_item_fits() {
        let heights = vec![1];
        assert_predictor_matches(&heights, 0, Some(0), 3, "single item");
    }

    #[test]
    fn predict_offset_past_end_is_clamped() {
        // prev_offset exceeds the list length. ratatui clamps to
        // items.len()-1 before doing any work; the predictor must match.
        let heights = vec![1, 1, 1];
        assert_predictor_matches(&heights, 99, Some(0), 3, "offset past end");
    }

    // -- recenter_offset / compute_max_item_offset tests --
    //
    // These exercise the pure viewport math added for mouse-wheel
    // scrolling. The renderer itself is covered by the snapshot
    // tests; these tests isolate the centering / clamping logic so a
    // regression in either is pinpointed without a full render round
    // trip.

    use super::{compute_max_item_offset, recenter_offset};

    #[test]
    fn recenter_centers_middle_item_in_long_list() {
        // 20 items, all height 1, viewport of 10 rows. A selection
        // in the middle (index 10) should produce an offset that
        // leaves roughly equal rows above and below.
        let heights = vec![1usize; 20];
        let offset = recenter_offset(&heights, 10, 10);
        // Target row = 10 - 5 = 5; first item at cumulative 5 is
        // index 5 (cumulative 0..4 -> j<=5 means chosen=5).
        assert_eq!(offset, 5);
    }

    #[test]
    fn recenter_clamps_at_top() {
        // Selecting item near the top of a long list must not go
        // below offset 0 (no negative offsets).
        let heights = vec![1usize; 20];
        assert_eq!(recenter_offset(&heights, 0, 10), 0);
        assert_eq!(recenter_offset(&heights, 2, 10), 0);
    }

    #[test]
    fn recenter_clamps_at_bottom() {
        // Selecting the last item produces the max legal offset so
        // the tail items are all visible with no wasted space.
        let heights = vec![1usize; 20];
        let max = compute_max_item_offset(&heights, 10);
        assert_eq!(recenter_offset(&heights, 19, 10), max);
        assert_eq!(max, 10, "20 items / body 10 -> max offset 10");
    }

    #[test]
    fn recenter_handles_variable_heights() {
        // Mixed heights that mirror the real list shape: group
        // headers (1 row) between items (2 rows each).
        let heights = vec![1, 2, 2, 2, 1, 2, 2, 2, 2];
        // body = 5 rows, selected = 7 (a 2-row item deep in group 2).
        let offset = recenter_offset(&heights, 7, 5);
        // sel_row = sum(heights[0..7]) = 1+2+2+2+1+2+2 = 12.
        // sel_center = 12 + 1 = 13. target = 13 - 2 = 11.
        // Walk: j=0 cum=0 ok chosen=0, cum=1; j=1 cum=1 chosen=1, cum=3;
        // j=2 cum=3 chosen=2, cum=5; j=3 cum=5 chosen=3, cum=7;
        // j=4 cum=7 chosen=4, cum=8; j=5 cum=8 chosen=5, cum=10;
        // j=6 cum=10 chosen=6, cum=12; j=7 cum=12 > 11 break.
        // -> chosen=6. clamped by max_offset for body=5:
        //    from tail acc: i=8 h=2 acc=2; i=7 h=2 acc=4; i=6 h=2 acc=6>5
        //    return 7. So max_offset=7. min(6,7)=6.
        assert_eq!(offset, 6);
    }

    #[test]
    fn compute_max_item_offset_fits_whole_list() {
        // When every item fits, the max offset is 0 (you can't
        // scroll a list that's shorter than its viewport).
        let heights = vec![2, 2, 2];
        assert_eq!(compute_max_item_offset(&heights, 10), 0);
    }

    #[test]
    fn compute_max_item_offset_short_viewport() {
        // All items 1 row, 10 items, body 3 rows -> last 3 items
        // fit. First offset that fits-everything-from-there is 7.
        let heights = vec![1usize; 10];
        assert_eq!(compute_max_item_offset(&heights, 3), 7);
    }

    #[test]
    fn recenter_empty_or_oversized_selection_is_zero() {
        // Defensive: out-of-range selection or empty list returns 0.
        assert_eq!(recenter_offset(&[], 0, 10), 0);
        assert_eq!(recenter_offset(&[1, 2], 99, 10), 0);
        // body_height = 0 is degenerate (inner too small).
        assert_eq!(recenter_offset(&[1, 2, 3], 1, 0), 0);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::draw_to_buffer;
    use crate::app::{
        App, DisplayEntry, FocusPanel, ReviewGateOrigin, ReviewGateState, StubBackend,
        UserActionKey, ViewMode, is_selectable,
    };
    use crate::theme::Theme;
    use crate::work_item::{
        BackendType, CheckStatus, MergeableState, PrInfo, PrState, RepoAssociation, ReviewDecision,
        UnlinkedPr, WorkItem, WorkItemError, WorkItemId, WorkItemStatus,
    };
    use crate::work_item_backend::{BackendError, CreateWorkItem, WorkItemBackend, WorkItemRecord};
    use ratatui_core::{backend::TestBackend, terminal::Terminal};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Helper: render the app into a TestBackend and return the buffer as a string.
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default_theme();
        terminal
            .draw(|frame: &mut ratatui_core::terminal::Frame<'_>| {
                draw_to_buffer(frame.area(), frame.buffer_mut(), app, &theme)
            })
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

    /// A mock backend that returns predefined records for testing the display.
    struct MockBackend {
        records: Vec<WorkItemRecord>,
    }

    impl WorkItemBackend for MockBackend {
        fn read(&self, id: &WorkItemId) -> Result<WorkItemRecord, BackendError> {
            self.records
                .iter()
                .find(|r| r.id == *id)
                .cloned()
                .ok_or_else(|| BackendError::NotFound(id.clone()))
        }
        fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
            Ok(crate::work_item_backend::ListResult {
                records: self.records.clone(),
                corrupt: Vec::new(),
            })
        }
        fn create(&self, _req: CreateWorkItem) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not implemented".into()))
        }
        fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_status(
            &self,
            _id: &WorkItemId,
            _status: WorkItemStatus,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn import(&self, _unlinked: &UnlinkedPr) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not implemented".into()))
        }
        fn import_review_request(
            &self,
            _rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not supported in test".into()))
        }
        fn append_activity(
            &self,
            _id: &WorkItemId,
            _entry: &crate::work_item_backend::ActivityEntry,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(None)
        }
        fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
            Ok(())
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> BackendType {
            BackendType::LocalFile
        }
    }

    /// Create an App with predefined work items and unlinked PRs
    /// without going through the backend.
    fn app_with_items(work_items: Vec<WorkItem>, unlinked_prs: Vec<UnlinkedPr>) -> App {
        let mut app = App::new();
        app.work_items = work_items;
        app.unlinked_prs = unlinked_prs;
        app.build_display_list();
        app
    }

    fn make_work_item(
        id_suffix: &str,
        title: &str,
        status: WorkItemStatus,
        pr: Option<PrInfo>,
        repo_count: usize,
    ) -> WorkItem {
        let mut associations = Vec::new();
        for i in 0..repo_count.max(1) {
            associations.push(RepoAssociation {
                repo_path: PathBuf::from(format!("/repo/{id_suffix}/{i}")),
                branch: Some(format!("branch-{id_suffix}")),
                worktree_path: None,
                pr: if i == 0 { pr.clone() } else { None },
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            });
        }
        WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from(format!("/data/{id_suffix}.json"))),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: title.to_string(),
            description: None,
            status,
            status_derived: false,
            repo_associations: associations,
            errors: Vec::new(),
        }
    }

    fn make_pr_info(number: u64, checks: CheckStatus) -> PrInfo {
        PrInfo {
            number,
            title: format!("PR #{number}"),
            state: PrState::Open,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks,
            mergeable: MergeableState::Unknown,
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    fn make_unlinked_pr(branch: &str, number: u64, is_draft: bool) -> UnlinkedPr {
        UnlinkedPr {
            repo_path: PathBuf::from("/repo/unlinked"),
            pr: PrInfo {
                number,
                title: format!("Unlinked PR #{number}"),
                state: PrState::Open,
                is_draft,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                mergeable: MergeableState::Unknown,
                url: format!("https://github.com/o/r/pull/{number}"),
            },
            branch: branch.to_string(),
        }
    }

    #[test]
    fn empty_app_default_view() {
        let mut app = App::new();
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn empty_app_with_status_message() {
        let mut app = App::new();
        app.status_message = Some("Press Ctrl+N to create a work item".to_string());
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn panel_title_shows_item_count() {
        let items = vec![
            make_work_item("a", "First item", WorkItemStatus::Backlog, None, 1),
            make_work_item("b", "Second item", WorkItemStatus::Implementing, None, 1),
            make_work_item("c", "Third item", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_selected_no_session() {
        let items = vec![make_work_item(
            "todo-1",
            "Fix authentication bug",
            WorkItemStatus::Backlog,
            Some(make_pr_info(14, CheckStatus::Passing)),
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        // Select the first selectable work item entry.
        app.selected_item = app.display_list.iter().position(is_selectable);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn unlinked_pr_selected() {
        let items = vec![make_work_item(
            "prog-1",
            "Active feature",
            WorkItemStatus::Implementing,
            Some(make_pr_info(30, CheckStatus::Passing)),
            1,
        )];
        let unlinked = vec![make_unlinked_pr("fix-typo", 45, false)];
        let mut app = app_with_items(items, unlinked);
        // Select the unlinked item (index 1, since index 0 is UNLINKED header).
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn review_request_pr_selected() {
        let mut app = App::new();
        app.review_requested_prs
            .push(crate::work_item::ReviewRequestedPr {
                repo_path: PathBuf::from("/repo/upstream"),
                pr: PrInfo {
                    number: 77,
                    title: "Refactor auth middleware".into(),
                    state: PrState::Open,
                    is_draft: false,
                    review_decision: ReviewDecision::Pending,
                    checks: CheckStatus::Passing,
                    mergeable: MergeableState::Unknown,
                    url: "https://github.com/o/r/pull/77".into(),
                },
                branch: "refactor-auth".into(),
                requested_reviewer_logins: Vec::new(),
                requested_team_slugs: Vec::new(),
            });
        app.build_display_list();
        // Select the review request item (index 1: header at 0, item at 1).
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    /// Regression for a detail-panel bug where only "you" was added
    /// to the "Requested from:" line, silently dropping every other
    /// directly-requested user. A PR can request multiple direct
    /// reviewers (e.g. `alice` + `bob` + you); the detail panel must
    /// list every one of them, with "you" first, followed by the
    /// other user logins, followed by team slugs prefixed with
    /// "team ".
    #[test]
    fn review_request_pr_detail_lists_all_direct_reviewers() {
        let mut app = App::new();
        app.current_user_login = Some("bob".into());
        app.review_requested_prs
            .push(crate::work_item::ReviewRequestedPr {
                repo_path: PathBuf::from("/repo/upstream"),
                pr: PrInfo {
                    number: 77,
                    title: "Refactor auth middleware".into(),
                    state: PrState::Open,
                    is_draft: false,
                    review_decision: ReviewDecision::Pending,
                    checks: CheckStatus::Passing,
                    mergeable: MergeableState::Unknown,
                    url: "https://github.com/o/r/pull/77".into(),
                },
                branch: "refactor-auth".into(),
                requested_reviewer_logins: vec!["alice".into(), "bob".into(), "carol".into()],
                requested_team_slugs: vec!["frontend".into()],
            });
        app.build_display_list();
        app.selected_item = Some(1);

        // Use a wide terminal so the "Requested from:" line doesn't
        // wrap and we can assert its full contents in one line.
        let rendered = render(&mut app, 160, 24);

        // "you" must come first, then the other direct reviewers in
        // their original order, then teams prefixed with "team ".
        // The detail panel uses ", " as the list separator.
        assert!(
            rendered.contains("Requested from you, alice, carol, team frontend"),
            "detail panel must list every direct reviewer and every team; got:\n{rendered}",
        );
    }

    /// When no current_user_login is known, the detail panel cannot
    /// collapse any login to "you", so every directly-requested user
    /// must be rendered as their literal login.
    #[test]
    fn review_request_pr_detail_renders_all_logins_when_login_unknown() {
        let mut app = App::new();
        app.current_user_login = None;
        app.review_requested_prs
            .push(crate::work_item::ReviewRequestedPr {
                repo_path: PathBuf::from("/repo/upstream"),
                pr: PrInfo {
                    number: 77,
                    title: "Refactor auth middleware".into(),
                    state: PrState::Open,
                    is_draft: false,
                    review_decision: ReviewDecision::Pending,
                    checks: CheckStatus::Passing,
                    mergeable: MergeableState::Unknown,
                    url: "https://github.com/o/r/pull/77".into(),
                },
                branch: "refactor-auth".into(),
                requested_reviewer_logins: vec!["alice".into(), "bob".into()],
                requested_team_slugs: Vec::new(),
            });
        app.build_display_list();
        app.selected_item = Some(1);

        let rendered = render(&mut app, 160, 24);
        assert!(
            rendered.contains("Requested from alice, bob"),
            "detail panel must render every literal login when current user is unknown; got:\n{rendered}",
        );
        assert!(
            !rendered.contains("Requested from you"),
            "no 'you' should be promoted when current_user_login is None; got:\n{rendered}",
        );
    }

    #[test]
    fn unlinked_pr_selected_badge_has_highlight_style() {
        let items = vec![make_work_item(
            "prog-1",
            "Active feature",
            WorkItemStatus::Implementing,
            Some(make_pr_info(30, CheckStatus::Passing)),
            1,
        )];
        let unlinked = vec![make_unlinked_pr("fix-typo", 45, false)];
        let mut app = app_with_items(items, unlinked);
        app.selected_item = Some(1); // select the unlinked item

        let width: u16 = 80;
        let height: u16 = 24;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default_theme();
        terminal
            .draw(|frame| draw_to_buffer(frame.area(), frame.buffer_mut(), &mut app, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // The selected row is row 3 (row 0 = header, row 1 = border, row 2 = UNLINKED header, row 3 = item).
        let selected_row: u16 = 3;
        let hl = theme.style_tab_highlight();
        let mut found_badge = false;
        for x in 0..width {
            let cell = buf.cell((x, selected_row)).unwrap();
            if cell.symbol() == "P" {
                let next: String = (x..x + 5)
                    .filter_map(|cx| buf.cell((cx, selected_row)).map(|c| c.symbol().to_string()))
                    .collect();
                if next == "PR#45" {
                    assert_eq!(
                        cell.style().fg,
                        hl.fg,
                        "PR badge fg on selected unlinked item should match highlight style, \
                         got {:?} (expected {:?}). Green-on-Cyan is invisible.",
                        cell.style().fg,
                        hl.fg,
                    );
                    found_badge = true;
                    break;
                }
            }
        }
        assert!(found_badge, "PR#45 badge not found on the selected row");
    }

    #[test]
    fn unlinked_pr_long_branch_wraps() {
        // Branch long enough to force at least 2 wrap lines at the default
        // test-layout width (terminal 80 -> left-panel inner 23 cols,
        // content_width 21, first-branch budget 14 cols for PR#1 badge).
        let long_branch = "feature/very-long-branch-that-must-wrap";
        let items = vec![make_work_item(
            "prog-1",
            "Anchor",
            WorkItemStatus::Implementing,
            None,
            1,
        )];
        let unlinked = vec![make_unlinked_pr(long_branch, 1, false)];
        let mut app = app_with_items(items, unlinked);

        // Structural check via the public ListItem API: the formatted item
        // should span at least three rows (>=1 branch-wrap line + >=1
        // continuation line + 1 repo-dir meta line).
        let theme = Theme::default_theme();
        let max_width = 23_usize; // matches the left-panel inner width at term=80
        let item = super::format_unlinked_item(&app, 0, max_width, &theme, false);
        assert!(
            item.height() >= 3,
            "expected long-branch unlinked item to render as >=3 lines, got {}",
            item.height()
        );

        // End-to-end render check: the full branch must be reconstructible
        // from the wrapped left-panel rows (no truncation), and a meta row
        // with the repo directory name must follow the branch rows.
        let output = render(&mut app, 80, 24);
        let left_lines: Vec<String> = output
            .lines()
            .map(|line| {
                line.chars()
                    .skip(1) // skip the left border
                    .take(max_width) // take the left panel inner width
                    .collect::<String>()
            })
            .collect();

        // No left-panel row may exceed the content width.
        for row in &left_lines {
            assert!(
                row.chars().count() <= max_width,
                "left panel row exceeded inner width {max_width}: {row:?}"
            );
        }

        // Find the row that begins the unlinked item. The row format is
        // "<margin>? <branch-start>..." where margin is "> " if selected or
        // "  " otherwise. We match the "? " marker with either margin.
        let item_row = left_lines
            .iter()
            .position(|l| {
                let mut chars = l.chars();
                // 2 margin chars (either "> " or "  "), then "? ".
                chars.next();
                chars.next();
                chars.next() == Some('?') && chars.next() == Some(' ')
            })
            .expect("expected to find a '? ' unlinked item marker in the left panel rows");

        // First branch chunk sits between the "  ? " prefix and the right-
        // aligned "PR#" badge on the same row.
        let first_row = &left_lines[item_row];
        let first_content = first_row.get(4..).unwrap_or("");
        let first_chunk = match first_content.rfind("PR#") {
            Some(idx) => first_content[..idx].trim_end().to_string(),
            None => first_content.trim_end().to_string(),
        };
        let mut branch_chunks = vec![first_chunk];

        // Continuation rows use a 4-space indent.
        let mut i = item_row + 1;
        while i < left_lines.len() && left_lines[i].starts_with("    ") {
            branch_chunks.push(left_lines[i].trim().to_string());
            i += 1;
        }
        assert!(
            branch_chunks.len() >= 2,
            "expected branch to wrap across >=2 lines, got {}: {branch_chunks:?}",
            branch_chunks.len()
        );

        // Next row is the repo-dir meta line, indented by 2 spaces.
        assert!(
            i < left_lines.len(),
            "expected a repo-dir meta row after the branch wrap lines"
        );
        let meta_row = &left_lines[i];
        assert_eq!(
            meta_row.trim(),
            "unlinked",
            "expected meta row to contain the repo directory name 'unlinked', got {meta_row:?}"
        );

        // Concatenating the branch chunks (wrap strips intermediate whitespace
        // at break points) should reconstruct the original branch string
        // exactly - no characters dropped or truncated.
        let reconstructed: String = branch_chunks.join("");
        assert_eq!(
            reconstructed, long_branch,
            "wrapped branch chunks should reconstruct the original branch text"
        );
    }

    #[test]
    fn right_panel_focused_with_session() {
        // We cannot easily create a real session in tests, so we test the
        // "no session" case and the welcome message case instead.
        // The focused border styling is tested here via focus state.
        let items = vec![make_work_item(
            "todo-1",
            "Fix authentication bug",
            WorkItemStatus::Backlog,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);
        app.focus = FocusPanel::Right;
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_with_context_bar() {
        use crate::work_item::IssueInfo;
        use crate::work_item::IssueState;
        let mut wi = make_work_item("ctx-1", "Fix resize bug", WorkItemStatus::Backlog, None, 1);
        // Add issue with labels to trigger the context bar.
        wi.repo_associations[0].issue = Some(IssueInfo {
            number: 42,
            title: "Fix resize bug".into(),
            state: IssueState::Open,
            labels: vec!["bug".into(), "P1".into()],
        });
        let mut app = app_with_items(vec![wi], vec![]);
        // Select the first selectable work item entry.
        app.selected_item = app.display_list.iter().position(is_selectable);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_context_bar_no_labels() {
        let items = vec![make_work_item(
            "ctx-2",
            "Add authentication",
            WorkItemStatus::Backlog,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_context_bar_with_status() {
        use crate::work_item::IssueInfo;
        use crate::work_item::IssueState;
        let mut wi = make_work_item("ctx-3", "Fix resize bug", WorkItemStatus::Backlog, None, 1);
        wi.repo_associations[0].issue = Some(IssueInfo {
            number: 42,
            title: "Fix resize bug".into(),
            state: IssueState::Open,
            labels: vec!["bug".into()],
        });
        let mut app = app_with_items(vec![wi], vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);
        app.status_message = Some("Right panel focused - press Ctrl+] to return".into());
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn activity_indicator_overrides_status_message() {
        let items = vec![make_work_item(
            "act-1",
            "Build feature",
            WorkItemStatus::Implementing,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);
        app.status_message = Some("This should be hidden".into());
        app.start_activity("Creating pull request...");
        app.spinner_tick = 3; // Pick a specific frame for deterministic snapshot.
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn activity_indicator_with_count() {
        let items = vec![make_work_item(
            "act-2",
            "Multi-task",
            WorkItemStatus::Implementing,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);
        app.start_activity("Running review gate...");
        app.start_activity("Creating pull request...");
        app.spinner_tick = 0;
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn settings_overlay_with_config() {
        use crate::config::Config;

        // Use /tmp (not std::env::temp_dir()) so rendered paths are
        // deterministic across machines. macOS temp_dir() returns
        // /var/folders/... which differs per user.
        let base = std::path::PathBuf::from("/tmp/workbridge-test-settings-overlay");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("discovered-a/.git")).unwrap();
        std::fs::create_dir_all(base.join("discovered-b/.git")).unwrap();

        let base_str = base.display().to_string();
        let discovered_a = base.join("discovered-a").display().to_string();

        let config = Config {
            base_dirs: vec![base_str],
            // Use an absolute path instead of ~ to avoid tilde expansion
            // which produces different paths on different machines.
            repos: vec!["/root/Forks/special-repo".into()],
            included_repos: vec![discovered_a],
            ..Config::for_test()
        };
        let mut app = App::with_config(config, Arc::new(StubBackend));
        app.show_settings = true;
        let output = render(&mut app, 80, 24);

        let _ = std::fs::remove_dir_all(&base);

        insta::assert_snapshot!(output);
    }

    // -- Work item display tests --

    #[test]
    fn work_item_list_grouped() {
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Backlog,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
            make_work_item(
                "todo-2",
                "Add user settings page",
                WorkItemStatus::Backlog,
                None,
                1,
            ),
            make_work_item(
                "prog-1",
                "Refactor backend API",
                WorkItemStatus::Implementing,
                Some(make_pr_info(88, CheckStatus::Failing)),
                2,
            ),
            make_work_item(
                "prog-2",
                "Update dependencies",
                WorkItemStatus::Implementing,
                Some(make_pr_info(12, CheckStatus::Pending)),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_list_with_unlinked() {
        let items = vec![make_work_item(
            "prog-1",
            "Active feature",
            WorkItemStatus::Implementing,
            Some(make_pr_info(30, CheckStatus::Passing)),
            1,
        )];
        let unlinked = vec![
            make_unlinked_pr("fix-typo", 45, false),
            make_unlinked_pr("update-deps", 12, true),
        ];
        let mut app = app_with_items(items, unlinked);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_list_empty_groups() {
        let mut app = app_with_items(vec![], vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_list_with_done_group() {
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Backlog,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
            make_work_item(
                "prog-1",
                "Refactor backend API",
                WorkItemStatus::Implementing,
                Some(make_pr_info(88, CheckStatus::Failing)),
                1,
            ),
            make_work_item(
                "done-1",
                "Update dependencies",
                WorkItemStatus::Done,
                Some(PrInfo {
                    number: 50,
                    title: "Update deps".to_string(),
                    state: PrState::Merged,
                    is_draft: false,
                    review_decision: ReviewDecision::None,
                    checks: CheckStatus::Passing,
                    mergeable: MergeableState::Unknown,
                    url: "https://github.com/o/r/pull/50".to_string(),
                }),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    /// Test helper: mark the given work item id as currently at a
    /// review gate by inserting a minimal `ReviewGateState` into
    /// `app.review_gates`. Starts a status-bar activity so the
    /// production `drop_review_gate` invariant (every drop site ends
    /// the activity) stays exercisable.
    ///
    /// The receiver is a dead-end `unbounded()` channel: we never poll
    /// the gate in this test, so no messages ever need to flow.
    fn mark_at_review_gate(app: &mut App, wi_id: &WorkItemId) {
        let (_tx, rx) = crossbeam_channel::unbounded();
        let activity = app.start_activity("test review gate");
        app.review_gates.insert(
            wi_id.clone(),
            ReviewGateState {
                rx,
                progress: None,
                origin: ReviewGateOrigin::Tui,
                activity,
            },
        );
    }

    #[test]
    fn work_item_list_review_gate() {
        // Baseline: plain `[IM]` item (no gate) to confirm adjacent rows
        // are unaffected.
        let plain = make_work_item(
            "plain-im",
            "Plain implementing item",
            WorkItemStatus::Implementing,
            None,
            1,
        );
        // `[IM]` item sitting at a review gate -> `[IM][RG]`.
        let gated_im = make_work_item(
            "gated-im",
            "Implementing at review gate",
            WorkItemStatus::Implementing,
            None,
            1,
        );
        // `[BK]` item sitting at a review gate -> `[BK][RG]`. The gate
        // can still be active when a work item retreats from
        // Implementing to Blocked (see `docs/work-items.md`).
        let gated_bk = make_work_item(
            "gated-bk",
            "Blocked at review gate",
            WorkItemStatus::Blocked,
            None,
            1,
        );
        // Review-request kind at a gate -> `[RR][IM][RG]`, confirming
        // the [RG] badge composes correctly with the [RR] kind badge.
        let mut gated_rr = make_work_item(
            "gated-rr",
            "Review request at gate",
            WorkItemStatus::Implementing,
            None,
            1,
        );
        gated_rr.kind = crate::work_item::WorkItemKind::ReviewRequest;

        let gated_im_id = gated_im.id.clone();
        let gated_bk_id = gated_bk.id.clone();
        let gated_rr_id = gated_rr.id.clone();

        let items = vec![plain, gated_im, gated_bk, gated_rr];
        let mut app = app_with_items(items, vec![]);
        mark_at_review_gate(&mut app, &gated_im_id);
        mark_at_review_gate(&mut app, &gated_bk_id);
        mark_at_review_gate(&mut app, &gated_rr_id);
        // Rebuild the display list after mutating review-gate state in
        // case grouping/ordering depends on it. (It doesn't today, but
        // keeping this call defensive matches how `app_with_items`
        // primes the list.)
        app.build_display_list();
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn work_item_with_errors_no_session() {
        let items = vec![WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/data/err.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Broken work item".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/repo/alpha"),
                branch: Some("42-fix-bug".to_string()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            errors: vec![
                WorkItemError::MultiplePrsForBranch {
                    repo_path: PathBuf::from("/repo/alpha"),
                    branch: "42-fix-bug".to_string(),
                    count: 2,
                },
                WorkItemError::IssueNotFound {
                    repo_path: PathBuf::from("/repo/alpha"),
                    issue_number: 42,
                },
            ],
        }];
        let mut app = app_with_items(items, vec![]);
        // Select the first selectable work item entry (skipping group headers).
        app.selected_item = app.display_list.iter().position(is_selectable);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn create_dialog_default_view() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![
            PathBuf::from("/Volumes/X10/Projects/workbridge"),
            PathBuf::from("/Volumes/X10/Projects/other-repo"),
        ];
        app.create_dialog.open(
            &repos,
            Some(&PathBuf::from("/Volumes/X10/Projects/workbridge")),
        );
        assert!(app.create_dialog.visible);
        assert_eq!(app.create_dialog.focus_field, CreateDialogFocus::Title);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn create_dialog_with_input_and_repos_focused() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![
            PathBuf::from("/repo/alpha"),
            PathBuf::from("/repo/beta"),
            PathBuf::from("/repo/gamma"),
        ];
        app.create_dialog
            .open(&repos, Some(&PathBuf::from("/repo/beta")));
        // Type a title
        app.create_dialog.title_input.set_text("My feature");
        // Focus on repos
        app.create_dialog.focus_field = CreateDialogFocus::Repos;
        app.create_dialog.repo_cursor = 1; // beta is selected
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn create_dialog_with_error() {
        let mut app = App::new();
        let repos = vec![PathBuf::from("/repo/only")];
        app.create_dialog.open(&repos, None);
        app.create_dialog.error_message = Some("Title cannot be empty".to_string());
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    /// Ctrl+N with multiple managed repos opens a compact quick-start
    /// dialog containing only the repo list. The render must:
    /// - show the "Quick start - select repo" title and "Repos:" label
    /// - NOT render any of the Title/Description/Branch fields that the
    ///   full Ctrl+B dialog uses
    /// - show the quickstart-specific hint (Up/Down/Space, no Tab)
    ///
    /// Rendered at 120 columns so the full hint line fits inside the
    /// dialog's 60%-of-width box (the 80-col default would truncate it
    /// like `create_dialog_default_view` already does).
    #[test]
    fn create_dialog_quickstart_view() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![PathBuf::from("/repo/alpha"), PathBuf::from("/repo/beta")];
        app.create_dialog.open_quickstart(&repos);
        assert!(app.create_dialog.visible);
        assert!(app.create_dialog.quickstart_mode);
        assert_eq!(app.create_dialog.focus_field, CreateDialogFocus::Repos);

        let rendered = render(&mut app, 120, 24);

        assert!(
            rendered.contains("Quick start - select repo"),
            "expected dialog title 'Quick start - select repo':\n{rendered}"
        );
        assert!(
            rendered.contains("Repos:"),
            "expected 'Repos:' label:\n{rendered}"
        );
        assert!(
            rendered.contains("/repo/alpha"),
            "expected first repo path to be listed:\n{rendered}"
        );
        assert!(
            rendered.contains("/repo/beta"),
            "expected second repo path to be listed:\n{rendered}"
        );
        assert!(
            !rendered.contains("Title:"),
            "Title: field must not be rendered in quick-start mode:\n{rendered}"
        );
        assert!(
            !rendered.contains("Description (optional)"),
            "Description field label must not be rendered in quick-start mode:\n{rendered}"
        );
        assert!(
            !rendered.contains("Branch (optional)"),
            "Branch field label must not be rendered in quick-start mode:\n{rendered}"
        );
        assert!(
            rendered.contains("Up/Down: Move"),
            "expected quickstart-specific hint 'Up/Down: Move':\n{rendered}"
        );
        assert!(
            rendered.contains("Space: Select repo"),
            "expected quickstart-specific hint 'Space: Select repo':\n{rendered}"
        );
        assert!(
            !rendered.contains("Tab: Next field"),
            "hint must not mention 'Tab: Next field' in quick-start mode:\n{rendered}"
        );
    }

    /// Regression test for "Creating a new workitem does not allow
    /// multi-line description" (#2266).
    ///
    /// A description that is much longer than the textarea width must
    /// wrap onto multiple visible rows instead of being clipped at the
    /// right margin. The description is built from a distinctive set of
    /// uppercase tokens so the test can cheaply confirm that more than
    /// one token lands on a different row - which is only possible if
    /// the TextArea wrapped.
    #[test]
    fn create_dialog_wraps_long_description() {
        let mut app = App::new();
        let repos = vec![PathBuf::from("/repo/only")];
        app.create_dialog.open(&repos, None);
        // Sixteen distinctive uppercase tokens; at a 48-column dialog
        // width the TextArea's inner width is well under ~46 cols, so
        // these tokens cannot all fit on one line.
        let tokens = [
            "ALPHA", "BETA", "GAMMA", "DELTA", "EPSILON", "ZETA", "ETA", "THETA", "IOTA", "KAPPA",
            "LAMBDA", "MU", "NU", "XI", "OMICRON", "PI",
        ];
        let long_description = tokens.join(" ");
        app.create_dialog
            .description_input
            .set_text(&long_description);

        // A 40-row terminal gives the dialog vertical slack so the
        // description textarea sits well inside the visible area.
        let rendered = render(&mut app, 80, 40);

        // Find rows that contain any of the unique tokens. If wrapping
        // happened at least two distinct rows in the rendered output
        // contain different tokens. A non-wrapping TextArea (or a
        // horizontally-clipped Paragraph) would only ever place one
        // row's worth of description text on screen, so no more than a
        // single row would contain a token.
        let mut rows_with_token = 0usize;
        for line in rendered.lines() {
            if tokens.iter().any(|t| line.contains(t)) {
                rows_with_token += 1;
            }
        }
        assert!(
            rows_with_token >= 2,
            "expected the long description to wrap onto at least 2 rows, \
             but only {rows_with_token} rendered row(s) contained any \
             description token.\nRendered output:\n{rendered}"
        );
    }

    #[test]
    fn work_item_list_scrollbar_visible_on_overflow() {
        let items: Vec<WorkItem> = (0..15)
            .map(|i| {
                let status = match i % 3 {
                    0 => WorkItemStatus::Implementing,
                    1 => WorkItemStatus::Backlog,
                    _ => WorkItemStatus::Review,
                };
                make_work_item(
                    &format!("item-{i}"),
                    &format!("Work item number {i}"),
                    status,
                    None,
                    1,
                )
            })
            .collect();
        let mut app = app_with_items(items, vec![]);
        // Select an item near the end to force scrolling. Set the
        // recenter flag so the first render behaves as if the user
        // keyboard-navigated to this selection - without it the new
        // decoupled viewport starts at offset 0 and the selection
        // would be offscreen (with only the scrollbar marker to show
        // where it is). The existing snapshot was captured against
        // the old auto-scroll-to-selection behaviour, so mimic that
        // here.
        app.selected_item = Some(app.display_list.len().saturating_sub(2));
        app.recenter_viewport_on_selection.set(true);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    /// Render through `TestBackend` and return the raw buffer so
    /// tests can inspect per-cell symbol + foreground color. The
    /// string-returning `render()` helper drops style information;
    /// tests that must distinguish the Cyan selection marker from
    /// the Gray scrollbar thumb (both use `\u{2588}`) need the
    /// buffer directly.
    fn render_buffer(app: &mut App, width: u16, height: u16) -> ratatui_core::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default_theme();
        terminal
            .draw(|frame: &mut ratatui_core::terminal::Frame<'_>| {
                draw_to_buffer(frame.area(), frame.buffer_mut(), app, &theme)
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Count cells in column `x` (across all rows of `buf`) whose
    /// symbol is `\u{2588}` and whose foreground color matches
    /// `fg`. Used to distinguish selection marker (Cyan) from
    /// scrollbar thumb (Gray) in the same column.
    fn count_block_cells_with_fg(
        buf: &ratatui_core::buffer::Buffer,
        x: u16,
        fg: ratatui_core::style::Color,
    ) -> usize {
        let area = buf.area;
        let mut n = 0;
        for y in area.y..(area.y + area.height) {
            if let Some(cell) = buf.cell((x, y))
                && cell.symbol() == "\u{2588}"
                && cell.fg == fg
            {
                n += 1;
            }
        }
        n
    }

    /// Scrollbar column for the left panel at the given terminal
    /// width. Mirrors `draw_work_item_list`'s scrollbar geometry:
    /// the track sits at `area.x + area.width - 1`, i.e. the last
    /// column of the left panel's bordered block.
    fn scrollbar_column(width: u16) -> u16 {
        let pl = crate::layout::compute(width, 24, 0);
        // The left panel occupies columns 0..pl.left_width, and the
        // scrollbar is painted on its right border column.
        pl.left_width - 1
    }

    /// Offscreen-selection marker, selection above the viewport.
    ///
    /// With the decoupled viewport, a selection that has scrolled off
    /// the top of the visible body is signalled by a single Cyan
    /// filled-block cell in the scrollbar column at the y-coordinate
    /// corresponding to the selection's position in the full list.
    /// We inspect the buffer directly because the Gray thumb uses
    /// the same glyph - only the foreground color distinguishes the
    /// marker from the thumb.
    #[test]
    fn offscreen_selection_marker_above_viewport() {
        let items: Vec<WorkItem> = (0..15)
            .map(|i| {
                make_work_item(
                    &format!("item-{i}"),
                    &format!("Work item number {i}"),
                    WorkItemStatus::Implementing,
                    None,
                    1,
                )
            })
            .collect();
        let mut app = app_with_items(items, vec![]);
        // Select the first selectable item, then scroll the viewport
        // down without touching the selection - simulates the
        // user wheel-scrolling past their keyboard cursor.
        app.selected_item = app.display_list.iter().position(is_selectable);
        app.list_scroll_offset.set(app.display_list.len() - 2);
        app.recenter_viewport_on_selection.set(false);

        let buf = render_buffer(&mut app, 80, 24);
        let x = scrollbar_column(80);
        let cyan = count_block_cells_with_fg(&buf, x, ratatui_core::style::Color::Cyan);
        assert_eq!(
            cyan, 1,
            "offscreen selection must paint exactly one Cyan block in the scrollbar column",
        );
    }

    /// Offscreen-selection marker, selection below the viewport. Same
    /// setup as above but in the other direction - keep the viewport
    /// at the top while the selection sits deep in the list.
    #[test]
    fn offscreen_selection_marker_below_viewport() {
        let items: Vec<WorkItem> = (0..15)
            .map(|i| {
                make_work_item(
                    &format!("item-{i}"),
                    &format!("Work item number {i}"),
                    WorkItemStatus::Implementing,
                    None,
                    1,
                )
            })
            .collect();
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().rposition(is_selectable);
        app.list_scroll_offset.set(0);
        app.recenter_viewport_on_selection.set(false);

        let buf = render_buffer(&mut app, 80, 24);
        let x = scrollbar_column(80);
        let cyan = count_block_cells_with_fg(&buf, x, ratatui_core::style::Color::Cyan);
        assert_eq!(
            cyan, 1,
            "offscreen selection must paint exactly one Cyan block in the scrollbar column",
        );
    }

    /// When the selection is inside the visible viewport, only the
    /// normal scrollbar thumb is rendered - the offscreen marker must
    /// NOT double-paint on top of the thumb. Since the whole list
    /// fits at this terminal size, neither the thumb nor the marker
    /// is drawn, so the scrollbar column must contain no block cells
    /// at all.
    #[test]
    fn selection_visible_no_extra_marker() {
        let items = vec![
            make_work_item("a", "First item", WorkItemStatus::Backlog, None, 1),
            make_work_item("b", "Second item", WorkItemStatus::Implementing, None, 1),
            make_work_item("c", "Third item", WorkItemStatus::Review, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);

        let buf = render_buffer(&mut app, 80, 24);
        let x = scrollbar_column(80);
        let cyan = count_block_cells_with_fg(&buf, x, ratatui_core::style::Color::Cyan);
        let gray = count_block_cells_with_fg(&buf, x, ratatui_core::style::Color::Gray);
        assert_eq!(
            cyan, 0,
            "fully-visible list must not paint the Cyan selection marker",
        );
        assert_eq!(
            gray, 0,
            "fully-visible list has no overflow so the scrollbar thumb must not draw",
        );
    }

    /// Scrollbar-overflow companion: when the list overflows AND the
    /// selection is onscreen, the Gray thumb paints but the Cyan
    /// marker does NOT. Catches a regression where the marker might
    /// double-paint on top of the thumb for visible selections.
    #[test]
    fn selection_onscreen_paints_thumb_but_no_marker() {
        let items: Vec<WorkItem> = (0..15)
            .map(|i| {
                make_work_item(
                    &format!("item-{i}"),
                    &format!("Work item number {i}"),
                    WorkItemStatus::Implementing,
                    None,
                    1,
                )
            })
            .collect();
        let mut app = app_with_items(items, vec![]);
        // Select the first selectable item AND keep the viewport at
        // the top via recenter so the selection is definitely visible.
        app.selected_item = app.display_list.iter().position(is_selectable);
        app.recenter_viewport_on_selection.set(true);

        let buf = render_buffer(&mut app, 80, 24);
        let x = scrollbar_column(80);
        let cyan = count_block_cells_with_fg(&buf, x, ratatui_core::style::Color::Cyan);
        let gray = count_block_cells_with_fg(&buf, x, ratatui_core::style::Color::Gray);
        assert_eq!(cyan, 0, "onscreen selection must not paint the Cyan marker",);
        assert!(
            gray > 0,
            "overflowing list must paint at least one Gray thumb cell",
        );
    }

    #[test]
    fn work_item_list_no_scrollbar_when_fits() {
        let items = vec![
            make_work_item("a", "First item", WorkItemStatus::Backlog, None, 1),
            make_work_item("b", "Second item", WorkItemStatus::Implementing, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    // -- Board view snapshot tests --

    #[test]
    fn board_view_empty() {
        let mut app = App::new();
        app.view_mode = ViewMode::Board;
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn board_view_items_distributed() {
        let items = vec![
            make_work_item("bl1", "Add caching layer", WorkItemStatus::Backlog, None, 1),
            make_work_item(
                "pl1",
                "Refactor auth middleware",
                WorkItemStatus::Planning,
                None,
                1,
            ),
            make_work_item(
                "im1",
                "Fix race condition",
                WorkItemStatus::Implementing,
                None,
                1,
            ),
            make_work_item(
                "rv1",
                "Update CI pipeline",
                WorkItemStatus::Review,
                Some(make_pr_info(42, CheckStatus::Passing)),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        app.view_mode = ViewMode::Board;
        app.sync_board_cursor();
        insta::assert_snapshot!(render(&mut app, 120, 40));
    }

    #[test]
    fn board_view_selected_item() {
        let items = vec![
            make_work_item("bl1", "First item", WorkItemStatus::Backlog, None, 1),
            make_work_item("im1", "Active work", WorkItemStatus::Implementing, None, 1),
            make_work_item(
                "im2",
                "Second active",
                WorkItemStatus::Implementing,
                None,
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        app.view_mode = ViewMode::Board;
        // Select second item in Implementing column (column 2, row 1).
        app.board_cursor.column = 2;
        app.board_cursor.row = Some(1);
        app.sync_selection_from_board();
        insta::assert_snapshot!(render(&mut app, 120, 40));
    }

    #[test]
    fn board_view_blocked_item() {
        let items = vec![
            make_work_item("im1", "Normal work", WorkItemStatus::Implementing, None, 1),
            make_work_item("bk1", "Blocked task", WorkItemStatus::Blocked, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        app.view_mode = ViewMode::Board;
        app.board_cursor.column = 2; // Implementing column
        app.board_cursor.row = Some(0);
        app.sync_selection_from_board();
        insta::assert_snapshot!(render(&mut app, 120, 40));
    }

    #[test]
    fn board_view_long_title_wraps() {
        let items = vec![make_work_item(
            "long1",
            "Add response caching layer with Redis integration for the API users endpoint",
            WorkItemStatus::Backlog,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.view_mode = ViewMode::Board;
        app.sync_board_cursor();
        // At 80 cols: column is 20 wide, inner 16. Title must wrap, not clip.
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn board_view_with_status_bar() {
        let items = vec![make_work_item(
            "bl1",
            "Test item",
            WorkItemStatus::Backlog,
            None,
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.view_mode = ViewMode::Board;
        app.sync_board_cursor();
        app.status_message = Some("Item moved to Planning".to_string());
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn board_view_item_in_every_column_120x40() {
        let items = vec![
            make_work_item(
                "bl1",
                "Add response caching layer",
                WorkItemStatus::Backlog,
                None,
                1,
            ),
            make_work_item(
                "pl1",
                "Refactor auth middleware",
                WorkItemStatus::Planning,
                None,
                1,
            ),
            make_work_item(
                "im1",
                "Fix race condition in fetcher",
                WorkItemStatus::Implementing,
                None,
                1,
            ),
            make_work_item(
                "rv1",
                "Update CI pipeline config",
                WorkItemStatus::Review,
                Some(make_pr_info(42, CheckStatus::Passing)),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        app.view_mode = ViewMode::Board;
        app.sync_board_cursor();
        // At 120x40, each column is 30 wide (28 inner). No title should clip.
        insta::assert_snapshot!(render(&mut app, 120, 40));
    }

    // -- Prompt dialog snapshot tests --

    #[test]
    fn merge_prompt_dialog() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_wi_id = Some(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn merge_progress_dialog() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_in_progress = true;
        app.merge_wi_id = Some(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        app.spinner_tick = 3;
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn rework_prompt_dialog() {
        let mut app = App::new();
        app.rework_prompt_visible = true;
        app.rework_prompt_wi = Some(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        app.rework_prompt_input
            .set_text("needs more error handling");
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn no_plan_prompt_dialog() {
        let mut app = App::new();
        app.no_plan_prompt_visible = true;
        app.no_plan_prompt_queue
            .push_back(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn cleanup_confirm_dialog() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        app.cleanup_unlinked_target =
            Some((PathBuf::from("/tmp/repo"), "feature-branch".to_string(), 42));
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn cleanup_reason_dialog() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        app.cleanup_reason_input_active = true;
        app.cleanup_unlinked_target =
            Some((PathBuf::from("/tmp/repo"), "feature-branch".to_string(), 42));
        app.cleanup_reason_input.set_text("closing - abandoned");
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn cleanup_progress_dialog() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        // Simulate an in-flight unlinked cleanup by admitting the
        // helper entry directly and then ending the visible
        // status-bar activity. This mirrors spawn_unlinked_cleanup,
        // which hides the status-bar spinner so only the in-dialog
        // spinner is shown.
        let aid = app
            .try_begin_user_action(
                UserActionKey::UnlinkedCleanup,
                std::time::Duration::ZERO,
                "Cleaning up unlinked PR...",
            )
            .expect("helper admit should succeed");
        app.end_activity(aid);
        app.cleanup_progress_pr_number = Some(42);
        app.spinner_tick = 3; // deterministic spinner frame
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn alert_dialog() {
        let mut app = App::new();
        app.alert_message = Some("PR close failed: permission denied".to_string());
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    // -- Sticky group header tests --

    #[test]
    fn sticky_header_visible_when_scrolled() {
        // Create enough items to force scrolling in a short viewport.
        // All items are in the same repo so they share one ACTIVE header.
        let items = vec![
            make_work_item("a1", "Item A1", WorkItemStatus::Implementing, None, 1),
            make_work_item("a2", "Item A2", WorkItemStatus::Implementing, None, 1),
            make_work_item("a3", "Item A3", WorkItemStatus::Implementing, None, 1),
            make_work_item("a4", "Item A4", WorkItemStatus::Implementing, None, 1),
            make_work_item("a5", "Item A5", WorkItemStatus::Implementing, None, 1),
            make_work_item("b1", "Item B1", WorkItemStatus::Backlog, None, 1),
            make_work_item("b2", "Item B2", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Select the last selectable item to force the viewport to scroll.
        // Set the recenter flag so the render simulates the viewport
        // that keyboard navigation would produce (the new decoupled
        // viewport does NOT auto-scroll on selection - wheel scrolls
        // park it, keyboard navigation recenters it).
        if let Some(pos) = app.display_list.iter().rposition(is_selectable) {
            app.selected_item = Some(pos);
            app.recenter_viewport_on_selection.set(true);
        }
        // Short viewport forces the ACTIVE header off-screen -> sticky.
        insta::assert_snapshot!(render(&mut app, 80, 12));
    }

    #[test]
    fn no_sticky_header_at_top_of_list() {
        // With only a few items, the header is always visible at the top.
        let items = vec![
            make_work_item("a", "Item A", WorkItemStatus::Implementing, None, 1),
            make_work_item("b", "Item B", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Offset 0, header is visible - no sticky header should appear.
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn no_sticky_header_in_drill_down() {
        // In board drill-down mode, the display list has no group headers.
        let items = vec![
            make_work_item("a", "Item A", WorkItemStatus::Implementing, None, 1),
            make_work_item("b", "Item B", WorkItemStatus::Implementing, None, 1),
            make_work_item("c", "Item C", WorkItemStatus::Implementing, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        app.board_drill_stage = Some(WorkItemStatus::Implementing);
        app.board_drill_down = true;
        app.build_display_list();
        app.selected_item = app.display_list.iter().rposition(is_selectable);
        insta::assert_snapshot!(render(&mut app, 80, 12));
    }

    #[test]
    fn work_item_mergequeue_hint_and_pr_url() {
        let items = vec![make_work_item(
            "mq-1",
            "Waiting for merge",
            WorkItemStatus::Mergequeue,
            Some(make_pr_info(42, CheckStatus::Passing)),
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);

        let rendered = render(&mut app, 100, 30);
        assert!(
            rendered.contains("Waiting for PR to be merged"),
            "should render mergequeue hint: {rendered}"
        );
        assert!(
            rendered.contains("Shift+Left"),
            "should mention Shift+Left to cancel: {rendered}"
        );
        assert!(
            rendered.contains("https://github.com/o/r/pull/42"),
            "should render full PR URL: {rendered}"
        );
    }

    /// Regression for F-2: long PR URLs must not lose horizontal space
    /// to the field-label prefix. Before the fix the URL was rendered as
    /// a labelled row (`  PR URL      <url>`), which left only ~40 cols
    /// of value space; a real URL would clip well before the panel edge.
    /// The fix renders the URL on its own dedicated line after the field
    /// block, so it uses the full inner width of the right pane and only
    /// clips at the terminal boundary itself.
    #[test]
    fn work_item_long_pr_url_uses_full_panel_width() {
        let mut item = make_work_item(
            "long-url",
            "Has long URL",
            WorkItemStatus::Review,
            Some(make_pr_info(123456, CheckStatus::Passing)),
            1,
        );
        let long_url =
            "https://github.com/very-long-org-name/very-long-repo-name/pull/123456".to_string();
        item.repo_associations[0].pr.as_mut().unwrap().url = long_url.clone();

        let mut app = app_with_items(vec![item], vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);

        // At a wide terminal the entire URL fits and must appear in full.
        let wide = render(&mut app, 160, 30);
        assert!(
            wide.contains(&long_url),
            "long PR URL should appear in full at 160-col width:\n{wide}"
        );

        // At 80 cols the right pane is narrower than the URL, so the URL
        // necessarily clips at the panel boundary - but it must clip
        // strictly later than the old labelled-row layout would have. The
        // old layout reserved ~14 cols for the label prefix, so any
        // visible URL prefix longer than 14 chars + 14 chars (~28) of URL
        // body proves the dedicated-line layout is in use. Use 40 chars
        // as a comfortable lower bound that the labelled-row layout could
        // never have produced.
        let narrow = render(&mut app, 80, 24);
        let prefix = &long_url[..40];
        assert!(
            narrow.contains(prefix),
            "narrow render should still show at least the first 40 chars of \
             the URL on a dedicated line; got:\n{narrow}"
        );
    }

    #[test]
    fn sticky_header_shows_correct_group_when_multiple_groups() {
        // Create items across two groups - when scrolled to the second group,
        // the second group's header should be sticky (not the first).
        let items = vec![
            make_work_item("a1", "Active 1", WorkItemStatus::Implementing, None, 1),
            make_work_item("a2", "Active 2", WorkItemStatus::Implementing, None, 1),
            make_work_item("a3", "Active 3", WorkItemStatus::Implementing, None, 1),
            make_work_item("b1", "Backlog 1", WorkItemStatus::Backlog, None, 1),
            make_work_item("b2", "Backlog 2", WorkItemStatus::Backlog, None, 1),
            make_work_item("b3", "Backlog 3", WorkItemStatus::Backlog, None, 1),
            make_work_item("b4", "Backlog 4", WorkItemStatus::Backlog, None, 1),
            make_work_item("b5", "Backlog 5", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Select the last backlog item to scroll deep into the BACKLOGGED group.
        // Set the recenter flag so the render simulates keyboard navigation
        // (the new decoupled viewport does not auto-scroll).
        if let Some(pos) = app.display_list.iter().rposition(is_selectable) {
            app.selected_item = Some(pos);
            app.recenter_viewport_on_selection.set(true);
        }
        // Short viewport so the BACKLOGGED header scrolls off -> sticky.
        insta::assert_snapshot!(render(&mut app, 80, 12));
    }

    /// Regression: the sticky group header must NEVER paint over the first
    /// wrapped line of the topmost visible (and in particular the selected)
    /// work item. Before the structural-slot fix the sticky `Paragraph`
    /// overlay overwrote the first row of the list body, hiding the title
    /// of the selected item when it was the topmost visible entry and its
    /// group header had scrolled above the viewport.
    ///
    /// This test uses a text-based assertion (not a snapshot) so small
    /// unrelated layout changes do not require re-blessing the expectation.
    /// It picks a title with a unique wrap-friendly substring and asserts
    /// that:
    ///   1. the sticky header is still displayed (the fix did not disable it),
    ///   2. the selected item's first line is still present in the rendered
    ///      output (the fix did not merely hide the sticky).
    #[test]
    fn sticky_header_does_not_overlap_selected_item() {
        // Two groups. The first BACKLOGGED item gets a distinctive title
        // chosen to mirror the user's screenshot - when it is selected and
        // the ACTIVE group has scrolled above the viewport, the buggy
        // overlay would paint "BACKLOGGED (repo)" over "show cwd in...".
        let items = vec![
            make_work_item("a1", "Active one", WorkItemStatus::Implementing, None, 1),
            make_work_item("a2", "Active two", WorkItemStatus::Implementing, None, 1),
            make_work_item("a3", "Active three", WorkItemStatus::Implementing, None, 1),
            make_work_item("a4", "Active four", WorkItemStatus::Implementing, None, 1),
            make_work_item(
                "b1",
                "show cwd in status bar for workitems",
                WorkItemStatus::Backlog,
                None,
                1,
            ),
            make_work_item("b2", "Backlog other", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Select the first BACKLOGGED item specifically. With a short
        // viewport this forces the list to scroll so that the BACKLOGGED
        // group header sits at the top of the body and the ACTIVE group
        // header is above the viewport - the exact scenario where the old
        // overlay clobbered the selected item's first wrapped line.
        let target = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(idx) if *idx == 4))
            .expect("target BACKLOGGED item must be in display list");
        app.selected_item = Some(target);
        // Simulate the keyboard navigation that would have set this
        // selection in production - the new decoupled viewport only
        // scrolls to the selection when this flag is set, wheel
        // scrolls deliberately leave it alone.
        app.recenter_viewport_on_selection.set(true);

        let rendered = render(&mut app, 40, 12);

        // The sticky (or real) BACKLOGGED header must still be shown.
        assert!(
            rendered.contains("BACKLOGGED"),
            "BACKLOGGED header must still render after the fix:\n{rendered}"
        );
        // The distinctive first-line substring of the selected item must
        // be present in the output. Before the fix it was painted over.
        assert!(
            rendered.contains("show cwd"),
            "selected item's first wrapped line must be visible, \
             not overlapped by the sticky header:\n{rendered}"
        );
    }
}
