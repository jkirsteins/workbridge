//! Shell subsystem - top-level app chrome (quit, focus, status bar,
//! pane geometry, shutdown sequence).
//!
//! Stage 2.5 of the Phase 4 logical decomposition. `App` previously
//! held `should_quit`, `focus`, `status_message`, `confirm_quit`,
//! `pane_cols`, `pane_rows`, `shutting_down`, and `shutdown_started`
//! as eight sibling fields. All of them belong to a single "shell"
//! concern: the outer window chrome + status bar + quit / shutdown
//! lifecycle. Grouping them into a `Shell` struct makes the state
//! machine legible in one place.

use super::FocusPanel;

/// Owns the app-wide shell state: quit flag, focus panel, status
/// bar message, shutdown lifecycle, and PTY pane dimensions. The
/// `confirm_quit` flag is the debounce for the double-Q quit
/// gesture; `shutting_down` + `shutdown_started` drive the 10-
/// second graceful-exit window during which only force-quit is
/// accepted.
pub struct Shell {
    /// True once a quit has been committed and the event loop
    /// should tear down on the next tick.
    pub should_quit: bool,
    /// Which top-level panel has keyboard focus.
    pub focus: FocusPanel,
    /// Status message displayed to the user (errors, confirmations,
    /// etc.). Cleared by the next status-level action or by the
    /// background `drain_pending_fetch_errors` tick.
    pub status_message: Option<String>,
    /// True when waiting for a second press to confirm quit.
    pub confirm_quit: bool,
    /// The terminal columns available for the right panel (PTY pane).
    pub pane_cols: u16,
    /// The terminal rows available for the right panel (PTY pane).
    pub pane_rows: u16,
    /// True when the app has sent SIGTERM to all sessions and is
    /// waiting for them to exit. During shutdown, only force-quit
    /// (Q) is accepted.
    pub shutting_down: bool,
    /// When shutdown was initiated. Used to enforce the 10-second
    /// deadline after which all remaining sessions are force-killed.
    pub shutdown_started: Option<std::time::Instant>,
}

impl Shell {
    /// Construct the initial shell state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            should_quit: false,
            focus: FocusPanel::Left,
            status_message: None,
            confirm_quit: false,
            pane_cols: 80,
            pane_rows: 24,
            shutting_down: false,
            shutdown_started: None,
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_shell_is_in_left_focus_not_quitting() {
        let s = Shell::new();
        assert!(!s.should_quit);
        assert!(matches!(s.focus, FocusPanel::Left));
        assert!(s.status_message.is_none());
        assert!(!s.confirm_quit);
        assert!(!s.shutting_down);
        assert!(s.shutdown_started.is_none());
    }

    #[test]
    fn default_and_new_match() {
        // Regression path: `Shell::default()` and `Shell::new()` must
        // produce identical state, or tests that pick one shape
        // accidentally observe different starting points.
        let a = Shell::new();
        let b = Shell::default();
        assert_eq!(a.should_quit, b.should_quit);
        assert_eq!(a.confirm_quit, b.confirm_quit);
        assert_eq!(a.pane_cols, b.pane_cols);
        assert_eq!(a.pane_rows, b.pane_rows);
        assert_eq!(a.shutting_down, b.shutting_down);
    }
}
