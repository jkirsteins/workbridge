/// Panel dimensions derived from the total terminal size.
pub struct PanelLayout {
    /// Width of the left (tab list) panel.
    pub left_width: u16,
    /// Width available for the pane content inside the right panel
    /// (after subtracting borders).
    pub pane_cols: u16,
    /// Rows available for the pane content inside the right panel
    /// (after subtracting borders).
    pub pane_rows: u16,
}

/// Compute panel layout from total terminal dimensions.
///
/// Left panel gets 25% of width (minimum 25 columns, capped at total width).
/// Right panel gets the remaining width minus 2 for borders.
/// Pane rows = total rows minus 2 for borders, minus `bottom_bar_rows` for
/// any bottom bars (context bar, status bar, etc.).
pub fn compute(cols: u16, rows: u16, bottom_bar_rows: u16) -> PanelLayout {
    let left_width = std::cmp::max(cols / 4, 25).min(cols);
    let right_raw = cols.saturating_sub(left_width).saturating_sub(2);
    let pane_cols = if right_raw > 0 { right_raw } else { 1 };

    let row_raw = rows.saturating_sub(2).saturating_sub(bottom_bar_rows);
    let pane_rows = if row_raw > 0 { row_raw } else { 1 };

    PanelLayout {
        left_width,
        pane_cols,
        pane_rows,
    }
}

/// Drawer dimensions for the global assistant overlay.
pub struct DrawerLayout {
    /// Width of the drawer rectangle (terminal width minus inset margins).
    pub drawer_width: u16,
    /// Height of the drawer rectangle (60% of terminal height, minimum 5).
    pub drawer_height: u16,
    /// Columns available for the PTY inside the drawer (after borders).
    pub pane_cols: u16,
    /// Rows available for the PTY inside the drawer (after borders).
    pub pane_rows: u16,
}

/// Compute global drawer geometry from terminal dimensions.
///
/// The drawer occupies 60% of the terminal height (minimum 5 rows),
/// inset 2 columns on each side. PTY dimensions subtract border cells.
/// Uses u32 intermediate arithmetic to avoid u16 overflow on large terminals.
pub fn compute_drawer(cols: u16, rows: u16) -> DrawerLayout {
    let drawer_width = cols.saturating_sub(4);
    // Cast to u32 to prevent overflow when rows > 1092 (rows * 60 > u16::MAX).
    let drawer_height = ((rows as u32 * 60 / 100).max(5) as u16).min(rows);
    let pane_cols = drawer_width.saturating_sub(2).max(1);
    let pane_rows = drawer_height.saturating_sub(2).max(1);
    DrawerLayout {
        drawer_width,
        drawer_height,
        pane_cols,
        pane_rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_terminal_80x24() {
        let pl = compute(80, 24, 0);
        // 80 / 4 = 20, but minimum is 25
        assert_eq!(pl.left_width, 25);
        // right = 80 - 25 - 2 (borders) = 53
        assert_eq!(pl.pane_cols, 53);
        // rows = 24 - 2 (borders) = 22
        assert_eq!(pl.pane_rows, 22);
    }

    #[test]
    fn standard_terminal_with_one_bottom_bar() {
        let pl = compute(80, 24, 1);
        assert_eq!(pl.left_width, 25);
        assert_eq!(pl.pane_cols, 53);
        // rows = 24 - 2 (borders) - 1 (bar) = 21
        assert_eq!(pl.pane_rows, 21);
    }

    #[test]
    fn standard_terminal_with_two_bottom_bars() {
        let pl = compute(80, 24, 2);
        assert_eq!(pl.left_width, 25);
        assert_eq!(pl.pane_cols, 53);
        // rows = 24 - 2 (borders) - 2 (bars) = 20
        assert_eq!(pl.pane_rows, 20);
    }

    #[test]
    fn wide_terminal_200x50() {
        let pl = compute(200, 50, 0);
        // 200 / 4 = 50, above minimum of 30
        assert_eq!(pl.left_width, 50);
        // right = 200 - 50 - 2 = 148
        assert_eq!(pl.pane_cols, 148);
        // rows = 50 - 2 = 48
        assert_eq!(pl.pane_rows, 48);
    }

    #[test]
    fn tiny_terminal_clamps_to_minimums() {
        let pl = compute(10, 3, 0);
        // 10 / 5 = 2, but minimum is 20, capped at cols (10)
        assert_eq!(pl.left_width, 10);
        // right = 10 - 10 - 2 = 0 saturated, so pane_cols = 1 (floor)
        assert_eq!(pl.pane_cols, 1);
        // rows = 3 - 2 = 1
        assert_eq!(pl.pane_rows, 1);
    }

    #[test]
    fn zero_dimensions_produce_minimum_1() {
        let pl = compute(0, 0, 0);
        assert_eq!(pl.pane_cols, 1);
        assert_eq!(pl.pane_rows, 1);
    }

    #[test]
    fn bottom_bars_on_tiny_terminal() {
        let pl = compute(80, 2, 1);
        // rows = 2 - 2 (borders) - 1 (bar) = 0 saturated -> 1
        assert_eq!(pl.pane_rows, 1);
    }

    #[test]
    fn drawer_standard_terminal() {
        let dl = compute_drawer(80, 24);
        // width = 80 - 4 = 76
        assert_eq!(dl.drawer_width, 76);
        // height = 24 * 60 / 100 = 14
        assert_eq!(dl.drawer_height, 14);
        // pane_cols = 76 - 2 = 74
        assert_eq!(dl.pane_cols, 74);
        // pane_rows = 14 - 2 = 12
        assert_eq!(dl.pane_rows, 12);
    }

    #[test]
    fn drawer_tiny_terminal() {
        let dl = compute_drawer(10, 4);
        // width = 10 - 4 = 6
        assert_eq!(dl.drawer_width, 6);
        // height = 4 * 60 / 100 = 2, but min 5 -> 5, capped at rows -> 4
        assert_eq!(dl.drawer_height, 4);
        // pane_cols = 6 - 2 = 4
        assert_eq!(dl.pane_cols, 4);
        // pane_rows = 4 - 2 = 2
        assert_eq!(dl.pane_rows, 2);
    }

    #[test]
    fn drawer_no_u16_overflow_large_rows() {
        // rows=2000: 2000*60=120000 would overflow u16 (max 65535).
        // With u32 intermediate: 120000/100=1200, capped to u16 fine.
        let dl = compute_drawer(200, 2000);
        assert_eq!(dl.drawer_height, 1200);
        assert_eq!(dl.pane_rows, 1198);
    }
}
