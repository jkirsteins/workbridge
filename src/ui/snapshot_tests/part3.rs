//! Snapshot tests: create-dialog layout, list scrollbar, board-view layout.
//! See `src/ui/snapshot_tests/mod.rs` for shared helpers.

use super::*;

/// Regression for the "typed characters hidden on first row of
/// description" bug.
///
/// Background: at 80x24 (the common single-pane tmux / terminal
/// size) the Create Work Item dialog requests ~28 rows, is clamped
/// to 24 by `centered_rect_fixed`, and ratatui's constraint solver
/// silently scales the `Length` constraints down proportionally.
/// The description textarea ends up with only 2-3 visible rows.
/// Once the user types enough to wrap past that viewport, rat-text
/// pins the cursor row into the tiny viewport and scrolls the
/// earliest characters off the top. What the user sees is a blank
/// textarea that only starts showing content after text wraps into
/// the new row - precisely the symptom in the bug report.
///
/// The test types >2 rows of text and asserts that the very first
/// characters of what was typed are still visible. Without the
/// layout fix the textarea is 2 rows and the first ~60 characters
/// scroll off the top; with the fix the textarea is at least
/// `DESC_TEXTAREA_HEIGHT` (6) rows tall when the terminal has
/// room, so the entire 100-char payload fits and the first `A` is
/// visible.
///
/// Using `insert_char` (not `set_text`) is important: `set_text`
/// resets rat-text's scroll state and masks the bug. Real user
/// keystrokes always route through `insert_char` / `insert_str`.
#[test]
fn create_dialog_first_keystroke_visible_on_small_terminal() {
    use crate::create_dialog::CreateDialogFocus;

    let mut app = App::new();
    // Six repos so the pre-fix layout squeezes the textarea hard
    // and matches the worst-case live configuration.
    let repos = vec![
        PathBuf::from("/repo/a"),
        PathBuf::from("/repo/b"),
        PathBuf::from("/repo/c"),
        PathBuf::from("/repo/d"),
        PathBuf::from("/repo/e"),
        PathBuf::from("/repo/f"),
    ];
    app.create_dialog
        .open(&repos, Some(&PathBuf::from("/repo/a")));
    app.create_dialog.focus_field = CreateDialogFocus::Description;
    // Prime the first render so rat-text records the viewport
    // size before the user starts typing, matching the live flow.
    let _ = render(&mut app, 80, 24);
    // Type 100 copies of 'A' one at a time, re-rendering after
    // each keystroke so rat-text's `scroll_to_cursor` latch fires
    // every stroke (the live event loop renders every tick).
    for _ in 0..100 {
        app.create_dialog.description_input.insert_char('A');
        let _ = render(&mut app, 80, 24);
    }

    let rendered = render(&mut app, 80, 24);

    // Count 'A's in the rendered output. With a 6-row textarea
    // and ~46 cols wide, 100 'A's fit in 3 visible rows with room
    // to spare, so all 100 should appear. Before the fix the
    // textarea is 2 rows, only the tail (~66 'A's) fits, and the
    // earliest characters are scrolled off.
    let visible_a_count: usize = rendered.matches('A').count();
    assert!(
        visible_a_count >= 100,
        "expected all 100 typed 'A's to be visible, but only \
             {visible_a_count} were in the rendered buffer. This is \
             the regression: the description textarea is squeezed \
             so small that rat-text scrolls the earliest characters \
             off the top.\n\nRendered output:\n{rendered}"
    );
}

/// Companion to `create_dialog_first_keystroke_visible_on_small_terminal`
/// at a terminal size where the dialog fits with plenty of room.
/// Guards against a future layout change that fixes the small
/// terminal case by regressing the tall-terminal case.

#[test]
fn create_dialog_first_keystroke_visible_on_tall_terminal() {
    use crate::create_dialog::CreateDialogFocus;

    let mut app = App::new();
    let repos = vec![
        PathBuf::from("/repo/a"),
        PathBuf::from("/repo/b"),
        PathBuf::from("/repo/c"),
    ];
    app.create_dialog
        .open(&repos, Some(&PathBuf::from("/repo/a")));
    app.create_dialog.focus_field = CreateDialogFocus::Description;
    let _ = render(&mut app, 80, 40);
    for _ in 0..100 {
        app.create_dialog.description_input.insert_char('A');
        let _ = render(&mut app, 80, 40);
    }

    let rendered = render(&mut app, 80, 40);

    let visible_a_count: usize = rendered.matches('A').count();
    assert!(
        visible_a_count >= 100,
        "expected all 100 typed 'A's to be visible at 80x40, but \
             only {visible_a_count} were in the rendered buffer:\n{rendered}"
    );
}

/// When the terminal is too short to fit the compact Create dialog
/// (normal dialog shrunk to a 2-row textarea), the dialog must
/// render the "terminal too small" fallback instead of silently
/// drawing an unresponsive-looking popup where typed characters
/// vanish. Asserts the fallback message is present in the render
/// AND that the normal Description label is absent (so focus
/// cannot land on an invisible textarea).

#[test]
fn create_dialog_fallback_when_terminal_too_small() {
    let mut app = App::new();
    // Six repos so the compact dialog needs its full row budget.
    // At 80x12 (12 rows tall), even the compact layout cannot fit.
    let repos = vec![
        PathBuf::from("/repo/a"),
        PathBuf::from("/repo/b"),
        PathBuf::from("/repo/c"),
        PathBuf::from("/repo/d"),
        PathBuf::from("/repo/e"),
        PathBuf::from("/repo/f"),
    ];
    app.create_dialog
        .open(&repos, Some(&PathBuf::from("/repo/a")));

    let rendered = render(&mut app, 80, 12);

    assert!(
        rendered.contains("Terminal too small"),
        "expected the 'Terminal too small' fallback at 80x12:\n{rendered}"
    );
    assert!(
        !rendered.contains("Description (optional):"),
        "fallback dialog must NOT render the Description label (would \
             imply focus could land on an invisible textarea):\n{rendered}"
    );
    assert!(
        !rendered.contains("Repos:"),
        "fallback dialog must NOT render the Repos section:\n{rendered}"
    );
}

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
            draw_to_buffer(frame.area(), frame.buffer_mut(), app, &theme);
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
    assert_eq!(cyan, 0, "onscreen selection must not paint the Cyan marker");
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
    app.shell.status_message = Some("Item moved to Planning".to_string());
    insta::assert_snapshot!(render(&mut app, 80, 24));
}
