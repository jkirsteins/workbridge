use crate::app::App;
use crate::layout;

/// Handle a terminal resize event by updating pane dimensions and resizing PTY.
/// Called from the rat-salsa event callback in salsa.rs.
pub fn handle_resize(app: &mut App, cols: u16, rows: u16) {
    let bottom_rows = u16::from(app.has_visible_status_bar())
        + u16::from(app.selected_work_item_context().is_some());
    let pl = layout::compute(cols, rows, bottom_rows);
    app.shell.pane_cols = pl.pane_cols;
    app.shell.pane_rows = pl.pane_rows;

    // Compute global drawer PTY dimensions via shared helper.
    let dl = layout::compute_drawer(cols, rows);
    app.global_pane_cols = dl.pane_cols;
    app.global_pane_rows = dl.pane_rows;

    app.resize_pty_panes();
}

/// Recalculate layout from the current terminal size and resize PTY panes.
/// Called when the status bar visibility changes to keep the PTY pane
/// dimensions in sync with the actual display area.
pub fn sync_layout(app: &mut App) {
    if let Ok((cols, rows)) = ratatui_crossterm::crossterm::terminal::size() {
        handle_resize(app, cols, rows);
    }
}
