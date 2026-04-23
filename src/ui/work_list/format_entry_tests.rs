use std::path::PathBuf;
use std::sync::Arc;

use unicode_width::UnicodeWidthStr;

use super::format_work_item_entry;
use crate::app::{App, StubBackend};
use crate::theme::Theme;
use crate::work_item::*;

/// Render a `ListItem` to a string by putting it in a List widget and
/// rendering to a buffer.
fn render_list_item_to_string(item: ratatui_widgets::list::ListItem<'_>, width: usize) -> String {
    use ratatui_core::buffer::Buffer;
    use ratatui_core::layout::Rect;
    use ratatui_core::widgets::Widget;
    let height = u16::try_from(item.height()).expect("list item height fits in u16");
    let width_u16 = u16::try_from(width).expect("test width fits in u16");
    let area = Rect::new(0, 0, width_u16, height);
    let list = ratatui_widgets::list::List::new(vec![item]);
    let mut buf = Buffer::empty(area);
    list.render(area, &mut buf);
    let mut lines = Vec::new();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width_u16 {
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
    let height = u16::try_from(item.height()).expect("list item height fits in u16");
    let width_u16 = u16::try_from(width).expect("test width fits in u16");
    let area = Rect::new(0, 0, width_u16, height);
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

/// Every line in the rendered item must fit within `max_width`.
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
            "line {i} is {line_width} cols but max is {max_width}: {line:?}",
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

/// Build a minimal Review-state work item with no session attached.
/// The state badge `[RV]` is the first thing to the right of the
/// 2-column left margin, so its cells sit at columns 2..=5 on row 0.
fn review_state_work_item() -> WorkItem {
    WorkItem {
        display_id: None,
        id: WorkItemId::LocalFile(PathBuf::from("/tmp/dim-badge-test.json")),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Pending review".to_string(),
        description: None,
        status: WorkItemStatus::Review,
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
    }
}

/// With no attached PTY session, the `[RV]` state badge must render
/// with `Modifier::DIM` and a `DarkGray` foreground - matching the
/// `dim_badge_style` helper's contract. Verifying the first cell
/// of the badge (column 2, row 0) is sufficient because the helper
/// applies uniformly to every cell of the span.
#[test]
fn work_item_badge_dims_when_no_session() {
    use ratatui_core::style::{Color, Modifier};

    let wi = review_state_work_item();
    let app = make_app_with_work_item(wi);
    let theme = Theme::default_theme();

    let item = format_work_item_entry(&app, 0, 40, &theme, false);
    let buf = render_list_item_to_buffer(item, 40, false);

    // Sanity-check the layout: cell (2,0) is the `[` of `[RV]`.
    let badge_cell = buf.cell((2, 0)).expect("badge cell exists");
    assert_eq!(
        badge_cell.symbol(),
        "[",
        "expected `[` at column 2, got {:?}",
        badge_cell.symbol()
    );

    assert!(
        badge_cell.modifier.contains(Modifier::DIM),
        "badge cell should carry Modifier::DIM when no session is attached; modifier={:?}",
        badge_cell.modifier
    );
    assert_eq!(
        badge_cell.fg,
        Color::DarkGray,
        "badge cell fg should be DarkGray when no session is attached, got {:?}",
        badge_cell.fg
    );
}

/// With a live session registered for the work item, the badge must
/// render in its normal theme style - `dim_badge_style` is a no-op
/// and the helper must not leak DIM or `DarkGray` into the output.
#[test]
fn work_item_badge_undimmed_when_session_attached() {
    use std::sync::{Arc, Mutex};

    use ratatui_core::style::{Color, Modifier};

    use crate::work_item::SessionEntry;

    let wi = review_state_work_item();
    let id = wi.id.clone();
    let status = wi.status;
    let mut app = make_app_with_work_item(wi);

    // Register a minimal fake SessionEntry under the work item's
    // current stage, mirroring the pattern used by
    // `session_lookup_requires_correct_stage` in app.rs.
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.sessions.insert(
        (id, status),
        SessionEntry {
            parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );

    let theme = Theme::default_theme();
    let item = format_work_item_entry(&app, 0, 40, &theme, false);
    let buf = render_list_item_to_buffer(item, 40, false);

    let badge_cell = buf.cell((2, 0)).expect("badge cell exists");
    assert_eq!(
        badge_cell.symbol(),
        "[",
        "expected `[` at column 2, got {:?}",
        badge_cell.symbol()
    );

    assert!(
        !badge_cell.modifier.contains(Modifier::DIM),
        "badge cell must NOT carry Modifier::DIM when a session is attached; modifier={:?}",
        badge_cell.modifier
    );
    assert_ne!(
        badge_cell.fg,
        Color::DarkGray,
        "badge cell fg must NOT collapse to DarkGray when a session is attached",
    );
}
