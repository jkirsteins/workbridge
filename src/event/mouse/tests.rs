use ratatui_core::layout::Rect as UiRect;

use crate::app::{App, DisplayEntry, RightPanelTab};
use crate::event::mouse::{MouseTarget, handle_mouse_with_terminal_size, mouse_target_with_size};
use crate::salsa::ct::event::{MouseButton, MouseEvent, MouseEventKind};

// -- Chrome click (click-to-copy) regression tests --
//
// These tests exercise `handle_mouse_with_terminal_size` directly
// so the geometric classifier (`mouse_target_with_size`) runs
// against a known terminal size. Passing a real size is essential:
// under `cargo test`, `crossterm::terminal::size()` returns `Err`
// and the public `handle_mouse` would collapse to
// `MouseTarget::None`, silently bypassing the PTY-area dispatch
// and masking any regression where right-panel clicks swallow
// interactive labels. See `docs/UI.md` "Interactive labels".
//
// Terminal chosen: 120 cols x 40 rows. With `App::new()` (no
// status bar, no context bar, no drawer open), `mouse_target_with_size`
// computes `left_width=30`, right-panel inner rect
// `(col in [31..119), row in [2..39))`. Any coordinate inside
// that rect is classified as `RightPanel` - that's the exact
// arm the priority check must rescue.

const TEST_COLS: u16 = 120;
const TEST_ROWS: u16 = 40;
const TEST_SIZE: Option<(u16, u16)> = Some((TEST_COLS, TEST_ROWS));

/// Make a `MouseEvent` with `KeyModifiers::NONE`. Keeps test
/// bodies readable - the tests only care about kind/column/row.
fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    use ratatui_crossterm::crossterm::event::KeyModifiers;
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// Sanity check: confirm the test terminal size really does put
/// our sample coordinate inside the right panel. If this test
/// ever breaks (because `layout::compute` or
/// `mouse_target_with_size` changes), the rest of the chrome
/// click tests below need their coordinates updated too.
#[test]
fn mouse_target_with_size_classifies_right_panel_for_test_size() {
    let app = App::new();
    // (column=50, row=10) sits comfortably inside
    // (col in [31..119), row in [2..39)) for a 120x40 terminal.
    let target = mouse_target_with_size(&app, 50, 10, (TEST_COLS, TEST_ROWS));
    assert!(
        matches!(target, MouseTarget::RightPanel { .. }),
        "expected RightPanel classification, got {target:?}",
    );
}

/// **Regression for the "labels are unreachable" bug.**
///
/// Seed a click target at a coordinate that `mouse_target_with_size`
/// classifies as `MouseTarget::RightPanel`, then dispatch
/// `Down(Left)` + `Up(Left)` through
/// `handle_mouse_with_terminal_size` with the real terminal size.
/// The priority check in `handle_mouse_with_terminal_size` must
/// route the click through `handle_chrome_click_fallback` instead
/// of the text-selection branch, and a toast must fire.
///
/// Before the fix, the `RightPanel` arm would match first,
/// `active_session_entry_mut_for_tab` would return `None` (no
/// session on a fresh `App`), and the Down event would be
/// consumed as a no-op selection click - `pending_chrome_click`
/// would never get set and no toast would be pushed.
#[test]
fn chrome_click_inside_right_panel_still_fires() {
    use ratatui_core::layout::Rect;

    use crate::click_targets::ClickKind;

    let mut app = App::new();
    // Register a target that overlaps the right-panel inner area.
    {
        let mut reg = app.click_registry.borrow_mut();
        reg.push_copy(
            Rect {
                x: 40,
                y: 10,
                width: 30,
                height: 1,
            },
            ClickKind::Branch,
            "feat/my-branch".to_string(),
        );
    }

    // Independently assert that the click coordinate really does
    // hit the RightPanel arm with the test terminal size. Without
    // this, a future change to the layout math could silently
    // move the test click outside the right panel and make the
    // whole test vacuous.
    let classification = mouse_target_with_size(&app, 50, 10, (TEST_COLS, TEST_ROWS));
    assert!(
        matches!(classification, MouseTarget::RightPanel { .. }),
        "test coordinate must land inside the right panel, got {classification:?}",
    );

    let down = mouse(MouseEventKind::Down(MouseButton::Left), 50, 10);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 50, 10);

    assert!(handle_mouse_with_terminal_size(&mut app, down, TEST_SIZE));
    assert!(
        app.pending_chrome_click.is_some(),
        "Down(Left) on a registered label must arm the pending click \
         even when geometric classification says RightPanel",
    );
    assert!(handle_mouse_with_terminal_size(&mut app, up, TEST_SIZE));
    assert!(
        app.pending_chrome_click.is_none(),
        "Up(Left) must clear the pending click",
    );
    assert_eq!(app.toasts.entries.len(), 1, "one toast must be queued");
    assert!(
        app.toasts.entries[0].text.contains("feat/my-branch"),
        "toast text must mention the copied value, got {:?}",
        app.toasts.entries[0].text,
    );
}

/// Regression companion: a drag between Down and Up cancels the
/// click-to-copy gesture even when the click coordinate is
/// classified as `RightPanel`. Guards against a future priority
/// check that forgets to honour the drag-cancel invariant.
#[test]
fn chrome_click_drag_inside_right_panel_cancels() {
    use ratatui_core::layout::Rect;

    use crate::click_targets::ClickKind;

    let mut app = App::new();
    {
        let mut reg = app.click_registry.borrow_mut();
        reg.push_copy(
            Rect {
                x: 40,
                y: 10,
                width: 30,
                height: 1,
            },
            ClickKind::PrUrl,
            "https://example.com/pull/42".to_string(),
        );
    }

    let down = mouse(MouseEventKind::Down(MouseButton::Left), 50, 10);
    let drag = mouse(MouseEventKind::Drag(MouseButton::Left), 52, 10);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 52, 10);

    handle_mouse_with_terminal_size(&mut app, down, TEST_SIZE);
    handle_mouse_with_terminal_size(&mut app, drag, TEST_SIZE);
    handle_mouse_with_terminal_size(&mut app, up, TEST_SIZE);
    assert!(
        app.toasts.is_empty(),
        "drag must cancel the copy gesture, got toasts={:?}",
        app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>(),
    );
    assert!(app.pending_chrome_click.is_none());
}

/// Negative test: a right-panel click that does NOT hit any
/// registered target must fall through to the normal `RightPanel`
/// arm (which is a no-op on a fresh `App` with no session), NOT
/// spuriously arm a pending chrome click. Guards against a
/// future priority check that accidentally hit-tests the empty
/// registry loosely (e.g. "any click in the area arms").
#[test]
fn right_panel_click_without_registry_hit_does_not_arm_chrome_click() {
    use ratatui_core::layout::Rect;

    use crate::click_targets::ClickKind;

    let mut app = App::new();
    // Register a target somewhere on the same row, but NOT at
    // the click coordinate.
    {
        let mut reg = app.click_registry.borrow_mut();
        reg.push_copy(
            Rect {
                x: 80,
                y: 10,
                width: 10,
                height: 1,
            },
            ClickKind::RepoPath,
            "never-copied".to_string(),
        );
    }

    // Sanity: (50, 10) is inside the right panel but outside the
    // registered rect.
    assert!(matches!(
        mouse_target_with_size(&app, 50, 10, (TEST_COLS, TEST_ROWS)),
        MouseTarget::RightPanel { .. }
    ));

    let down = mouse(MouseEventKind::Down(MouseButton::Left), 50, 10);
    handle_mouse_with_terminal_size(&mut app, down, TEST_SIZE);
    assert!(
        app.pending_chrome_click.is_none(),
        "click outside any registered target must not arm a chrome copy",
    );

    let up = mouse(MouseEventKind::Up(MouseButton::Left), 50, 10);
    handle_mouse_with_terminal_size(&mut app, up, TEST_SIZE);
    assert!(
        app.toasts.is_empty(),
        "unregistered click must not push a toast, got {:?}",
        app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>(),
    );
}

/// Future-proofing: the priority check must also rescue clicks
/// on registered targets drawn inside the global drawer. Today
/// `draw_work_item_detail` is the only caller that pushes
/// targets, but the priority rule is structural - labels
/// rendered anywhere in chrome should stay clickable. This test
/// forces the `GlobalDrawer` classification by opening the
/// drawer and seeding a target in its inner area.
#[test]
fn chrome_click_inside_global_drawer_still_fires() {
    use ratatui_core::layout::Rect;

    use crate::click_targets::ClickKind;

    let mut app = App::new();
    app.global_drawer_open = true;

    // Pick a drawer-inside coordinate. `compute_drawer(120, 40)`
    // produces a drawer wide enough that (col=10, row=30) is
    // comfortably inside the inner area for this test size.
    // Verify the classification before relying on it.
    let classification = mouse_target_with_size(&app, 10, 30, (TEST_COLS, TEST_ROWS));
    assert!(
        matches!(classification, MouseTarget::GlobalDrawer { .. }),
        "test coordinate must land inside the global drawer, got {classification:?}",
    );

    {
        let mut reg = app.click_registry.borrow_mut();
        reg.push_copy(
            Rect {
                x: 5,
                y: 30,
                width: 20,
                height: 1,
            },
            ClickKind::Title,
            "workbridge".to_string(),
        );
    }

    let down = mouse(MouseEventKind::Down(MouseButton::Left), 10, 30);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 10, 30);

    assert!(handle_mouse_with_terminal_size(&mut app, down, TEST_SIZE));
    assert!(
        app.pending_chrome_click.is_some(),
        "priority check must also rescue drawer-area clicks",
    );
    assert!(handle_mouse_with_terminal_size(&mut app, up, TEST_SIZE));
    assert_eq!(app.toasts.entries.len(), 1, "one toast must be queued");
    assert!(
        app.toasts.entries[0].text.contains("workbridge"),
        "toast text must mention the copied value, got {:?}",
        app.toasts.entries[0].text,
    );
}

/// Stale-pending regression: when a `SelectDown` lands on a
/// registered label and the matching `SelectUp` arrives somewhere
/// else (no intervening `Drag`, as happens on terminals that
/// coalesce drags or over lossy SSH sessions), the pending
/// click-to-copy state must NOT survive the unmatched up. If it
/// did, a later unrelated `SelectUp` on a same-kind label could
/// fire a false copy without a fresh matching `SelectDown`.
///
/// This test drives the exact failure: down on label A, up at
/// an unregistered right-panel coordinate, and asserts the
/// pending state is cleared. It then synthesizes a later up on
/// label A (simulating the attacker sequence) and asserts no
/// toast is pushed, since there was no fresh down on A.
#[test]
fn unmatched_select_up_clears_stale_pending_chrome_click() {
    use ratatui_core::layout::Rect;

    use crate::click_targets::ClickKind;

    let mut app = App::new();
    {
        let mut reg = app.click_registry.borrow_mut();
        reg.push_copy(
            Rect {
                x: 40,
                y: 10,
                width: 30,
                height: 1,
            },
            ClickKind::Branch,
            "feat/my-branch".to_string(),
        );
    }

    // Sanity: both coordinates are inside the right-panel area
    // with the test terminal size, so the classifier's RightPanel
    // arm is the one we're testing against.
    assert!(matches!(
        mouse_target_with_size(&app, 50, 10, (TEST_COLS, TEST_ROWS)),
        MouseTarget::RightPanel { .. }
    ));
    assert!(matches!(
        mouse_target_with_size(&app, 100, 10, (TEST_COLS, TEST_ROWS)),
        MouseTarget::RightPanel { .. }
    ));

    // Down on label A at (50, 10) - inside the registered rect.
    let down_on_label = mouse(MouseEventKind::Down(MouseButton::Left), 50, 10);
    handle_mouse_with_terminal_size(&mut app, down_on_label, TEST_SIZE);
    assert!(
        app.pending_chrome_click.is_some(),
        "priority check must arm pending on down over a registered label",
    );

    // Up at (100, 10) - still inside the right panel but
    // OUTSIDE the registered rect. No intervening Drag event:
    // this is the "lossy terminal" case. The stale-pending
    // clear must drop the pending state here.
    let up_off_label = mouse(MouseEventKind::Up(MouseButton::Left), 100, 10);
    handle_mouse_with_terminal_size(&mut app, up_off_label, TEST_SIZE);
    assert!(
        app.pending_chrome_click.is_none(),
        "unmatched SelectUp must clear stale pending_chrome_click, \
         otherwise a later unrelated SelectUp on a same-kind label \
         could fire a false copy",
    );
    assert!(
        app.toasts.is_empty(),
        "unmatched up must not push a toast, got {:?}",
        app.toasts.iter().map(|t| &t.text).collect::<Vec<_>>(),
    );

    // Now simulate the attacker sequence: another `SelectUp` on
    // label A with no fresh matching `SelectDown`. This reaches
    // the priority path (registry hit) and routes to the chrome
    // click fallback. The fallback reads `pending` - which must
    // be None thanks to the clear above - and therefore must NOT
    // fire a copy. Before the fix this is where the false copy
    // would happen.
    let up_on_label_again = mouse(MouseEventKind::Up(MouseButton::Left), 50, 10);
    handle_mouse_with_terminal_size(&mut app, up_on_label_again, TEST_SIZE);
    assert!(
        app.toasts.is_empty(),
        "up on label without a fresh matching down must not fire a copy",
    );
}

// -- Work item list wheel-scroll / click-to-select tests --
//
// These exercise `handle_work_item_list_scroll` and
// `handle_work_item_row_click` indirectly via
// `handle_mouse_with_terminal_size`, with the left-panel body rect
// pre-populated on the `App` (the renderer normally sets it, but
// these tests bypass rendering so they can control the rect
// exactly).

/// Install a synthetic left-panel body rect, a populated display
/// list, and a wheel-scroll clamp. Returns the rect so tests can
/// compute hit coordinates. The body is at `(0, 0, 30, 20)` so
/// any column/row in that rect classifies as `WorkItemList`.
fn seed_work_item_list(app: &mut App, row_count: usize, max_item_offset: usize) -> UiRect {
    // Populate the display list with `row_count` unlinked-PR
    // entries, which are selectable without any backend setup.
    app.unlinked_prs.clear();
    for i in 0..row_count {
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: std::path::PathBuf::from(format!("/repo/{i}")),
            branch: format!("branch-{i}"),
            pr: crate::work_item::PrInfo {
                number: i as u64,
                title: format!("PR {i}"),
                state: crate::work_item::PrState::Open,
                is_draft: false,
                review_decision: crate::work_item::ReviewDecision::None,
                checks: crate::work_item::CheckStatus::None,
                mergeable: crate::work_item::MergeableState::Unknown,
                url: String::new(),
            },
        });
    }
    app.display_list = app
        .unlinked_prs
        .iter()
        .enumerate()
        .map(|(i, _)| DisplayEntry::UnlinkedItem(i))
        .collect();

    let rect = UiRect {
        x: 0,
        y: 0,
        width: 30,
        height: 20,
    };
    app.work_item_list_body.set(Some(rect));
    app.list_max_item_offset.set(max_item_offset);
    app.list_scroll_offset.set(0);
    rect
}

#[test]
fn wheel_down_advances_offset_by_3() {
    let mut app = App::new();
    seed_work_item_list(&mut app, 20, 15);
    let ev = mouse(MouseEventKind::ScrollDown, 10, 5);
    assert!(handle_mouse_with_terminal_size(&mut app, ev, TEST_SIZE));
    assert_eq!(app.list_scroll_offset.get(), 3);
}

#[test]
fn wheel_up_retreats_offset_by_3() {
    let mut app = App::new();
    seed_work_item_list(&mut app, 20, 15);
    app.list_scroll_offset.set(10);
    let ev = mouse(MouseEventKind::ScrollUp, 10, 5);
    assert!(handle_mouse_with_terminal_size(&mut app, ev, TEST_SIZE));
    assert_eq!(app.list_scroll_offset.get(), 7);
}

#[test]
fn wheel_clamps_at_top() {
    let mut app = App::new();
    seed_work_item_list(&mut app, 20, 15);
    app.list_scroll_offset.set(1);
    let ev = mouse(MouseEventKind::ScrollUp, 10, 5);
    assert!(handle_mouse_with_terminal_size(&mut app, ev, TEST_SIZE));
    assert_eq!(app.list_scroll_offset.get(), 0);
    // Another scroll-up at offset 0 is a no-op (returns false).
    let ev = mouse(MouseEventKind::ScrollUp, 10, 5);
    assert!(!handle_mouse_with_terminal_size(&mut app, ev, TEST_SIZE));
    assert_eq!(app.list_scroll_offset.get(), 0);
}

#[test]
fn wheel_clamps_at_bottom() {
    let mut app = App::new();
    seed_work_item_list(&mut app, 20, 15);
    app.list_scroll_offset.set(14);
    let ev = mouse(MouseEventKind::ScrollDown, 10, 5);
    assert!(handle_mouse_with_terminal_size(&mut app, ev, TEST_SIZE));
    // 14 + 3 = 17, clamped to max_item_offset = 15.
    assert_eq!(app.list_scroll_offset.get(), 15);
    // Another scroll-down at max is a no-op.
    let ev = mouse(MouseEventKind::ScrollDown, 10, 5);
    assert!(!handle_mouse_with_terminal_size(&mut app, ev, TEST_SIZE));
    assert_eq!(app.list_scroll_offset.get(), 15);
}

#[test]
fn wheel_does_not_move_selection_or_arm_recenter() {
    let mut app = App::new();
    seed_work_item_list(&mut app, 20, 15);
    app.selected_item = Some(0);
    app.recenter_viewport_on_selection.set(false);
    let ev = mouse(MouseEventKind::ScrollDown, 10, 5);
    handle_mouse_with_terminal_size(&mut app, ev, TEST_SIZE);
    assert_eq!(app.selected_item, Some(0), "wheel must not move selection");
    assert!(
        !app.recenter_viewport_on_selection.get(),
        "wheel must not arm the recenter flag - that is keyboard-only",
    );
}

#[test]
fn left_click_on_row_selects_it() {
    let mut app = App::new();
    let rect = seed_work_item_list(&mut app, 20, 15);
    // Register one row target at y=5 covering the full body width.
    {
        let mut reg = app.click_registry.borrow_mut();
        reg.push_work_item_row(
            UiRect {
                x: rect.x,
                y: rect.y + 5,
                width: rect.width,
                height: 1,
            },
            3,
        );
    }
    app.selected_item = Some(0);
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 10, 5);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 10, 5);
    assert!(handle_mouse_with_terminal_size(&mut app, down, TEST_SIZE));
    // Selection does not change on SelectDown - only on SelectUp.
    assert_eq!(app.selected_item, Some(0));
    assert!(handle_mouse_with_terminal_size(&mut app, up, TEST_SIZE));
    assert_eq!(app.selected_item, Some(3));
    assert!(
        app.recenter_viewport_on_selection.get(),
        "click-to-select must arm recenter so the next frame centers \
         on the clicked row",
    );
    assert!(matches!(app.right_panel_tab, RightPanelTab::ClaudeCode));
}

#[test]
fn left_click_outside_list_does_not_select() {
    let mut app = App::new();
    seed_work_item_list(&mut app, 20, 15);
    app.selected_item = Some(0);
    // Coordinate outside the list body (x=100 is beyond width=30)
    // and also outside the right-panel classification for this
    // terminal size. No registry entry at this location.
    let down = mouse(MouseEventKind::Down(MouseButton::Left), 100, 10);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 100, 10);
    handle_mouse_with_terminal_size(&mut app, down, TEST_SIZE);
    handle_mouse_with_terminal_size(&mut app, up, TEST_SIZE);
    assert_eq!(app.selected_item, Some(0), "selection must not change");
}

/// Drawer-gate regression: when the global drawer is open and a
/// click falls over a registered `WorkItemRow` target (because
/// the drawer visually covers part of the list), the row must NOT
/// be selected. The click instead flows through the geometric
/// classifier to the drawer's own handler. This is the asymmetry
/// vs chrome copies: a fire-and-forget clipboard copy is fine to
/// resolve through the drawer, but silently changing
/// `selected_item` + `right_panel_tab` + viewport behind an open
/// modal surprises the user.
#[test]
fn drawer_open_suppresses_work_item_row_click() {
    let mut app = App::new();
    let rect = seed_work_item_list(&mut app, 20, 15);
    {
        let mut reg = app.click_registry.borrow_mut();
        reg.push_work_item_row(
            UiRect {
                x: rect.x,
                y: rect.y + 5,
                width: rect.width,
                height: 1,
            },
            3,
        );
    }
    app.global_drawer_open = true;
    app.selected_item = Some(0);
    let initial_tab = app.right_panel_tab;
    app.recenter_viewport_on_selection.set(false);

    let down = mouse(MouseEventKind::Down(MouseButton::Left), 10, 5);
    let up = mouse(MouseEventKind::Up(MouseButton::Left), 10, 5);
    handle_mouse_with_terminal_size(&mut app, down, TEST_SIZE);
    handle_mouse_with_terminal_size(&mut app, up, TEST_SIZE);

    assert_eq!(
        app.selected_item,
        Some(0),
        "row click behind open drawer must not mutate selection",
    );
    assert!(
        !app.recenter_viewport_on_selection.get(),
        "row click behind open drawer must not arm recenter",
    );
    assert!(
        app.right_panel_tab == initial_tab,
        "row click behind open drawer must not change right_panel_tab",
    );
}

#[test]
fn mouse_target_classifies_work_item_list() {
    let mut app = App::new();
    let rect = seed_work_item_list(&mut app, 20, 15);
    let target = mouse_target_with_size(&app, rect.x + 5, rect.y + 5, (TEST_COLS, TEST_ROWS));
    assert!(
        matches!(target, MouseTarget::WorkItemList),
        "point inside body rect must classify as WorkItemList, got {target:?}",
    );
}
