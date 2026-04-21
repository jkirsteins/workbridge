pub mod clicks;
pub mod selection;

use crate::app::App;
use crate::click_targets::ClickTarget;
use crate::event::mouse::clicks::{
    handle_chrome_click_fallback, handle_work_item_list_scroll, handle_work_item_row_click,
};
use crate::event::mouse::selection::{
    active_session_entry_mut_for_tab, child_wants_mouse_global, child_wants_mouse_right,
    handle_scroll_global, handle_scroll_right, handle_selection_up_global,
    handle_selection_up_right,
};
use crate::event::util::any_modal_visible;
use crate::layout;
use crate::salsa::ct::event::{MouseButton, MouseEvent, MouseEventKind};
use crate::work_item::SelectionState;

// -- Mouse scroll handling ---------------------------------------------------

/// Which PTY area (if any) the mouse cursor is over.
#[derive(Debug)]
pub enum MouseTarget {
    /// Mouse is over the global assistant drawer's inner area.
    GlobalDrawer { local_col: u16, local_row: u16 },
    /// Mouse is over the right panel's inner area.
    RightPanel { local_col: u16, local_row: u16 },
    /// Mouse is over the left-panel work item list's body area.
    /// Row selection is routed through the `ClickTarget::WorkItemRow`
    /// click-target registry (each visible row pushes a target each
    /// frame), so only the "is in the list body" signal is needed to
    /// dispatch wheel scrolls - there is no row payload on the
    /// variant itself.
    WorkItemList,
    /// Mouse is not over any PTY area.
    None,
}

/// Determine which PTY area contains the given terminal-absolute
/// coordinates, given an explicit `(cols, rows)` terminal size.
///
/// Checks the global drawer first (since it overlays everything), then
/// the right panel. Returns `MouseTarget::None` if outside both areas.
///
/// Callers on the UI-event path pass `crossterm::terminal::size()`;
/// unit tests pass an explicit size so the geometric classifier
/// actually runs under `cargo test` (where `terminal::size()` returns
/// `Err` and would otherwise collapse classification to
/// `MouseTarget::None`, silently bypassing the PTY-area dispatch and
/// masking regressions).
pub fn mouse_target_with_size(
    app: &App,
    column: u16,
    row: u16,
    (cols, rows): (u16, u16),
) -> MouseTarget {
    const HEADER_ROWS: u16 = 1;

    // Check global drawer first (it overlays everything when open).
    if app.global_drawer_open {
        let dl = layout::compute_drawer(cols, rows);
        // Drawer origin matches the render code in ui.rs:
        // drawer_x = 2, drawer_y = rows - drawer_height
        let drawer_x = 2u16;
        let drawer_y = rows.saturating_sub(dl.drawer_height);

        // Inner area is 1 cell inside the border on all sides.
        let inner_x = drawer_x + 1;
        let inner_y = drawer_y + 1;
        let inner_right = drawer_x + dl.drawer_width; // exclusive
        let inner_bottom = drawer_y + dl.drawer_height; // exclusive (border row)

        if column >= inner_x
            && column < inner_right
            && row >= inner_y
            && row < inner_bottom.saturating_sub(1)
        {
            return MouseTarget::GlobalDrawer {
                local_col: column - inner_x,
                local_row: row - inner_y,
            };
        }

        // The drawer is open but the mouse is outside its inner area.
        // Do not fall through to the right panel hit-test since the
        // background is dimmed and should not receive scroll events.
        return MouseTarget::None;
    }

    // Compute right panel geometry. Must mirror draw_to_buffer in ui.rs:
    // the full area is split into a 1-row view-mode header + main_area +
    // optional bottom bars, so layout::compute is called with main_area's
    // height (rows - header - bottom_bars), not the raw terminal height.
    let bottom_rows = u16::from(app.has_visible_status_bar())
        + u16::from(app.selected_work_item_context().is_some());
    let main_area_height = rows.saturating_sub(HEADER_ROWS).saturating_sub(bottom_rows);
    let pl = layout::compute(cols, main_area_height, 0);

    // Right panel inner area: past the left panel + its left border column,
    // and past the view-mode header + the right panel's top border row.
    let inner_x = pl.left_width + 1;
    let inner_y = HEADER_ROWS + 1;

    if column >= inner_x
        && column < inner_x + pl.pane_cols
        && row >= inner_y
        && row < inner_y + pl.pane_rows
    {
        return MouseTarget::RightPanel {
            local_col: column - inner_x,
            local_row: row - inner_y,
        };
    }

    // Left-panel work item list. The body rect is stored by the
    // renderer in absolute frame coordinates on every frame and
    // cleared once the list is not drawn (e.g. behind a modal
    // overlay). A hit here dispatches wheel scrolls to the list's
    // `list_scroll_offset`; left-clicks are handled separately via
    // the `ClickTarget::WorkItemRow` entries in the click registry
    // and take priority over this classification.
    if let Some(rect) = app.work_item_list_body.get()
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
    {
        return MouseTarget::WorkItemList;
    }

    MouseTarget::None
}

/// Handle a mouse event. Processes scroll events (ScrollUp/ScrollDown) and
/// left-button click/drag/release for text selection.
///
/// Scroll events are hit-tested against the global drawer and right panel
/// areas. If the mouse is over a PTY area, the scroll is encoded and
/// forwarded to the corresponding PTY session.
///
/// Left-button events drive text selection: click starts a selection, drag
/// updates it, and release finalizes it and copies the selected text to the
/// system clipboard. Selection is only intercepted when the child process
/// has NOT enabled mouse reporting (or when in local scrollback mode).
///
/// Returns `true` if the event modified app state, `false` otherwise.
/// Abstract categorization of a mouse event. Used by `handle_mouse`
/// and its helpers (`handle_chrome_click_fallback`) to share the
/// classification logic without having to inspect
/// `MouseEventKind` in multiple places.
pub enum MouseAction {
    Scroll { up: bool },
    SelectDown,
    SelectDrag,
    SelectUp,
}

pub fn handle_mouse(app: &mut App, mouse: MouseEvent) -> bool {
    let terminal_size = ratatui_crossterm::crossterm::terminal::size().ok();
    handle_mouse_with_terminal_size(app, mouse, terminal_size)
}

/// Test seam for `handle_mouse`: accepts an explicit terminal size so
/// unit tests can force `mouse_target` to classify a click as
/// `GlobalDrawer` / `RightPanel` and verify the dispatch actually runs
/// in that arm. Under `cargo test` `crossterm::terminal::size()`
/// returns `Err`, so the production path would otherwise collapse to
/// `MouseTarget::None` and silently skip the PTY-area branches.
pub fn handle_mouse_with_terminal_size(
    app: &mut App,
    mouse: MouseEvent,
    terminal_size: Option<(u16, u16)>,
) -> bool {
    let action = match mouse.kind {
        MouseEventKind::ScrollUp => MouseAction::Scroll { up: true },
        MouseEventKind::ScrollDown => MouseAction::Scroll { up: false },
        MouseEventKind::Down(MouseButton::Left) => MouseAction::SelectDown,
        MouseEventKind::Drag(MouseButton::Left) => MouseAction::SelectDrag,
        MouseEventKind::Up(MouseButton::Left) => MouseAction::SelectUp,
        _ => return false,
    };

    // Ignore during shutdown or when overlays are visible.
    if app.shutting_down || any_modal_visible(app) {
        return false;
    }

    // Any drag cancels a click-to-copy gesture in progress. Must
    // happen before target dispatch because a drag over a PTY pane
    // still invalidates a pending chrome click that started elsewhere.
    if matches!(action, MouseAction::SelectDrag) {
        app.click_tracking.pending = None;
    }

    // Interactive labels (click-to-copy) and work item row clicks both
    // flow through the per-frame `ClickRegistry`. The registry is
    // cleared at the top of every frame and is populated by the
    // renderer with two kinds of targets:
    //
    // - `ClickTarget::WorkItemRow { index }` - one per visible row in
    //   the left-panel work item list. A left-click release selects
    //   the row.
    // - `ClickTarget::Copy { kind, value }` - one per interactive
    //   chrome label. A down+up pair on the same target copies the
    //   value to the clipboard.
    //
    // Both take priority over the geometric PTY-area classification
    // because the classification would otherwise route right-panel
    // labels into the text-selection branch.
    //
    // **Drawer gate for row clicks only.** Chrome copies are
    // intentionally allowed through the global drawer (see the
    // `chrome_click_inside_global_drawer_still_fires` test): a copy
    // is fire-and-forget and the user might reasonably want to copy
    // a value drawn behind the drawer. Row selection, in contrast,
    // has side effects (selected_item, right_panel_tab,
    // recenter_viewport_on_selection) that the user cannot see while
    // the drawer is open and would only discover after closing it,
    // at which point the list has silently scrolled and a different
    // item is highlighted. When the drawer is open we therefore
    // short-circuit `WorkItemRow` hits: falling through lets
    // `mouse_target_with_size` return `GlobalDrawer` / `None` as
    // appropriate, and the drawer's own handler deals with the
    // click.
    if matches!(action, MouseAction::SelectDown | MouseAction::SelectUp) {
        let dispatch = app
            .click_tracking
            .registry
            .try_borrow()
            .ok()
            .and_then(|r| r.hit_test(mouse.column, mouse.row).cloned());
        if let Some(target) = dispatch {
            match target {
                ClickTarget::WorkItemRow { index, .. } => {
                    if !app.global_drawer_open {
                        return handle_work_item_row_click(app, index, action);
                    }
                    // Fall through: drawer is open, let the geometric
                    // classifier route the click to the drawer.
                }
                ClickTarget::Copy { .. } => {
                    return handle_chrome_click_fallback(app, mouse, action);
                }
            }
        }
    }

    // A `SelectUp` that did not hit any registered target ends the
    // click-to-copy gesture from the registry's point of view: the
    // user released outside every interactive label, so any pending
    // click armed by an earlier `SelectDown` is abandoned. Without
    // this clear, a stale `click_tracking.pending` could linger and
    // later fire a false copy on an unrelated `SelectUp` that
    // happens to hit a same-kind label (for example on terminals
    // that coalesce intervening `Drag` events, or over SSH sessions
    // that drop them, or in X10/Default mouse modes that only
    // report `Down`/`Up`). The drag-cancel clear above catches the
    // well-behaved case; this catches the lossy case. It is safe
    // on all paths because (a) the priority check above already
    // `take()`s `click_tracking.pending` on a matching up and returns
    // before reaching here, and (b) the `MouseTarget::None` fallback
    // below also `take()`s it, so clearing here cannot destroy any
    // state that another branch still needs.
    if matches!(action, MouseAction::SelectUp) {
        app.click_tracking.pending = None;
    }

    let target = terminal_size.map_or(MouseTarget::None, |size| {
        mouse_target_with_size(app, mouse.column, mouse.row, size)
    });

    match target {
        MouseTarget::GlobalDrawer {
            local_col,
            local_row,
        } => match action {
            MouseAction::Scroll { up: scroll_up } => {
                handle_scroll_global(app, scroll_up, local_col, local_row)
            }
            MouseAction::SelectDown => {
                // Check if child wants mouse events and we are NOT in scrollback.
                if child_wants_mouse_global(app) {
                    return false;
                }
                if let Some(entry) = app.global_session.as_mut() {
                    entry.selection = Some(SelectionState {
                        anchor: (local_row, local_col),
                        current: (local_row, local_col),
                        dragging: true,
                    });
                }
                true
            }
            MouseAction::SelectDrag => {
                if let Some(entry) = app.global_session.as_mut()
                    && entry.selection.as_ref().is_some_and(|s| s.dragging)
                {
                    if let Some(sel) = entry.selection.as_mut() {
                        sel.current = (local_row, local_col);
                    }
                    return true;
                }
                false
            }
            MouseAction::SelectUp => handle_selection_up_global(app, local_row, local_col),
        },
        MouseTarget::RightPanel {
            local_col,
            local_row,
        } => match action {
            MouseAction::Scroll { up: scroll_up } => {
                handle_scroll_right(app, scroll_up, local_col, local_row)
            }
            MouseAction::SelectDown => {
                // Check if child wants mouse events and we are NOT in scrollback.
                if child_wants_mouse_right(app) {
                    return false;
                }
                if let Some(entry) = active_session_entry_mut_for_tab(app) {
                    entry.selection = Some(SelectionState {
                        anchor: (local_row, local_col),
                        current: (local_row, local_col),
                        dragging: true,
                    });
                }
                true
            }
            MouseAction::SelectDrag => {
                if let Some(entry) = active_session_entry_mut_for_tab(app)
                    && entry.selection.as_ref().is_some_and(|s| s.dragging)
                {
                    if let Some(sel) = entry.selection.as_mut() {
                        sel.current = (local_row, local_col);
                    }
                    return true;
                }
                false
            }
            MouseAction::SelectUp => handle_selection_up_right(app, local_row, local_col),
        },
        MouseTarget::WorkItemList => match action {
            MouseAction::Scroll { up: scroll_up } => handle_work_item_list_scroll(app, scroll_up),
            // `SelectDown` / `SelectUp` on the list body only matters
            // if a row click hit-tested in the priority path above.
            // If we reach here with a select action, the click landed
            // between rows (e.g. on a group header) and should be a
            // no-op rather than bleed into the right-panel selection
            // branch.
            MouseAction::SelectDown | MouseAction::SelectUp | MouseAction::SelectDrag => false,
        },
        MouseTarget::None => handle_chrome_click_fallback(app, mouse, action),
    }
}

#[cfg(test)]
mod tests;
