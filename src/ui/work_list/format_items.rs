//! List-item formatters for the work-item list: review requests,
//! unlinked PRs, and work items. Each returns a multi-line `ListItem`
//! sized for the panel width passed in by the caller.
use ratatui_core::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::theme::Theme;
use crate::work_item::{CheckStatus, MergeableState, PrState, WorkItemKind, WorkItemStatus};

use ratatui_widgets::list::ListItem;

use super::super::common::{SPINNER_FRAMES, dim_badge_style, wrap_text, wrap_two_widths};

/// Format a review-requested PR entry for the left panel list.
pub fn format_review_request_item<'a>(
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
        .saturating_sub(usize::from(!right.is_empty()));
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
pub fn format_unlinked_item<'a>(
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

    let mut lines: Vec<Line<'_>> = Vec::new();

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
        .map_or_else(|| "<unknown repo>".to_string(), str::to_string);
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
/// Returns a 2-line `ListItem`:
///   Line 1: title (+ PR badge + CI badge if present)
///   Line 2: repo-name  branch-name  [no wt] (all muted)
pub fn format_work_item_entry<'a>(
    app: &App,
    idx: usize,
    max_width: usize,
    theme: &Theme,
    is_selected: bool,
) -> ListItem<'a> {
    // Minimum number of display columns reserved for the title so it never
    // vanishes when badges consume all available width.
    const MIN_TITLE_BUDGET: usize = 5;

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
    // `WorktreeService::list_worktrees` + `GithubClient::fetch_live_merge_state`
    // precheck and distinguishes the variants via
    // `MergeReadiness::classify`. The chip render here is a pure cache
    // read and cannot shell out, honouring the "no blocking I/O on
    // the UI thread" invariant.
    //
    // Ahead/behind state is rendered via the dedicated `!pushed` /
    // `!pulled` chips below; `!cl` is exclusively for "uncommitted
    // changes in the worktree" so a clean-but-diverged branch no
    // longer flags as unclean.
    let is_unclean = wi
        .repo_associations
        .iter()
        .any(|a| a.git_state.as_ref().is_some_and(|gs| gs.dirty));
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
        .any(|a| a.git_state.as_ref().is_some_and(|gs| gs.ahead > 0));
    if needs_push {
        right_parts.push((" !pushed".to_string(), theme.style_badge_pushed()));
    }

    // Needs-pull chip: any repo association is behind its upstream.
    // Rendered whenever `git_state.behind > 0` for at least one
    // association. Coexists with `!cl` and `!pushed` on a row that is
    // dirty AND diverged in both directions.
    let needs_pull = wi
        .repo_associations
        .iter()
        .any(|a| a.git_state.as_ref().is_some_and(|gs| gs.behind > 0));
    if needs_pull {
        right_parts.push((" !pulled".to_string(), theme.style_badge_pulled()));
    }

    // Multi-repo indicator.
    let repo_count = wi.repo_associations.len();
    if repo_count > 1 {
        right_parts.push((format!(" [{repo_count} repos]"), theme.style_text_muted()));
    }

    // Dim every right-side badge style in one pass when the work item has
    // no live Claude PTY session attached. Doing it here, after the block
    // that populates `right_parts` and before the width-budget loop that
    // decides which badges to keep, avoids having to wrap each individual
    // `push` site above and guarantees no badge escapes the rule.
    for (_, style) in &mut right_parts {
        *style = dim_badge_style(*style, has_session);
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
        .saturating_sub(usize::from(!right_text.is_empty()));

    // When selected, the List widget only sets bg (via style_tab_highlight_bg).
    // We apply fg per-span here so title+badge get the original highlight look
    // (Black + BOLD) while branch metadata uses the theme-owned
    // `style_meta_selected` (paired with `tab_highlight_bg` inside Theme so
    // the fg+bg pair cannot drift when the highlight bg is retuned).
    let hl = theme.style_tab_highlight();
    let (title_style, badge_style, right_badge_style, meta_style) = if is_selected {
        // Dim the badge styles even on a selected row - "dim = no session"
        // is the single unambiguous encoding and must not be masked by the
        // highlight bar. Title style stays unchanged because title text
        // colour is orthogonal to the badge rule.
        (
            hl,
            dim_badge_style(hl, has_session),
            dim_badge_style(hl, has_session),
            theme.style_meta_selected(),
        )
    } else {
        let ts = if wi.status == WorkItemStatus::Done {
            theme.style_done_item()
        } else {
            theme.style_text()
        };
        (
            ts,
            dim_badge_style(theme.style_stage_badge(wi.status), has_session),
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
            Span::styled(
                "[RR]".to_string(),
                dim_badge_style(theme.style_badge_review_request_kind(), has_session),
            ),
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
            Span::styled(
                "[RG]".to_string(),
                dim_badge_style(theme.style_badge_review_gate(), has_session),
            ),
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

#[cfg(test)]
#[path = "format_entry_tests.rs"]
mod format_entry_tests;
