use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::config::{Config, RepoEntry, RepoSource};
use crate::session::Session;

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FocusPanel {
    Left,
    Right,
}

/// Which list has focus inside the settings overlay.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsListFocus {
    Managed,
    Available,
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
    /// Whether to show the settings overlay.
    pub show_settings: bool,
    /// Cached active repo entries (explicit + included). Rebuilt when
    /// inclusions change, not on every frame or keypress.
    pub active_repo_cache: Vec<RepoEntry>,
    /// Cursor position in the managed repos list.
    pub settings_repo_selected: usize,
    /// Cursor position in the available repos list.
    pub settings_available_selected: usize,
    /// Which list has focus inside the settings overlay.
    pub settings_list_focus: SettingsListFocus,
}

impl App {
    /// Create a new App with default (empty) config.
    /// Used by tests as a convenience constructor.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    /// Create a new App with the given config.
    pub fn with_config(config: Config) -> Self {
        let active_repo_cache = config.active_repos();
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
            show_settings: false,
            active_repo_cache,
            settings_repo_selected: 0,
            settings_available_selected: 0,
            settings_list_focus: SettingsListFocus::Managed,
        }
    }

    /// Rebuild the cached active repo list after inclusion changes.
    fn refresh_repo_cache(&mut self) {
        self.active_repo_cache = self.config.active_repos();
    }

    /// Total number of active repos for scroll bounds.
    pub fn total_repos(&self) -> usize {
        self.active_repo_cache.len()
    }

    /// Build the list of available (unmanaged) repos: all repos minus active.
    /// Used by the settings overlay to show what can be managed.
    pub fn available_repos(&self) -> Vec<RepoEntry> {
        let active_paths: Vec<_> = self
            .active_repo_cache
            .iter()
            .map(|e| &e.path)
            .collect();
        self.config
            .all_repos()
            .into_iter()
            .filter(|entry| !active_paths.contains(&&entry.path))
            .collect()
    }

    /// Unmanage the currently selected managed repo and save config.
    /// Removes from included_repos. Explicit repos cannot be unmanaged
    /// this way (they must be removed via `remove_path`).
    /// If the save fails, the in-memory mutation is rolled back so the
    /// UI stays consistent with what is persisted on disk.
    pub fn unmanage_selected_repo(&mut self) {
        if self.active_repo_cache.is_empty() {
            return;
        }
        let idx = self
            .settings_repo_selected
            .min(self.active_repo_cache.len().saturating_sub(1));
        let entry = &self.active_repo_cache[idx];
        if entry.source == RepoSource::Explicit {
            self.status_message =
                Some("Explicit repos cannot be unmanaged (use 'repos remove')".into());
            return;
        }
        let path = entry.path.display().to_string();
        self.config.uninclude_repo(&path);
        if let Err(e) = self.config.save() {
            // Rollback: re-add the inclusion since save failed.
            self.config.include_repo(&path);
            self.status_message = Some(format!("Error saving config: {e}"));
            return;
        }
        self.status_message = Some(format!("Unmanaged: {path}"));
        self.refresh_repo_cache();
        // Adjust cursor if it went past the end.
        if !self.active_repo_cache.is_empty() {
            self.settings_repo_selected = self
                .settings_repo_selected
                .min(self.active_repo_cache.len() - 1);
        } else {
            self.settings_repo_selected = 0;
        }
    }

    /// Manage the currently selected available repo and save config.
    /// Adds to included_repos.
    /// If the save fails, the in-memory mutation is rolled back.
    pub fn manage_selected_repo(&mut self) {
        let available = self.available_repos();
        if available.is_empty() {
            return;
        }
        let idx = self
            .settings_available_selected
            .min(available.len().saturating_sub(1));
        let path = available[idx].path.display().to_string();
        self.config.include_repo(&path);
        if let Err(e) = self.config.save() {
            // Rollback: remove the inclusion since save failed.
            self.config.uninclude_repo(&path);
            self.status_message = Some(format!("Error saving config: {e}"));
            return;
        }
        self.status_message = Some(format!("Managed: {path}"));
        self.refresh_repo_cache();
        // Adjust cursor if it went past the end.
        let new_available = self.available_repos();
        let new_len = new_available.len();
        if new_len > 0 {
            self.settings_available_selected =
                self.settings_available_selected.min(new_len - 1);
        } else {
            self.settings_available_selected = 0;
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
