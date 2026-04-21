//! Work item list rendering: the left-panel list of work items, review
//! requests, and unlinked PRs, plus the sticky group header and the
//! scrollbar / offscreen-selection marker overlays.
pub mod format_items;

use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Margin, Rect};
use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span, Text};
use ratatui_core::widgets::{StatefulWidget, Widget};
use ratatui_widgets::block::Block;
use ratatui_widgets::borders::Borders;
use ratatui_widgets::paragraph::Paragraph;

use crate::app::{App, DisplayEntry, FocusPanel, GroupHeaderKind, is_selectable};
use crate::theme::Theme;
use crate::work_item::WorkItemStatus;

use ratatui_widgets::list::{List, ListItem, ListState};
use ratatui_widgets::scrollbar::{Scrollbar, ScrollbarOrientation, ScrollbarState};

pub use self::format_items::*;

pub fn find_current_group_header(display_list: &[DisplayEntry], offset: usize) -> Option<usize> {
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

/// Predict the `ListState::offset()` that ratatui's `List` widget will choose
/// when rendered with the given per-item row heights, previous offset,
/// selection, and available body height.
///
/// This mirrors `ratatui_widgets::list::List::get_items_bounds` for the
/// default `scroll_padding = 0` case, and also mirrors the `ListState::select`
/// side effect that resets `offset` to 0 when the selection is cleared
/// (see `ratatui_widgets::list::state::ListState::select`).
///
/// **No longer used in the render path.** With the decoupled-viewport
/// model (`App::list_scroll_offset` is authoritative, not derived), the
/// renderer no longer needs a ratatui predictor - the offset lives on
/// `App` and is mutated only by mouse wheel events and the
/// recenter-on-selection pass. The predictor is retained under
/// `#[cfg(test)]` so the parallel-render tests in `mod sticky_header_tests`
/// still document what ratatui's own math does, which is useful for
/// sanity-checking the new `recenter_offset` helper against the old
/// auto-scroll behavior.
#[cfg(test)]
pub fn predict_list_offset(
    item_heights: &[usize],
    prev_offset: usize,
    selected: Option<usize>,
    max_height: usize,
) -> usize {
    if item_heights.is_empty() {
        return 0;
    }

    let last_valid_index = item_heights.len() - 1;
    // Mirror `ListState::select(None)`'s side effect: clearing the
    // selection also resets the offset to 0. This matches what ratatui
    // will actually render when the production call site invokes
    // `state.select(None)` after `with_offset`.
    let effective_prev_offset = match selected {
        Some(_) => prev_offset.min(last_valid_index),
        None => 0,
    };
    if max_height == 0 {
        return effective_prev_offset;
    }
    let mut first_visible_index = effective_prev_offset;
    let mut last_visible_index = first_visible_index;
    let mut height_from_offset: usize = 0;

    // Walk forward from the current offset, summing heights until the next
    // item would overflow the viewport. After this loop `last_visible_index`
    // is the exclusive end of the visible range (i.e. one past the last
    // fully-visible item).
    for h in item_heights.iter().skip(first_visible_index) {
        if height_from_offset + h > max_height {
            break;
        }
        height_from_offset += h;
        last_visible_index += 1;
    }

    // With `scroll_padding = 0` the index we must keep on screen is just the
    // selected item (falling back to the offset when nothing is selected).
    let index_to_display = selected.map_or(first_visible_index, |s| s.min(last_valid_index));

    // If the selected item is past the current viewport, scroll down: add
    // items to the tail and drop items from the head until the selected
    // index is visible.
    while index_to_display >= last_visible_index {
        height_from_offset = height_from_offset.saturating_add(item_heights[last_visible_index]);
        last_visible_index += 1;
        while height_from_offset > max_height && first_visible_index < last_visible_index {
            height_from_offset =
                height_from_offset.saturating_sub(item_heights[first_visible_index]);
            first_visible_index += 1;
        }
    }

    // If the selected item is before the current viewport, scroll up: add
    // items to the head and drop items from the tail.
    while index_to_display < first_visible_index {
        first_visible_index -= 1;
        height_from_offset = height_from_offset.saturating_add(item_heights[first_visible_index]);
        while height_from_offset > max_height && last_visible_index > first_visible_index + 1 {
            last_visible_index -= 1;
            height_from_offset =
                height_from_offset.saturating_sub(item_heights[last_visible_index]);
        }
    }

    first_visible_index
}

pub fn draw_work_item_list(buf: &mut Buffer, app: &App, theme: &Theme, area: Rect) {
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
    let title = app.board_drill_stage.as_ref().map_or_else(
        || format!(" Work Items ({}) ", app.work_items.len()),
        |stage| {
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
        },
    );

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

    // Build list items. When a row is the selected row, the row's
    // background is painted with `style_tab_highlight_bg` directly on
    // the ListItem so the `List` widget itself no longer owns the
    // selection highlight. `ListState::select` is deliberately NOT
    // called below - decoupling the viewport from the selection means
    // the renderer must not let ratatui's `get_items_bounds` force an
    // auto-scroll. The highlight is a styling concern, the viewport a
    // state concern, and the two must stay independent for the wheel
    // scroll to work without snapping back.
    let items: Vec<ListItem<'_>> = app
        .display_list
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_selected = app.selected_item == Some(i);
            let item = match entry {
                DisplayEntry::GroupHeader { label, count, kind } => {
                    let text = format!("{label} ({count})");
                    let style = match kind {
                        GroupHeaderKind::Blocked => theme.style_group_header_blocked(),
                        GroupHeaderKind::Normal => theme.style_group_header(),
                    };
                    ListItem::new(Line::from(vec![Span::raw("  "), Span::styled(text, style)]))
                }
                DisplayEntry::UnlinkedItem(idx) => {
                    format_unlinked_item(app, *idx, inner_width, theme, is_selected)
                }
                DisplayEntry::ReviewRequestItem(idx) => {
                    format_review_request_item(app, *idx, inner_width, theme, is_selected)
                }
                DisplayEntry::WorkItemEntry(idx) => {
                    format_work_item_entry(app, *idx, inner_width, theme, is_selected)
                }
            };
            if is_selected {
                item.style(theme.style_tab_highlight_bg())
            } else {
                item
            }
        })
        .collect();

    // Pre-compute per-item row heights for scrollbar calculations.
    let item_heights: Vec<usize> = items.iter().map(ListItem::height).collect();
    let total_rows: usize = item_heights.iter().sum();

    // Draw the block (borders + title) directly into `area` so we can
    // split the inner area ourselves and hand only a sub-rect to the
    // `List` widget. This lets us reserve a dedicated 1-row slot at the
    // top of the inner area for the sticky group header, guaranteeing
    // the selected work item is never painted over by the sticky row.
    Widget::render(block, area, buf);
    let inner = area.inner(Margin::new(1, 1));

    // The viewport offset (`list_scroll_offset`) is authoritative here:
    // it is mutated only by the wheel-scroll handler, by the recenter
    // pass below (triggered by keyboard selection changes), and by the
    // clamp on list shrink. The renderer reads it directly; no
    // predictor is consulted for the body offset.
    let selected = app.selected_item;
    let drill_down = app.board_drill_stage.is_some();

    // Resolve the pending recenter request first, against a tentative
    // body height that equals `inner.height` (no sticky slot reserved
    // yet). The sticky decision depends on the resolved offset, so we
    // cannot postpone the recenter past it - otherwise the first frame
    // after a keyboard navigation would show the old sticky decision
    // and flicker for one frame.
    let want_recenter = app.recenter_viewport_on_selection.take();
    let tentative_offset = if want_recenter {
        match selected {
            Some(idx) if idx < item_heights.len() => {
                recenter_offset(&item_heights, idx, inner.height as usize)
            }
            _ => app.list_scroll_offset.get(),
        }
    } else {
        app.list_scroll_offset.get()
    };
    let tentative_offset = tentative_offset.min(item_heights.len().saturating_sub(1));

    // Decide whether to reserve a sticky-header slot this frame. The
    // decision is made against the tentative offset, so a recenter
    // that lands deep in the list still reserves the slot on the same
    // frame.
    let (body_area, sticky_slot) = if drill_down || inner.height < 2 {
        (inner, None)
    } else {
        let sticky_would_fire = find_current_group_header(&app.display_list, tentative_offset)
            .is_some_and(|h| h < tentative_offset);
        if sticky_would_fire {
            let body_height = inner.height - 1;
            let slot = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 1,
            };
            let body = Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: body_height,
            };
            (body, Some(slot))
        } else {
            (inner, None)
        }
    };

    let body_height = body_area.height as usize;

    // Reconcile the tentative offset with the final body height. If
    // the sticky slot shrank the body, the recenter may need to shift
    // by one item; if the list shrank below the viewport, the offset
    // clamps to `max_item_offset`.
    let max_item_offset = compute_max_item_offset(&item_heights, body_height);
    let resolved_offset = if want_recenter {
        match selected {
            Some(idx) if idx < item_heights.len() => {
                recenter_offset(&item_heights, idx, body_height).min(max_item_offset)
            }
            _ => tentative_offset.min(max_item_offset),
        }
    } else {
        tentative_offset.min(max_item_offset)
    };
    app.list_scroll_offset.set(resolved_offset);
    app.list_max_item_offset.set(max_item_offset);
    app.work_item_list_body.set(Some(body_area));

    // Per-row click targets: push a `ClickTarget::WorkItemRow` for
    // each row that is at least partially visible so `handle_mouse`
    // can map a left-click at `(x, y)` back to a display-list index
    // without redoing the layout math. Offscreen rows are skipped -
    // the registry hit-test is a linear scan, so keeping it small
    // keeps the mouse path cheap.
    {
        let mut registry = app.click_registry.borrow_mut();
        let mut y = body_area.y;
        let end_y = body_area.y.saturating_add(body_area.height);
        for (i, h) in item_heights.iter().enumerate().skip(resolved_offset) {
            if y >= end_y {
                break;
            }
            let row_height = (*h as u16).min(end_y - y);
            if row_height == 0 {
                break;
            }
            // Only push selectable rows so clicks on group headers
            // (non-selectable) do not accidentally steal a row-click
            // dispatch. The mouse handler falls through to the
            // GlobalDrawer / RightPanel / WorkItemList arms otherwise.
            if is_selectable(&app.display_list[i]) {
                registry.push_work_item_row(
                    Rect {
                        x: body_area.x,
                        y,
                        width: body_area.width,
                        height: row_height,
                    },
                    i,
                );
            }
            y = y.saturating_add(row_height);
        }
    }

    let list = List::new(items);
    let mut state = ListState::default().with_offset(resolved_offset);

    StatefulWidget::render(list, body_area, buf, &mut state);

    // --- Sticky group header ---
    // Paint the reserved slot (if any) using the authoritative offset
    // (the one we wrote to `list_scroll_offset` above). Because the
    // slot was reserved structurally via `Layout`, the `List` body
    // never overlaps it.
    if !drill_down {
        let header_needed = find_current_group_header(&app.display_list, resolved_offset)
            .filter(|&h| h < resolved_offset);

        // The non-`(Some, Some)` cases are no-ops by design:
        // `(Some, None) | (None, Some)`: after the tentative-offset
        // reconciliation above, the sticky decision and the post-render
        // offset should agree. The only legitimate drift is a one-item
        // sticky decision flip caused by the recenter shrinking the body
        // by 1 row, in which case `header_needed` still matches
        // `sticky_slot`. Treat any remaining mismatch as a one-frame
        // visual glitch rather than a hard assertion - the next frame
        // will reconcile.
        // `(None, None)`: no header needed.
        if let (Some(slot), Some(header_idx)) = (sticky_slot, header_needed)
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
            // Fill the entire row with the sticky background so it
            // visually separates from the highlighted item below.
            let bg_style = Style::default().bg(theme.sticky_header_bg);
            let line = Line::from(vec![
                Span::styled("  ", bg_style),
                Span::styled(text, style),
            ]);
            Paragraph::new(line).style(bg_style).render(slot, buf);
        }
    }

    // Scrollbar - only when content overflows the list body. We use
    // `body_area.height` (not `inner.height`) so the scrollbar track
    // matches whichever area the `List` was rendered into.
    let max_row_offset = total_rows.saturating_sub(body_height);
    if total_rows > body_height || resolved_offset > 0 {
        // Convert the item-based offset to a row-based offset so the
        // scrollbar thumb position matches the actual viewport scroll.
        let row_offset: usize = item_heights.iter().take(resolved_offset).sum();

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
        // blank rows below the last item. `body_height` (not the block's
        // full inner height) is the correct viewport size because the
        // sticky slot reservation shrinks the area the `List` renders into.
        let content_length = max_row_offset + 1;
        let position = row_offset.min(max_row_offset);
        let mut scrollbar_state = ScrollbarState::new(content_length)
            .viewport_content_length(body_height)
            .position(position);

        let scrollbar_area = Rect {
            x: area.x,
            y: body_area.y,
            width: area.width,
            height: body_area.height,
        };
        StatefulWidget::render(scrollbar, scrollbar_area, buf, &mut scrollbar_state);
    }

    // Offscreen-selection marker: when the selected item is outside
    // the visible viewport, paint a single distinct-coloured cell in
    // the scrollbar column at the y-coordinate that corresponds to the
    // selection's position within the full list. This gives the user a
    // visual cue of where their keyboard selection sits relative to
    // the mouse-scrolled viewport. When the selection is visible, no
    // marker is painted - the normal thumb is enough.
    if let Some(idx) = selected
        && idx < item_heights.len()
        && body_area.width > 0
        && body_area.height > 0
    {
        let row_of_selection: usize = item_heights.iter().take(idx).sum();
        let sel_end = row_of_selection + item_heights[idx];
        let visible_start: usize = item_heights.iter().take(resolved_offset).sum();
        let visible_end = visible_start + body_height;
        let onscreen = row_of_selection < visible_end && sel_end > visible_start;
        if !onscreen && total_rows > 0 {
            // Map the selection's top row to a y within the body.
            // `max_row_offset == 0` means the whole list fits (no
            // scrolling possible) - guarded above via `!onscreen` +
            // `total_rows > 0` but keep the divisor non-zero.
            let denom = max_row_offset.max(1);
            let marker_row =
                (row_of_selection.saturating_mul(body_area.height as usize - 1)) / denom;
            let marker_row = marker_row.min(body_area.height as usize - 1);
            let marker_x = area.x + area.width - 1;
            let marker_y = body_area.y + marker_row as u16;
            if marker_x < buf.area.x + buf.area.width && marker_y < buf.area.y + buf.area.height {
                let cell = &mut buf[(marker_x, marker_y)];
                cell.set_symbol("\u{2588}");
                cell.set_style(theme.style_scrollbar_selection_marker());
            }
        }
    }
}

/// Compute the largest item-level viewport offset such that all
/// remaining items fit within `body_height` rows.
///
/// This is the upper bound the wheel-scroll handler uses to prevent
/// scrolling past the end of the list. Walks from the tail backward,
/// accumulating heights, and returns the first index where the sum
/// exceeds the viewport - that index (plus one) is the smallest offset
/// that still fits everything.
pub fn compute_max_item_offset(item_heights: &[usize], body_height: usize) -> usize {
    let total: usize = item_heights.iter().sum();
    if total <= body_height || item_heights.is_empty() {
        return 0;
    }
    let mut acc = 0usize;
    for (i, h) in item_heights.iter().enumerate().rev() {
        acc = acc.saturating_add(*h);
        if acc > body_height {
            return i + 1;
        }
    }
    0
}

/// Compute the item-level viewport offset that centers `selected` in a
/// `body_height`-row viewport, clamped to `[0, max_item_offset]`.
///
/// The offset is item-aligned (not row-aligned): the viewport starts
/// at an item boundary, never mid-item, so partial-item rows at the
/// top of the viewport are never rendered. The centering target is the
/// row directly above the selection that puts the selection's middle
/// row at the body's middle row, clamped to zero and to the tail.
pub fn recenter_offset(item_heights: &[usize], selected: usize, body_height: usize) -> usize {
    if item_heights.is_empty() || body_height == 0 || selected >= item_heights.len() {
        return 0;
    }
    let max_offset = compute_max_item_offset(item_heights, body_height);
    // Row at which the selected item begins in the full list.
    let sel_row: usize = item_heights.iter().take(selected).sum();
    let sel_height = item_heights[selected];
    // Target: selected item vertically centred. `sel_center_row` is
    // the absolute row of the selection's midpoint; to centre the
    // viewport on it we start at `sel_center_row - body_height/2`.
    let sel_center = sel_row + sel_height / 2;
    let target_row = sel_center.saturating_sub(body_height / 2);

    // Walk forward, adopting the largest item boundary `j` whose
    // cumulative row count is <= `target_row`. This keeps the offset
    // item-aligned.
    let mut cumulative = 0usize;
    let mut chosen = 0usize;
    for (j, h) in item_heights.iter().enumerate() {
        if cumulative <= target_row {
            chosen = j;
        } else {
            break;
        }
        cumulative = cumulative.saturating_add(*h);
    }
    chosen.min(max_offset)
}

#[cfg(test)]
mod sticky_header_tests;
