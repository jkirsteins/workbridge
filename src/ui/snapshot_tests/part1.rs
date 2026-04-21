//! Snapshot tests: empty app, work-item list panel, session indicators.
//! See `src/ui/snapshot_tests/mod.rs` for shared helpers.

use super::*;

#[test]
fn empty_app_default_view() {
    let mut app = App::new();
    insta::assert_snapshot!(render(&mut app, 80, 24));
}

#[test]
fn empty_app_with_status_message() {
    let mut app = App::new();
    app.shell.status_message = Some("Press Ctrl+N to create a work item".to_string());
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

/// When no `current_user_login` is known, the detail panel cannot
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
    let item = crate::ui::work_list::format_unlinked_item(&app, 0, max_width, &theme, false);
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
    let first_chunk = first_content.rfind("PR#").map_or_else(
        || first_content.trim_end().to_string(),
        |idx| first_content[..idx].trim_end().to_string(),
    );
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
    app.shell.focus = FocusPanel::Right;
    insta::assert_snapshot!(render(&mut app, 80, 24));
}

#[test]
fn work_item_with_context_bar() {
    use crate::work_item::IssueInfo;
    let mut wi = make_work_item("ctx-1", "Fix resize bug", WorkItemStatus::Backlog, None, 1);
    // Add issue with labels to trigger the context bar.
    wi.repo_associations[0].issue = Some(IssueInfo {
        number: 42,
        title: "Fix resize bug".into(),
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
    let mut wi = make_work_item("ctx-3", "Fix resize bug", WorkItemStatus::Backlog, None, 1);
    wi.repo_associations[0].issue = Some(IssueInfo {
        number: 42,
        title: "Fix resize bug".into(),
        labels: vec!["bug".into()],
    });
    let mut app = app_with_items(vec![wi], vec![]);
    app.selected_item = app.display_list.iter().position(is_selectable);
    app.shell.status_message = Some("Right panel focused - press Ctrl+] to return".into());
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
    app.shell.status_message = Some("This should be hidden".into());
    app.activities.start("Creating pull request...");
    app.activities.spinner_tick = 3; // Pick a specific frame for deterministic snapshot.
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
    app.activities.start("Running review gate...");
    app.activities.start("Creating pull request...");
    app.activities.spinner_tick = 0;
    insta::assert_snapshot!(render(&mut app, 80, 24));
}
