use super::find_current_group_header;
use crate::app::{DisplayEntry, GroupHeaderKind};

fn make_display_list() -> Vec<DisplayEntry> {
    vec![
        DisplayEntry::GroupHeader {
            label: "ACTIVE (repo)".into(),
            count: 2,
            kind: GroupHeaderKind::Normal,
        },
        DisplayEntry::WorkItemEntry(0),
        DisplayEntry::WorkItemEntry(1),
        DisplayEntry::GroupHeader {
            label: "BACKLOGGED (repo)".into(),
            count: 1,
            kind: GroupHeaderKind::Normal,
        },
        DisplayEntry::WorkItemEntry(2),
    ]
}

#[test]
fn header_at_offset_zero() {
    let list = make_display_list();
    // Offset 0 is the ACTIVE header itself.
    assert_eq!(find_current_group_header(&list, 0), Some(0));
}

#[test]
fn header_for_first_group_item() {
    let list = make_display_list();
    // Offset 1 is the first item under ACTIVE - header is at 0.
    assert_eq!(find_current_group_header(&list, 1), Some(0));
}

#[test]
fn header_for_second_group_item() {
    let list = make_display_list();
    // Offset 2 is the second item under ACTIVE - header still at 0.
    assert_eq!(find_current_group_header(&list, 2), Some(0));
}

#[test]
fn header_switches_at_second_group() {
    let list = make_display_list();
    // Offset 3 is the BACKLOGGED header - returns itself.
    assert_eq!(find_current_group_header(&list, 3), Some(3));
}

#[test]
fn header_for_item_in_second_group() {
    let list = make_display_list();
    // Offset 4 is the item under BACKLOGGED - header is at 3.
    assert_eq!(find_current_group_header(&list, 4), Some(3));
}

#[test]
fn empty_display_list() {
    let list: Vec<DisplayEntry> = vec![];
    assert_eq!(find_current_group_header(&list, 0), None);
}

#[test]
fn no_headers_at_all() {
    let list = vec![
        DisplayEntry::WorkItemEntry(0),
        DisplayEntry::WorkItemEntry(1),
    ];
    assert_eq!(find_current_group_header(&list, 0), None);
    assert_eq!(find_current_group_header(&list, 1), None);
}

#[test]
fn offset_beyond_list_length() {
    let list = make_display_list();
    // Offset far beyond the list - clamps to last valid index, finds
    // the BACKLOGGED header at index 3.
    assert_eq!(find_current_group_header(&list, 100), Some(3));
}

#[test]
fn consecutive_headers_returns_nearest() {
    // Two consecutive headers (empty first group).
    let list = vec![
        DisplayEntry::GroupHeader {
            label: "EMPTY GROUP".into(),
            count: 0,
            kind: GroupHeaderKind::Normal,
        },
        DisplayEntry::GroupHeader {
            label: "POPULATED GROUP".into(),
            count: 1,
            kind: GroupHeaderKind::Normal,
        },
        DisplayEntry::WorkItemEntry(0),
    ];
    // Offset 0: finds the first header.
    assert_eq!(find_current_group_header(&list, 0), Some(0));
    // Offset 1: finds the second header (closest).
    assert_eq!(find_current_group_header(&list, 1), Some(1));
    // Offset 2: item in second group, header at 1.
    assert_eq!(find_current_group_header(&list, 2), Some(1));
}

#[test]
fn blocked_header_kind_preserved() {
    let list = vec![
        DisplayEntry::GroupHeader {
            label: "BLOCKED (repo)".into(),
            count: 1,
            kind: GroupHeaderKind::Blocked,
        },
        DisplayEntry::WorkItemEntry(0),
    ];
    let idx = find_current_group_header(&list, 1).unwrap();
    assert_eq!(idx, 0);
    // Verify it's the blocked header (the caller can inspect the kind).
    match &list[idx] {
        DisplayEntry::GroupHeader { kind, .. } => {
            assert_eq!(*kind, GroupHeaderKind::Blocked);
        }
        _ => panic!("expected GroupHeader"),
    }
}

#[test]
fn three_groups_scrolled_to_middle() {
    let list = vec![
        DisplayEntry::GroupHeader {
            label: "GROUP A".into(),
            count: 1,
            kind: GroupHeaderKind::Normal,
        },
        DisplayEntry::WorkItemEntry(0),
        DisplayEntry::GroupHeader {
            label: "GROUP B".into(),
            count: 2,
            kind: GroupHeaderKind::Normal,
        },
        DisplayEntry::WorkItemEntry(1),
        DisplayEntry::WorkItemEntry(2),
        DisplayEntry::GroupHeader {
            label: "GROUP C".into(),
            count: 1,
            kind: GroupHeaderKind::Normal,
        },
        DisplayEntry::WorkItemEntry(3),
    ];
    // Scrolled to second item of GROUP B (index 4).
    assert_eq!(find_current_group_header(&list, 4), Some(2));
    // Scrolled to GROUP C header (index 5).
    assert_eq!(find_current_group_header(&list, 5), Some(5));
    // Scrolled to item in GROUP C (index 6).
    assert_eq!(find_current_group_header(&list, 6), Some(5));
}

// -- predict_list_offset tests --
//
// The predictor must match `ratatui_widgets::list::List::get_items_bounds`
// for the default `scroll_padding = 0` case. The parallel-render helper
// below builds a real `List` widget from the same item heights and
// renders it into a `TestBackend`, then compares `state.offset()`
// against the predictor's output. This catches any drift between our
// simulation and ratatui's actual math (e.g. if a future ratatui
// version changes the algorithm).

use ratatui_core::backend::TestBackend;
use ratatui_core::layout::Rect;
use ratatui_core::terminal::Terminal;
use ratatui_core::text::{Line, Text};
use ratatui_core::widgets::StatefulWidget;
use ratatui_widgets::list::{List, ListItem, ListState};

use super::predict_list_offset;

/// Build a `Vec<ListItem>` where item `i` has `item_heights[i]` rows.
/// Each row is a short, unique placeholder line so ratatui sees the
/// heights we specified via `ListItem::height()`.
fn items_with_heights(item_heights: &[usize]) -> Vec<ListItem<'static>> {
    item_heights
        .iter()
        .enumerate()
        .map(|(i, &h)| {
            let lines: Vec<Line<'static>> =
                (0..h).map(|r| Line::from(format!("i{i}r{r}"))).collect();
            ListItem::new(Text::from(lines))
        })
        .collect()
}

/// Render a real `List` through a `TestBackend` with the given heights,
/// prev offset, selection, and viewport height, and return the offset
/// that ratatui chose. This is the ground truth the predictor must
/// match.
fn ratatui_offset(
    item_heights: &[usize],
    prev_offset: usize,
    selected: Option<usize>,
    max_height: u16,
) -> usize {
    // Width is arbitrary; item_heights is authoritative.
    let backend = TestBackend::new(20, max_height.max(1));
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = ListState::default().with_offset(prev_offset);
    state.select(selected);
    let items = items_with_heights(item_heights);
    terminal
        .draw(|frame| {
            let list = List::new(items);
            let area = Rect {
                x: 0,
                y: 0,
                width: 20,
                height: max_height,
            };
            StatefulWidget::render(list, area, frame.buffer_mut(), &mut state);
        })
        .unwrap();
    state.offset()
}

/// Assert the predictor matches ratatui's actual offset for a given case.
fn assert_predictor_matches(
    item_heights: &[usize],
    prev_offset: usize,
    selected: Option<usize>,
    max_height: u16,
    case: &str,
) {
    let actual = ratatui_offset(item_heights, prev_offset, selected, max_height);
    let predicted = predict_list_offset(item_heights, prev_offset, selected, max_height as usize);
    assert_eq!(
        predicted, actual,
        "predictor disagreed with ratatui for case `{case}`: \
             heights={item_heights:?} prev_offset={prev_offset} \
             selected={selected:?} max_height={max_height}"
    );
}

#[test]
fn predict_empty_list() {
    assert_eq!(predict_list_offset(&[], 0, None, 10), 0);
    assert_eq!(predict_list_offset(&[], 0, Some(5), 10), 0);
    assert_eq!(predict_list_offset(&[], 7, Some(0), 10), 0);
}

#[test]
fn predict_zero_max_height() {
    // Degenerate: zero rows available. The predictor returns the
    // clamped prev_offset without touching ratatui (which would
    // panic). This is a defensive path; production callers never
    // pass max_height=0 because the inner area is always >= 1 row
    // when this function is called.
    assert_eq!(predict_list_offset(&[1, 2, 3], 1, Some(2), 0), 1);
}

#[test]
fn predict_no_scroll_needed() {
    // Everything fits, selected item is first.
    let heights = vec![1, 2, 2, 2];
    assert_predictor_matches(&heights, 0, Some(0), 10, "no scroll, select first");
}

#[test]
fn predict_selection_below_viewport_scrolls_down() {
    // Items don't fit; the selected index is past the tail, so the
    // list must scroll down.
    let heights = vec![1, 2, 2, 2, 1, 2, 2, 2, 2, 2];
    assert_predictor_matches(&heights, 0, Some(9), 8, "scroll down to last");
}

#[test]
fn predict_selection_above_viewport_scrolls_up() {
    // prev_offset is deep into the list but the selection is above
    // it - must scroll back up.
    let heights = vec![2, 2, 2, 2, 2, 2];
    assert_predictor_matches(&heights, 4, Some(0), 6, "scroll up to first");
}

#[test]
fn predict_variable_heights_mixed() {
    // Headers (1 row) interleaved with work items (2 rows) - the
    // bug case from the user's screenshot.
    let heights = vec![1, 2, 2, 2, 1, 2, 2, 2, 2, 2];
    assert_predictor_matches(&heights, 0, Some(9), 7, "sticky-bug layout");
    assert_predictor_matches(&heights, 0, Some(8), 8, "sticky-bug layout deeper");
}

#[test]
fn predict_selection_is_first_item_of_second_group() {
    // Display list with two 1-row headers at indices 0 and 4, and
    // 2-row items elsewhere. Select the first item under the second
    // header - exactly the scenario from the regression test.
    let heights = vec![1, 2, 2, 2, 1, 2, 2];
    assert_predictor_matches(&heights, 0, Some(5), 8, "first item of second group, fits");
    // Same list with a shorter viewport.
    assert_predictor_matches(
        &heights,
        0,
        Some(5),
        5,
        "first item of second group, short viewport",
    );
}

#[test]
fn predict_last_index_from_offset_zero() {
    let heights = vec![3, 3, 3, 3, 3];
    assert_predictor_matches(&heights, 0, Some(4), 6, "last item, tight");
}

#[test]
fn predict_selection_none_resets_offset_like_ratatui() {
    // ratatui's `ListState::select(None)` also resets the offset to 0.
    // The production call path renders with select() after with_offset,
    // so when no item is selected the effective offset is 0 regardless
    // of what prev_offset we pass in. The predictor must match.
    let heights = vec![1, 1, 1, 1, 1, 1, 1, 1];
    assert_predictor_matches(&heights, 3, None, 4, "no selection, reset to 0");
    let predicted = predict_list_offset(&heights, 3, None, 4);
    assert_eq!(predicted, 0);
}

#[test]
fn predict_single_item_fits() {
    let heights = vec![1];
    assert_predictor_matches(&heights, 0, Some(0), 3, "single item");
}

#[test]
fn predict_offset_past_end_is_clamped() {
    // prev_offset exceeds the list length. ratatui clamps to
    // items.len()-1 before doing any work; the predictor must match.
    let heights = vec![1, 1, 1];
    assert_predictor_matches(&heights, 99, Some(0), 3, "offset past end");
}

// -- recenter_offset / compute_max_item_offset tests --
//
// These exercise the pure viewport math added for mouse-wheel
// scrolling. The renderer itself is covered by the snapshot
// tests; these tests isolate the centering / clamping logic so a
// regression in either is pinpointed without a full render round
// trip.

use super::{compute_max_item_offset, recenter_offset};

#[test]
fn recenter_centers_middle_item_in_long_list() {
    // 20 items, all height 1, viewport of 10 rows. A selection
    // in the middle (index 10) should produce an offset that
    // leaves roughly equal rows above and below.
    let heights = vec![1usize; 20];
    let offset = recenter_offset(&heights, 10, 10);
    // Target row = 10 - 5 = 5; first item at cumulative 5 is
    // index 5 (cumulative 0..4 -> j<=5 means chosen=5).
    assert_eq!(offset, 5);
}

#[test]
fn recenter_clamps_at_top() {
    // Selecting item near the top of a long list must not go
    // below offset 0 (no negative offsets).
    let heights = vec![1usize; 20];
    assert_eq!(recenter_offset(&heights, 0, 10), 0);
    assert_eq!(recenter_offset(&heights, 2, 10), 0);
}

#[test]
fn recenter_clamps_at_bottom() {
    // Selecting the last item produces the max legal offset so
    // the tail items are all visible with no wasted space.
    let heights = vec![1usize; 20];
    let max = compute_max_item_offset(&heights, 10);
    assert_eq!(recenter_offset(&heights, 19, 10), max);
    assert_eq!(max, 10, "20 items / body 10 -> max offset 10");
}

#[test]
fn recenter_handles_variable_heights() {
    // Mixed heights that mirror the real list shape: group
    // headers (1 row) between items (2 rows each).
    let heights = vec![1, 2, 2, 2, 1, 2, 2, 2, 2];
    // body = 5 rows, selected = 7 (a 2-row item deep in group 2).
    let offset = recenter_offset(&heights, 7, 5);
    // sel_row = sum(heights[0..7]) = 1+2+2+2+1+2+2 = 12.
    // sel_center = 12 + 1 = 13. target = 13 - 2 = 11.
    // Walk: j=0 cum=0 ok chosen=0, cum=1; j=1 cum=1 chosen=1, cum=3;
    // j=2 cum=3 chosen=2, cum=5; j=3 cum=5 chosen=3, cum=7;
    // j=4 cum=7 chosen=4, cum=8; j=5 cum=8 chosen=5, cum=10;
    // j=6 cum=10 chosen=6, cum=12; j=7 cum=12 > 11 break.
    // -> chosen=6. clamped by max_offset for body=5:
    //    from tail acc: i=8 h=2 acc=2; i=7 h=2 acc=4; i=6 h=2 acc=6>5
    //    return 7. So max_offset=7. min(6,7)=6.
    assert_eq!(offset, 6);
}

#[test]
fn compute_max_item_offset_fits_whole_list() {
    // When every item fits, the max offset is 0 (you can't
    // scroll a list that's shorter than its viewport).
    let heights = vec![2, 2, 2];
    assert_eq!(compute_max_item_offset(&heights, 10), 0);
}

#[test]
fn compute_max_item_offset_short_viewport() {
    // All items 1 row, 10 items, body 3 rows -> last 3 items
    // fit. First offset that fits-everything-from-there is 7.
    let heights = vec![1usize; 10];
    assert_eq!(compute_max_item_offset(&heights, 3), 7);
}

#[test]
fn recenter_empty_or_oversized_selection_is_zero() {
    // Defensive: out-of-range selection or empty list returns 0.
    assert_eq!(recenter_offset(&[], 0, 10), 0);
    assert_eq!(recenter_offset(&[1, 2], 99, 10), 0);
    // body_height = 0 is degenerate (inner too small).
    assert_eq!(recenter_offset(&[1, 2, 3], 1, 0), 0);
}
