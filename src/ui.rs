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
    App, BOARD_COLUMNS, DisplayEntry, FocusPanel, GroupHeaderKind, RightPanelTab,
    SettingsListFocus, SettingsTab, UserActionKey, ViewMode, WorkItemContext,
};
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
            draw_prompt_dialog(
                buf,
                theme,
                area,
                PromptDialogKind::KeyChoice {
                    title: "Merge Strategy",
                    body: &format!("{spinner} Merging pull request... Please wait."),
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
    let is_working = app.claude_working.contains(&wi.id);
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

    let items: Vec<ListItem> = app
        .display_list
        .iter()
        .enumerate()
        .map(|(i, entry)| match entry {
            DisplayEntry::GroupHeader { label, count, kind } => {
                let text = format!("{label} ({count})");
                let style = match kind {
                    GroupHeaderKind::Blocked => theme.style_group_header_blocked(),
                    GroupHeaderKind::Normal => theme.style_group_header(),
                };
                ListItem::new(Line::from(vec![Span::raw("  "), Span::styled(text, style)]))
            }
            DisplayEntry::UnlinkedItem(idx) => {
                let selected = app.selected_item == Some(i);
                format_unlinked_item(app, *idx, inner_width, theme, selected)
            }
            DisplayEntry::ReviewRequestItem(idx) => {
                let selected = app.selected_item == Some(i);
                format_review_request_item(app, *idx, inner_width, theme, selected)
            }
            DisplayEntry::WorkItemEntry(idx) => {
                let selected = app.selected_item == Some(i);
                format_work_item_entry(app, *idx, inner_width, theme, selected)
            }
        })
        .collect();

    // Pre-compute per-item row heights for scrollbar calculations.
    let item_heights: Vec<usize> = items.iter().map(ListItem::height).collect();
    let total_rows: usize = item_heights.iter().sum();

    let list = List::new(items)
        .block(block)
        .highlight_style(theme.style_tab_highlight_bg());

    let mut state = ListState::default().with_offset(app.list_scroll_offset.get());
    state.select(app.selected_item);

    StatefulWidget::render(list, area, buf, &mut state);

    // Persist the (possibly adjusted) offset for the next frame.
    app.list_scroll_offset.set(state.offset());

    // --- Sticky group header overlay ---
    // When a group header has scrolled above the viewport, render it pinned
    // at the top of the list's inner area so the user always knows which
    // group the visible items belong to.
    if app.board_drill_stage.is_none() {
        let offset = state.offset();
        if let Some(header_idx) = find_current_group_header(&app.display_list, offset) {
            // Only show sticky header when the original is NOT visible
            // (i.e., it has scrolled above the viewport).
            if header_idx < offset
                && let DisplayEntry::GroupHeader {
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
                // The block has Borders::ALL, so the inner area has 1-cell
                // margin on each side.
                let inner = area.inner(Margin::new(1, 1));
                let sticky_area = Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: 1,
                };
                // Fill the entire row with the sticky background so it
                // visually separates from the highlighted item below.
                let bg_style = Style::default().bg(theme.sticky_header_bg);
                let line = Line::from(vec![
                    Span::styled("  ", bg_style),
                    Span::styled(text, style),
                ]);
                Paragraph::new(line)
                    .style(bg_style)
                    .render(sticky_area, buf);
            }
        }
    }

    // Scrollbar - only when content overflows the viewport.
    let inner_height = area.height.saturating_sub(2) as usize;
    if total_rows > inner_height || state.offset() > 0 {
        // Convert the item-based offset to a row-based offset so the
        // scrollbar thumb position matches the actual viewport scroll.
        let row_offset: usize = item_heights.iter().take(state.offset()).sum();

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
        // blank rows below the last item.
        let max_row_offset = total_rows.saturating_sub(inner_height);
        let content_length = max_row_offset + 1;
        let position = row_offset.min(max_row_offset);
        let mut scrollbar_state = ScrollbarState::new(content_length)
            .viewport_content_length(inner_height)
            .position(position);

        let scrollbar_area = area.inner(Margin::new(0, 1));
        StatefulWidget::render(scrollbar, scrollbar_area, buf, &mut scrollbar_state);
    }
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

    let pr_badge = format!("PR#{}", rr.pr.number);
    let mut draft_suffix = String::new();
    if rr.pr.is_draft {
        draft_suffix.push_str(" draft");
    }
    let right = format!("{pr_badge}{draft_suffix}");

    let title = &rr.pr.title;

    // Layout: "{margin}R title    PR#N [draft]"
    let prefix = "R ";
    let available = content_width
        .saturating_sub(prefix.width())
        .saturating_sub(right.width())
        .saturating_sub(1);
    let truncated_title = truncate_str(title, available);

    let padding =
        content_width.saturating_sub(prefix.width() + truncated_title.width() + right.width());
    let pad_str: String = " ".repeat(padding);

    let margin_style = if is_selected {
        theme.style_tab_highlight()
    } else {
        ratatui_core::style::Style::default()
    };

    ListItem::new(Line::from(vec![
        Span::styled(margin, margin_style),
        Span::styled(prefix.to_string(), theme.style_review_request_marker()),
        Span::styled(truncated_title, theme.style_text()),
        Span::raw(pad_str),
        Span::styled(right, theme.style_badge_pr()),
    ]))
}

/// Format an unlinked PR entry for the left panel list.
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

    // Title: branch name for unlinked items.
    let title = &unlinked.branch;

    // Layout: "{margin}? title    PR#N [draft]"
    // Reserve space: 2 for "? " prefix, right.len() for badge, 1 for gap.
    let prefix = "? ";
    let available = content_width
        .saturating_sub(prefix.width())
        .saturating_sub(right.width())
        .saturating_sub(1);
    let truncated_title = truncate_str(title, available);

    let padding =
        content_width.saturating_sub(prefix.width() + truncated_title.width() + right.width());
    let pad_str: String = " ".repeat(padding);

    let (margin_style, marker_style, title_style, badge_style) = if is_selected {
        let hl = theme.style_tab_highlight();
        (hl, hl, hl, hl)
    } else {
        (
            ratatui_core::style::Style::default(),
            theme.style_unlinked_marker(),
            theme.style_text(),
            theme.style_badge_pr(),
        )
    };

    ListItem::new(Line::from(vec![
        Span::styled(margin, margin_style),
        Span::styled(prefix.to_string(), marker_style),
        Span::styled(truncated_title, title_style),
        Span::raw(pad_str),
        Span::styled(right, badge_style),
    ]))
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
    let is_working = app.claude_working.contains(&wi.id) || at_review_gate;
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
    // a derived `GitState` that reports uncommitted changes, unpushed
    // commits, or a behind-remote delta. `git_state.dirty` is the union
    // of modified-tracked-files and untracked-files (see `GitState`
    // doc comment); the merge guard in `App::advance_stage` /
    // `App::execute_merge` distinguishes them via
    // `App::worktree_cleanliness`, which reads the raw `WorktreeInfo`
    // fields. Both paths are pure cache reads and cannot shell out,
    // honouring the "no blocking I/O on the UI thread" invariant.
    let is_unclean = wi.repo_associations.iter().any(|a| {
        a.git_state
            .as_ref()
            .map(|gs| gs.dirty || gs.ahead > 0 || gs.behind > 0)
            .unwrap_or(false)
    });
    if is_unclean {
        right_parts.push((" !cl".to_string(), theme.style_badge_worktree_unclean()));
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
    // (Black + BOLD) while branch metadata stays muted (DarkGray).
    let hl = theme.style_tab_highlight();
    let (title_style, badge_style, right_badge_style, meta_style) = if is_selected {
        (
            hl,
            hl,
            hl,
            ratatui_core::style::Style::default().fg(ratatui_core::style::Color::DarkGray),
        )
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
    wi: Option<&crate::work_item::WorkItem>,
    theme: &Theme,
    block: Block<'_>,
    area: Rect,
    mergequeue_poll_error: Option<&str>,
) {
    let Some(wi) = wi else {
        let text = Text::from(vec![
            Line::from(""),
            Line::from("  Press Enter to start"),
            Line::from("  a session."),
        ]);
        let paragraph = Paragraph::new(text)
            .block(block)
            .style(theme.style_text_muted());
        paragraph.render(area, buf);
        return;
    };

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

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(format!("  {}", wi.title), theme.style_text())),
        Line::from(""),
    ];

    // Each detail row: "  Label:      value"
    let fields: Vec<(&str, &str)> = vec![
        ("Status", status_str),
        ("Backend", backend_str),
        ("Repo", &repo_str),
        ("Branch", branch_str),
        ("Worktree", &worktree_str),
        ("PR", &pr_str),
        ("Issue", &issue_str),
        ("Errors", &errors_str),
    ];

    for (label, value) in &fields {
        lines.push(Line::from(vec![
            Span::styled(format!("  {label:<12}"), label_style),
            Span::styled(value.to_string(), val_style(value)),
        ]));
    }

    // PR URL on its own line so it gets the full inner width.
    if let Some(url) = pr_url {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  PR URL", label_style)));
        lines.push(Line::from(Span::styled(
            format!("  {url}"),
            theme.style_text(),
        )));
    }

    lines.push(Line::from(""));
    let hint_lines: &[&str] = match wi.status {
        WorkItemStatus::Backlog => &["  Press Shift+Right to move to Planning."],
        WorkItemStatus::Done => &["  Done."],
        WorkItemStatus::Mergequeue => &[
            "  Waiting for PR to be merged.",
            "  Polling GitHub every 30s.",
            "  Shift+Left to move back to Review and stop polling.",
        ],
        WorkItemStatus::Planning
        | WorkItemStatus::Implementing
        | WorkItemStatus::Blocked
        | WorkItemStatus::Review => &["  Press Enter to start a session."],
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

    let fields: Vec<(&str, &str)> = vec![
        ("PR", &pr_str),
        ("Repo", &repo_str),
        ("Branch", detail.branch),
        ("State", state_str),
        ("Draft", draft_str),
        ("Review", review_str),
        ("Checks", checks_str),
    ];

    for (label, value) in &fields {
        lines.push(Line::from(vec![
            Span::styled(format!("  {label:<12}"), label_style),
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
        Line::from(vec![
            Span::raw(" "),
            Span::styled(" Claude Code ", cc_style),
            Span::styled(" | ", theme.style_title()),
            Span::styled(" Terminal ", term_style),
            Span::styled(input_suffix, theme.style_title()),
        ])
    } else {
        let title_text = format!(" Claude Code{input_suffix}");
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
                    Line::from("  Press Tab to switch back to Claude Code."),
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
                        Line::from("  Press Enter to start"),
                        Line::from("  a new session."),
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
                                &["  Press Shift+Right to move to Planning."]
                            }
                            Some(WorkItemStatus::Done) => &["  Done."],
                            Some(WorkItemStatus::Mergequeue) => &[
                                "  Waiting for PR to be merged.",
                                "  Polling GitHub every 30s.",
                                "  Shift+Left to move back to Review and stop polling.",
                            ],
                            _ => &["  Press Enter to start a session."],
                        };
                        for hint in hint_lines {
                            lines.push(Line::from(Span::styled(*hint, theme.style_text_muted())));
                        }
                        let text = Text::from(lines);
                        let paragraph = Paragraph::new(text).block(block);
                        paragraph.render(area, buf);
                    } else {
                        let poll_error = wi
                            .and_then(|w| app.mergequeue_poll_errors.get(&w.id))
                            .map(String::as_str);
                        draw_work_item_detail(buf, wi, theme, block, area, poll_error);
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
                draw_importable_pr_detail(
                    buf,
                    &ImportablePrDetail {
                        pr: &rr.pr,
                        repo_path: &rr.repo_path,
                        branch: &rr.branch,
                        hint: "Press Enter to import this review request as a work item.",
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
                Line::from("  Up/Down   - Navigate items"),
                Line::from("  Enter     - Open session / Import"),
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
fn render_selection_overlay(buf: &mut Buffer, inner_area: Rect, selection: &SelectionState) {
    let (start_row, start_col, end_row, end_col) = {
        let (ar, ac) = selection.anchor;
        let (cr, cc) = selection.current;
        if ar < cr || (ar == cr && ac <= cc) {
            (ar, ac, cr, cc)
        } else {
            (cr, cc, ar, ac)
        }
    };

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
        binding("?", "Settings / keybindings (this overlay)"),
        binding("Q / Ctrl+Q", "Quit"),
        Line::from(""),
        Line::styled("List focused", h),
        binding("Up / Down", "Navigate items"),
        binding("Enter", "Open session / Import"),
        binding("Shift+Right", "Advance stage"),
        binding("Shift+Left", "Retreat stage"),
        binding("Ctrl+D / Delete", "Delete work item"),
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
            "  (all other keys forwarded to Claude)",
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
            }],
            status_derived: false,
            errors: vec![],
        };
        let id = wi.id.clone();
        let mut app = make_app_with_work_item(wi);
        app.claude_working.insert(id);
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
            }],
            status_derived: false,
            errors: vec![],
        };
        let id = wi.id.clone();
        let mut app = make_app_with_work_item(wi);
        app.claude_working.insert(id);
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
}

#[cfg(test)]
mod snapshot_tests {
    use super::draw_to_buffer;
    use crate::app::{
        App, FocusPanel, ReviewGateOrigin, ReviewGateState, StubBackend, UserActionKey, ViewMode,
        is_selectable,
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
            });
        app.build_display_list();
        // Select the review request item (index 1: header at 0, item at 1).
        app.selected_item = Some(1);
        insta::assert_snapshot!(render(&mut app, 80, 24));
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
        // Select an item near the end to force scrolling.
        app.selected_item = Some(app.display_list.len().saturating_sub(2));
        insta::assert_snapshot!(render(&mut app, 80, 24));
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
        if let Some(pos) = app.display_list.iter().rposition(is_selectable) {
            app.selected_item = Some(pos);
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
        if let Some(pos) = app.display_list.iter().rposition(is_selectable) {
            app.selected_item = Some(pos);
        }
        // Short viewport so the BACKLOGGED header scrolls off -> sticky.
        insta::assert_snapshot!(render(&mut app, 80, 12));
    }
}
