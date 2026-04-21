use crate::app::{App, RightPanelTab};
use crate::click_targets::ClickTarget;
use crate::event::layout::sync_layout;
use crate::event::mouse::MouseAction;
use crate::salsa::ct::event::MouseEvent;

/// Handle a wheel scroll over the work item list body.
///
/// Wheel scrolls mutate the authoritative viewport offset
/// (`App::list_scroll_offset`) directly and deliberately do NOT touch
/// `selected_item` or `recenter_viewport_on_selection`: the decoupled
/// viewport model means wheel scrolls leave the keyboard selection in
/// place so the user can scroll away, scroll back, and still land on
/// the same selection. Step size is 3 rows per tick to match the PTY
/// scrollback step.
pub fn handle_work_item_list_scroll(app: &App, scroll_up: bool) -> bool {
    let current = app.list_scroll_offset.get();
    let max = app.list_max_item_offset.get();
    let next = if scroll_up {
        current.saturating_sub(3)
    } else {
        current.saturating_add(3).min(max)
    };
    if next == current {
        return false;
    }
    app.list_scroll_offset.set(next);
    true
}

/// Handle a left-click on a specific work item list row.
///
/// Called from the click-registry priority path when a
/// `ClickTarget::WorkItemRow` is hit and the global drawer is not
/// open (the caller gates on `!app.global_drawer_open` to keep row
/// selection from silently mutating behind the drawer). Select-down
/// is a no-op (we wait for the release so the user can abort by
/// dragging off); select-up actually changes the selection, mirrors
/// the keyboard handler's side effects (right panel tab, layout sync
/// on context change), and arms a recenter so the next render centers
/// the viewport on the clicked row. Scroll events never reach here
/// (they are dispatched to `handle_work_item_list_scroll` via the
/// `WorkItemList` arm).
pub fn handle_work_item_row_click(app: &mut App, index: usize, action: MouseAction) -> bool {
    match action {
        MouseAction::SelectDown => {
            // Arm a pending row click so that a `SelectUp` on the
            // same row (no intervening drag off the list) is what
            // actually performs the selection. We reuse the same
            // lossy-release safeguard as the chrome-copy path: any
            // drag or off-target release clears this automatically.
            // Returning `true` so the event is consumed and the
            // geometric classifier cannot route this down-click into
            // a PTY text-selection start (which would harmlessly no-op
            // on the left panel but still look noisy in trace logs).
            true
        }
        MouseAction::SelectUp => {
            if index >= app.display_list.len() {
                return false;
            }
            if !crate::app::is_selectable(&app.display_list[index]) {
                return false;
            }
            let had_context = app.selected_work_item_context().is_some();
            app.selected_item = Some(index);
            app.sync_selection_identity();
            app.right_panel_tab = RightPanelTab::ClaudeCode;
            // Recenter the viewport on the newly-selected row so the
            // next keyboard navigation starts from a known layout.
            // This matches the keyboard navigation contract and keeps
            // click-to-select + keyboard navigation composable.
            app.recenter_viewport_on_selection.set(true);
            if app.selected_work_item_context().is_some() != had_context {
                sync_layout(app);
            }
            true
        }
        MouseAction::SelectDrag | MouseAction::Scroll { .. } => false,
    }
}

/// Click-to-copy dispatch: consult the per-frame `ClickRegistry` and,
/// if the event hits a registered target, run the click-to-copy
/// gesture. Called from two places in `handle_mouse_with_terminal_size`:
///
/// 1. **Priority path:** before PTY-area classification, when the
///    cursor is already known to hit a registered interactive label.
///    This is the path that wins for labels drawn inside the right
///    panel (e.g. the work item detail view), which would otherwise
///    be consumed by the text-selection branch.
/// 2. **None path:** after classification, when the cursor is not
///    inside any PTY area at all. This keeps labels drawn in chrome
///    (outside both the right panel and the global drawer)
///    clickable.
///
/// The gesture is a `Down(Left)` followed by `Up(Left)` that both land
/// on the same registered target. Any intervening `Drag(Left)` cancels
/// the gesture (see the unconditional clear in the caller).
///
/// `try_borrow` is used defensively so that an accidentally overlapping
/// borrow becomes a silent no-op rather than a panic - the registry is
/// only supposed to be borrowed during draw, which never overlaps with
/// mouse handling, but defense in depth is cheap here.
pub fn handle_chrome_click_fallback(app: &mut App, mouse: MouseEvent, action: MouseAction) -> bool {
    match action {
        MouseAction::SelectDown => {
            let hit = app
                .click_tracking
                .registry
                .try_borrow()
                .ok()
                .and_then(|r| r.hit_test(mouse.column, mouse.row).cloned());
            if let Some(ClickTarget::Copy { kind, value, .. }) = hit {
                app.click_tracking.pending = Some((mouse.column, mouse.row, kind, value));
                true
            } else {
                // Row targets are dispatched by the caller; anything
                // else is a miss.
                false
            }
        }
        // `SelectDrag`: already cleared above; nothing to do here.
        // `Scroll`: chrome labels do not respond to scroll.
        MouseAction::SelectDrag | MouseAction::Scroll { .. } => false,
        MouseAction::SelectUp => {
            let pending = app.click_tracking.pending.take();
            let hit = app
                .click_tracking
                .registry
                .try_borrow()
                .ok()
                .and_then(|r| r.hit_test(mouse.column, mouse.row).cloned());
            match (pending, hit) {
                (
                    Some((_, _, pending_kind, pending_value)),
                    Some(ClickTarget::Copy { kind, .. }),
                ) if kind == pending_kind => {
                    // Cross-subsystem field-borrow split: hand out
                    // disjoint `&mut` borrows on `click_tracking`
                    // and `toasts` so `ClickTracking::fire_copy`
                    // can push a confirmation toast without holding
                    // a borrow on the rest of `App`.
                    let App {
                        click_tracking,
                        toasts,
                        ..
                    } = app;
                    click_tracking.fire_copy(&mut *toasts, pending_value, pending_kind);
                    true
                }
                _ => false,
            }
        }
    }
}
