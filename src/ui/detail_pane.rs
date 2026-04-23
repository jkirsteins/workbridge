//! Structured detail views for the right panel: work item detail and
//! importable PR detail (unlinked / review-request variants).
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;
use ratatui_core::text::{Line, Span, Text};
use ratatui_core::widgets::Widget;
use ratatui_widgets::block::Block;
use ratatui_widgets::paragraph::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::click_targets::ClickKind;
use crate::theme::Theme;
use crate::work_item::{
    BackendType, CheckStatus, PrState, ReviewDecision, WorkItemError, WorkItemStatus,
};

/// Saturating narrow `usize` -> `u16` for layout geometry. Values that
/// overflow `u16::MAX` are clamped; real TUI widths never exceed
/// `u16::MAX` in practice, so this is effectively a total conversion
/// without reaching for an `as` cast.
fn usize_to_u16_sat(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Format a `WorkItemError` into a user-facing message and optional suggestion.
pub fn format_work_item_error(error: &WorkItemError) -> (String, Option<String>) {
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
        WorkItemError::IssueNotFound {
            repo_path,
            issue_number,
        } => (
            format!("Issue #{issue_number} not found in {}", repo_path.display()),
            Some("The issue may have been deleted or the number is wrong.".into()),
        ),
    }
}

/// Draw a structured detail view for a work item with no active session.
///
/// Shows title, status, backend type, repo, branch, worktree, PR, PR URL,
/// issue, and errors, followed by a stage-specific hint. When a
/// mergequeue poll error is supplied, it is rendered below the hint so
/// it survives longer than a transient `status_message`.
pub fn draw_work_item_detail(
    buf: &mut Buffer,
    app: &App,
    wi: Option<&crate::work_item::WorkItem>,
    theme: &Theme,
    block: Block<'_>,
    area: Rect,
    mergequeue_poll_error: Option<&str>,
) {
    const LABEL_INDENT: u16 = 2; // "  " indent before every row.
    const LABEL_WIDTH: u16 = 12; // Padded label column width.

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
    let mut registry = app.click_tracking.registry.borrow_mut();

    let label_style = theme.style_heading();
    let none_style = theme.style_text_muted();
    let rows = build_detail_rows(wi);

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

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Render detail rows in the historical order. Repo and Branch
    // are the two interactive rows; everything else is rendered as a
    // non-interactive "  Label       value" row.
    let plain_row = |label: &str, value: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {label:<12}"), label_style),
            Span::styled(value.to_string(), val_style(value)),
        ])
    };

    push_detail_title(&mut lines, &mut registry, theme, inner, wi);

    // lines[2]: blank separator
    lines.push(Line::from(""));

    lines.push(plain_row("Status", rows.status));
    lines.push(plain_row("Backend", rows.backend));

    push_interactive_value_row(
        &mut lines,
        &mut registry,
        theme,
        &plain_row,
        inner,
        InteractiveValueRow {
            label: "Repo",
            value: &rows.repo,
            click_kind: ClickKind::RepoPath,
            value_x_offset: LABEL_INDENT + LABEL_WIDTH,
        },
    );
    push_interactive_value_row(
        &mut lines,
        &mut registry,
        theme,
        &plain_row,
        inner,
        InteractiveValueRow {
            label: "Branch",
            value: &rows.branch,
            click_kind: ClickKind::Branch,
            value_x_offset: LABEL_INDENT + LABEL_WIDTH,
        },
    );

    lines.push(plain_row("Worktree", &rows.worktree));
    lines.push(plain_row("PR", &rows.pr));
    lines.push(plain_row("Issue", &rows.issue));
    lines.push(plain_row("Errors", &rows.errors));

    push_pr_url_block(
        &mut lines,
        &mut registry,
        theme,
        inner,
        label_style,
        rows.pr_url.as_ref(),
        LABEL_INDENT,
    );

    lines.push(Line::from(""));
    let hint_lines: &[&str] = stage_hint_lines(wi);
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

/// Pre-formatted strings for every row of the work-item detail view.
/// Computed once at the top of `draw_work_item_detail` so the
/// renderer does not re-derive them per row.
struct DetailRowStrings {
    status: &'static str,
    backend: &'static str,
    repo: String,
    branch: String,
    worktree: String,
    pr: String,
    pr_url: Option<String>,
    issue: String,
    errors: String,
}

/// Derive all of the detail-view value strings from `wi`. Each field
/// collapses to `"(none)"` when the underlying data is absent so the
/// renderer can uniformly style those rows as muted.
fn build_detail_rows(wi: &crate::work_item::WorkItem) -> DetailRowStrings {
    let first_assoc = wi.repo_associations.first();

    let status = match wi.status {
        WorkItemStatus::Backlog => "Backlog",
        WorkItemStatus::Planning => "Planning",
        WorkItemStatus::Implementing => "Implementing",
        WorkItemStatus::Blocked => "Blocked",
        WorkItemStatus::Review => "Review",
        WorkItemStatus::Mergequeue => "Mergequeue",
        WorkItemStatus::Done => "Done",
    };

    let backend = match wi.backend_type {
        BackendType::LocalFile => "Local file",
        BackendType::GithubIssue => "GitHub issue",
        BackendType::GithubProject => "GitHub project",
    };

    let repo = first_assoc.map_or_else(
        || "(none)".to_string(),
        |a| a.repo_path.display().to_string(),
    );

    let branch = first_assoc
        .and_then(|a| a.branch.as_deref())
        .map_or_else(|| "(none)".to_string(), std::string::ToString::to_string);

    let worktree = first_assoc
        .and_then(|a| a.worktree_path.as_ref())
        .map_or_else(|| "(none)".to_string(), |p| p.display().to_string());

    let pr = first_assoc.and_then(|a| a.pr.as_ref()).map_or_else(
        || "(none)".to_string(),
        |pr| format!("#{} - {}", pr.number, pr.title),
    );

    // PR URL is rendered on its own dedicated line below the field block
    // (not as a regular `label  value` row) so that the URL gets the full
    // inner width of the panel instead of just the few columns left after
    // the label prefix. Long real-world URLs (`/<long-org>/<long-repo>/
    // pull/<n>`) would silently truncate at the panel edge inside the
    // single-line `Paragraph` otherwise.
    let pr_url = first_assoc
        .and_then(|a| a.pr.as_ref())
        .map(|pr| pr.url.clone());

    let issue = first_assoc.and_then(|a| a.issue.as_ref()).map_or_else(
        || "(none)".to_string(),
        |issue| format!("#{} - {}", issue.number, issue.title),
    );

    let errors = if wi.errors.is_empty() {
        "(none)".to_string()
    } else {
        wi.errors
            .iter()
            .map(|e| format_work_item_error(e).0)
            .collect::<Vec<_>>()
            .join("; ")
    };

    DetailRowStrings {
        status,
        backend,
        repo,
        branch,
        worktree,
        pr,
        pr_url,
        issue,
        errors,
    }
}

/// Push the work item's title row (lines 0-1) into `lines`. When
/// the title is non-empty the full title value is registered as a
/// click-to-copy target.
fn push_detail_title(
    lines: &mut Vec<Line<'static>>,
    registry: &mut crate::click_targets::ClickRegistry,
    theme: &Theme,
    inner: Rect,
    wi: &crate::work_item::WorkItem,
) {
    const LABEL_INDENT: u16 = 2;
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
        let title_width = usize_to_u16_sat(UnicodeWidthStr::width(title_value.as_str()));
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
}

/// Inputs to `push_interactive_value_row`. Bundled so the helper
/// does not exceed clippy's `too_many_arguments` threshold.
#[derive(Clone, Copy)]
struct InteractiveValueRow<'a> {
    label: &'a str,
    value: &'a str,
    click_kind: ClickKind,
    value_x_offset: u16,
}

/// Push an interactive value row (Repo / Branch) with an
/// underline-styled value span and a click-to-copy registration. When
/// the value is `"(none)"`, falls back to the plain non-interactive
/// row via `plain_row_fn` (no underline, no registration).
fn push_interactive_value_row(
    lines: &mut Vec<Line<'static>>,
    registry: &mut crate::click_targets::ClickRegistry,
    theme: &Theme,
    plain_row_fn: &dyn Fn(&str, &str) -> Line<'static>,
    inner: Rect,
    row: InteractiveValueRow<'_>,
) {
    let InteractiveValueRow {
        label,
        value,
        click_kind,
        value_x_offset,
    } = row;
    let line_index = usize_to_u16_sat(lines.len());
    if value == "(none)" {
        lines.push(plain_row_fn(label, value));
    } else {
        let value_width = usize_to_u16_sat(UnicodeWidthStr::width(value));
        registry.push_copy(
            Rect {
                x: inner.x.saturating_add(value_x_offset),
                y: inner.y.saturating_add(line_index),
                width: value_width,
                height: 1,
            },
            click_kind,
            value.to_string(),
        );
        lines.push(Line::from(vec![
            Span::styled(format!("  {label:<12}"), theme.style_heading()),
            Span::styled(value.to_string(), theme.style_interactive()),
        ]));
    }
}

/// Append the "PR URL" block: a blank separator, a label line, then
/// the URL on its own full-width line so long URLs are not truncated
/// by the value column. The URL is registered as a click-to-copy
/// target. No-op when `pr_url` is `None`.
fn push_pr_url_block(
    lines: &mut Vec<Line<'static>>,
    registry: &mut crate::click_targets::ClickRegistry,
    theme: &Theme,
    inner: Rect,
    label_style: ratatui_core::style::Style,
    pr_url: Option<&String>,
    label_indent: u16,
) {
    let Some(url) = pr_url else {
        return;
    };
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  PR URL", label_style)));
    let line_index = usize_to_u16_sat(lines.len());
    let url_value = url.clone();
    let url_width = usize_to_u16_sat(UnicodeWidthStr::width(url_value.as_str()));
    registry.push_copy(
        Rect {
            x: inner.x.saturating_add(label_indent),
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

/// Pick the stage-specific hint shown below the detail rows. The
/// Planning / Implementing / Blocked / Review bucket branches on
/// whether any repo association has a stale worktree (so the hint
/// can steer the user toward recovery instead of a fresh session).
fn stage_hint_lines(wi: &crate::work_item::WorkItem) -> &'static [&'static str] {
    match wi.status {
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
    }
}

/// Detail fields for rendering an importable PR in the right panel.
pub struct ImportablePrDetail<'a> {
    pub pr: &'a crate::work_item::PrInfo,
    pub repo_path: &'a std::path::Path,
    pub branch: &'a str,
    pub hint: &'a str,
    /// Optional authoritative reviewer-identity list for review-request
    /// detail panels. Each element is a display string ready to join
    /// with ", " ("you", "team-core", etc.). `None` for unlinked-PR
    /// detail panels where the field is irrelevant; `Some` with at
    /// least one entry for review-request detail panels. Names are
    /// never truncated - the detail panel wraps naturally.
    pub requested_from: Option<Vec<String>>,
}

/// Draw a structured detail view for an importable PR (unlinked or review request).
///
/// Shows PR title, number/URL, repo, branch, state, draft status,
/// review decision, and CI checks, followed by a contextual hint.
pub fn draw_importable_pr_detail(
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
