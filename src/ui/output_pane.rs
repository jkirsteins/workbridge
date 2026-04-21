//! PTY output pane for the active session + right-panel empty-state views.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Margin, Rect};
use ratatui_core::text::{Line, Span, Text};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::paragraph::Paragraph;
use tui_term::widget::PseudoTerminal;

use super::detail_pane::{
    ImportablePrDetail, draw_importable_pr_detail, draw_work_item_detail, format_work_item_error,
};
use super::selection::render_selection_overlay;
use crate::app::{App, DisplayEntry, FocusPanel, RightPanelTab, UserActionKey};
use crate::theme::Theme;
use crate::work_item::WorkItemStatus;

pub fn draw_pane_output(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
    // When the settings overlay is open, dim background panels.
    let border_style = if app.show_settings {
        theme.style_border_unfocused()
    } else if app.shell.focus == FocusPanel::Right {
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
    } else if app.shell.focus == FocusPanel::Right {
        " [INPUT] "
    } else {
        " "
    };

    let title_line: Line<'_> = if has_worktree {
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
        let backend_tab = format!(
            " {} ",
            app.agent_backend_display_name_with_permission_marker()
        );
        Line::from(vec![
            Span::raw(" "),
            Span::styled(backend_tab, cc_style),
            Span::styled(" | ", theme.style_title()),
            Span::styled(" Terminal ", term_style),
            Span::styled(input_suffix, theme.style_title()),
        ])
    } else {
        let title_text = format!(
            " {}{input_suffix}",
            app.agent_backend_display_name_with_permission_marker()
        );
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
                        app.agent_backend_display_name_with_permission_marker()
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
                .is_some_and(|wi| app.rebase_gates.contains_key(&wi.id));

            if rebase_gate_active {
                let spinner_chars = [b'|', b'/', b'-', b'\\'];
                let frame = app.activities.spinner_tick % spinner_chars.len();
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
                .is_some_and(|wi| app.review_gates.contains_key(&wi.id));

            if review_gate_active {
                let spinner_chars = [b'|', b'/', b'-', b'\\'];
                let frame = app.activities.spinner_tick % spinner_chars.len();
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
                    let worktree_creating = app.work_items.get(*wi_idx).is_some_and(|wi| {
                        app.user_action_work_item(&UserActionKey::WorktreeCreate) == Some(&wi.id)
                    });

                    if worktree_creating {
                        let spinner_chars = [b'|', b'/', b'-', b'\\'];
                        let frame = app.activities.spinner_tick % spinner_chars.len();
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
                    let errors = wi.map(|w| &w.errors).filter(|e| !e.is_empty());

                    if let Some(errors) = errors {
                        let mut lines = vec![
                            Line::from(""),
                            Line::from(Span::styled("  Errors:", theme.style_error())),
                        ];
                        for error in errors {
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
