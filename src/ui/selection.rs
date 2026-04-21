//! Selection overlay rendering (mouse drag highlight).
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Position, Rect};
use ratatui_core::style::{Modifier, Style};

use crate::work_item::SelectionState;

/// range of cells; see the regression test
/// `event::selection_clipboard_tests::highlight_cell_count_matches_clipboard_chars`.
pub fn render_selection_overlay(buf: &mut Buffer, inner_area: Rect, selection: &SelectionState) {
    let (start_row, start_col, end_row, end_col) = selection.normalized_bounds();

    let max_col = inner_area.width;

    for row in start_row..=end_row {
        if row >= inner_area.height {
            break;
        }

        let col_start = if row == start_row { start_col } else { 0 };
        let col_end = if row == end_row {
            end_col
        } else {
            max_col.saturating_sub(1)
        };
        // Single-row selection: start_col to end_col.
        // (Already handled by the above logic since start_row == end_row.)

        for col in col_start..=col_end {
            if col >= max_col {
                break;
            }
            let x = inner_area.x + col;
            let y = inner_area.y + row;
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
            }
        }
    }
}
