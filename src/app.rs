use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::Config;
use crate::session::Session;

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FocusPanel {
    Left,
    Right,
}

/// Tab represents a single Claude Code session backed by a PTY.
pub struct Tab {
    pub name: String,
    /// Shared vt100 parser fed by the session's reader thread.
    /// The UI thread locks this to call .screen() for rendering.
    /// None if no session was spawned (parser lives in the Session).
    pub parser: Arc<Mutex<vt100::Parser>>,
    /// Whether the backing child process is alive.
    pub alive: bool,
    /// The PTY session. None if the session failed to spawn.
    pub session: Option<Session>,
}

/// App holds the entire application state.
pub struct App {
    pub tabs: Vec<Tab>,
    pub selected_tab: Option<usize>,
    pub should_quit: bool,
    pub focus: FocusPanel,
    /// Status message displayed to the user (errors, confirmations, etc.).
    pub status_message: Option<String>,
    /// True when waiting for a second press to confirm quit.
    pub confirm_quit: bool,
    /// True when waiting for a second press to confirm tab deletion.
    pub confirm_delete: bool,
    /// True when the app has sent SIGTERM to all sessions and is waiting
    /// for them to exit. During shutdown, only Q (force quit) is accepted.
    pub shutting_down: bool,
    /// When shutdown was initiated. Used to enforce the 10-second deadline
    /// after which all remaining sessions are force-killed.
    pub shutdown_started: Option<Instant>,
    next_id: u32,
    /// The terminal columns available for the right panel (PTY pane).
    pub pane_cols: u16,
    /// The terminal rows available for the right panel (PTY pane).
    pub pane_rows: u16,
    /// The loaded configuration (repo paths, base dirs, defaults).
    pub config: Config,
    /// Repos discovered by scanning base_dirs at startup.
    pub discovered_repos: Vec<PathBuf>,
    /// Whether to show the settings overlay.
    pub show_settings: bool,
}

impl App {
    /// Create a new App with default (empty) config and no discovered repos.
    /// Used by tests as a convenience constructor.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_config(Config::default(), Vec::new())
    }

    /// Create a new App with the given config and pre-discovered repos.
    pub fn with_config(config: Config, discovered_repos: Vec<PathBuf>) -> Self {
        Self {
            tabs: Vec::new(),
            selected_tab: None,
            should_quit: false,
            focus: FocusPanel::Left,
            status_message: None,
            confirm_quit: false,
            confirm_delete: false,
            shutting_down: false,
            shutdown_started: None,
            next_id: 0,
            pane_cols: 80,
            pane_rows: 24,
            config,
            discovered_repos,
            show_settings: false,
        }
    }

    /// Create a new tab, spawning a PTY session with `claude` running inside it.
    ///
    /// If `cwd` is provided, the child process starts in that directory.
    /// Otherwise it inherits the parent's working directory.
    pub fn new_tab(&mut self, cwd: Option<&Path>) {
        let id = self.next_id;
        self.next_id += 1;
        let name = format!("Tab {id}");

        match Session::spawn(self.pane_cols, self.pane_rows, cwd, &["claude"]) {
            Ok(session) => {
                let parser = Arc::clone(&session.parser);
                let tab = Tab {
                    name,
                    parser,
                    alive: true,
                    session: Some(session),
                };

                self.tabs.push(tab);
                self.selected_tab = Some(self.tabs.len() - 1);
                self.status_message = None;
            }
            Err(e) => {
                self.status_message = Some(format!("Error creating tab: {e}"));
            }
        }
    }

    /// Delete the currently selected tab, killing its PTY session.
    pub fn delete_tab(&mut self) {
        let Some(idx) = self.selected_tab else {
            return;
        };
        if idx >= self.tabs.len() {
            return;
        }

        // Kill the child process if it is still running.
        if let Some(ref mut session) = self.tabs[idx].session {
            session.kill();
        }

        self.tabs.remove(idx);

        // Return focus to the left panel and clear the status bar after deleting.
        self.focus = FocusPanel::Left;
        self.status_message = None;

        if self.tabs.is_empty() {
            self.selected_tab = None;
        } else if idx >= self.tabs.len() {
            self.selected_tab = Some(self.tabs.len() - 1);
        } else {
            self.selected_tab = Some(idx);
        }
    }

    /// Move selection to the next tab (wrapping around).
    pub fn next_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        match self.selected_tab {
            Some(idx) => {
                self.selected_tab = Some((idx + 1) % self.tabs.len());
            }
            None => {
                self.selected_tab = Some(0);
            }
        }
    }

    /// Move selection to the previous tab (wrapping around).
    pub fn prev_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        match self.selected_tab {
            Some(idx) => {
                if idx == 0 {
                    self.selected_tab = Some(self.tabs.len() - 1);
                } else {
                    self.selected_tab = Some(idx - 1);
                }
            }
            None => {
                self.selected_tab = Some(self.tabs.len() - 1);
            }
        }
    }

    /// Check liveness (try_wait) on all tabs. Called on periodic ticks.
    ///
    /// The reader threads handle PTY output continuously - no reading
    /// happens here. This only checks if child processes have exited.
    pub fn check_liveness(&mut self) {
        for tab in self.tabs.iter_mut() {
            if let Some(ref mut session) = tab.session {
                tab.alive = session.is_alive();
            } else {
                tab.alive = false;
            }
        }
    }

    /// Resize PTY sessions and vt100 parsers to match the current pane
    /// dimensions. Resize is an instant ioctl call, so we resize all tabs
    /// immediately. The Session::resize method handles both the PTY ioctl
    /// and the parser resize.
    pub fn resize_pty_panes(&mut self) {
        for tab in &mut self.tabs {
            if let Some(ref session) = tab.session {
                let _ = session.resize(self.pane_cols, self.pane_rows);
            }
        }
    }

    /// Send SIGTERM to all alive sessions without waiting.
    /// Used to initiate graceful shutdown - the main loop continues
    /// running so the UI stays responsive.
    pub fn send_sigterm_all(&mut self) {
        for tab in &mut self.tabs {
            if tab.alive
                && let Some(ref mut session) = tab.session
            {
                session.send_sigterm();
            }
        }
    }

    /// Check if all tabs are dead (or there are no tabs).
    pub fn all_dead(&self) -> bool {
        self.tabs.iter().all(|tab| !tab.alive)
    }

    /// SIGKILL all remaining alive sessions. Used for force-quit during
    /// the shutdown wait.
    pub fn force_kill_all(&mut self) {
        for tab in &mut self.tabs {
            if let Some(ref mut session) = tab.session {
                session.force_kill();
            }
            tab.alive = false;
        }
    }

    /// Send raw bytes to the active tab's PTY session.
    pub fn send_bytes_to_active(&mut self, data: &[u8]) {
        let Some(idx) = self.selected_tab else {
            return;
        };
        if idx >= self.tabs.len() {
            return;
        }
        if let Some(ref session) = self.tabs[idx].session
            && let Err(e) = session.write_bytes(data)
        {
            self.status_message = Some(format!("Send error: {e}"));
        }
    }
}
