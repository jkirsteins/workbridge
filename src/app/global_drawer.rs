//! `GlobalDrawer` subsystem - the persistent global-assistant drawer
//! state (PTY session, MCP server, context, geometry, spawn lifecycle).
//!
//! Stage 2.15 of the Phase 4 logical decomposition. `App` previously
//! held ten sibling fields for the global-assistant feature
//! (`global_drawer_open`, `global_session`, `global_mcp_server`,
//! `global_mcp_context`, `pre_drawer_focus`, `global_pane_cols`,
//! `global_pane_rows`, `global_mcp_config_path`,
//! `global_session_open_pending`, `global_mcp_context_dirty`) plus
//! the global PTY write buffer (`pending_global_pty_bytes`). That is
//! the complete state for one feature - the drawer lifecycle -
//! scattered across the top level of the struct. This module groups
//! them as a single `GlobalDrawer` owner so spawn/teardown can be
//! reasoned about in one place.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{FocusPanel, GlobalSessionOpenPending};
use crate::mcp::McpSocketServer;
use crate::work_item::SessionEntry;

/// Owns all global-assistant drawer state.
pub struct GlobalDrawer {
    /// Whether the drawer is currently open.
    pub open: bool,
    /// The global assistant PTY session (lazy, persistent).
    pub session: Option<SessionEntry>,
    /// MCP socket server for the global assistant.
    pub mcp_server: Option<McpSocketServer>,
    /// Dynamic context for the global MCP server, updated on each tick.
    pub mcp_context: Arc<Mutex<String>>,
    /// Which panel had focus before the drawer opened (restored on close).
    pub pre_drawer_focus: FocusPanel,
    /// PTY columns for the global assistant drawer (differs from main pane).
    pub pane_cols: u16,
    /// PTY rows for the global assistant drawer.
    pub pane_rows: u16,
    /// Path to the temp MCP config file for the global assistant.
    /// Tracked so it can be cleaned up on shutdown or respawn.
    pub mcp_config_path: Option<PathBuf>,
    /// In-flight preparation for the global assistant session.
    /// Populated by `spawn_global_session` while a background worker
    /// runs `McpSocketServer::start_global`, writes the MCP config
    /// tempfile, and calls `Session::spawn`. Drained by
    /// `poll_global_session_open` on each tick. Kept as a named
    /// struct so the activity ID cannot accidentally leak a permanent
    /// spinner.
    pub session_open_pending: Option<GlobalSessionOpenPending>,
    /// True when repo/work-item data has changed since the last
    /// `refresh_global_mcp_context` call. Set by `drain_fetch_results`
    /// returning true; cleared after the refresh runs.
    pub mcp_context_dirty: bool,
    /// Buffered bytes destined for the global PTY session. Key events
    /// that forward to the PTY push here instead of writing
    /// immediately. Flushed as a single write on the next timer tick.
    pub pending_pty_bytes: Vec<u8>,
}

impl GlobalDrawer {
    /// Construct an empty (closed) global drawer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            open: false,
            session: None,
            mcp_server: None,
            mcp_context: Arc::new(Mutex::new("{}".to_string())),
            pre_drawer_focus: FocusPanel::Left,
            pane_cols: 80,
            pane_rows: 24,
            mcp_config_path: None,
            session_open_pending: None,
            mcp_context_dirty: false,
            pending_pty_bytes: Vec::new(),
        }
    }
}

impl Default for GlobalDrawer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_global_drawer_is_closed() {
        let d = GlobalDrawer::new();
        assert!(!d.open);
        assert!(d.session.is_none());
        assert!(d.mcp_server.is_none());
        assert!(d.session_open_pending.is_none());
        assert!(!d.mcp_context_dirty);
        assert!(d.pending_pty_bytes.is_empty());
    }
}
