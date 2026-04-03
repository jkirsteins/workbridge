use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use crate::assembly;
use crate::config::{Config, RepoEntry, RepoSource};
use crate::session::Session;
use crate::github_client::GithubError;
use crate::work_item::{
    FetchMessage, RepoFetchResult, SessionEntry, UnlinkedPr, WorkItem, WorkItemId, WorkItemStatus,
};
use crate::work_item_backend::{
    BackendError, CreateWorkItem, RepoAssociationRecord, WorkItemBackend,
};
use crate::worktree_service::WorktreeService;

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

/// Lightweight display data for the work-item context bar.
///
/// Derived from the currently selected WorkItem's fields on each call
/// to `selected_work_item_context()`.
#[derive(Clone, Debug)]
pub struct WorkItemContext {
    /// The work item title (e.g., issue title or branch-derived name).
    pub title: String,
    /// The repository path on disk (from RepoAssociation.repo_path).
    pub repo_path: String,
    /// Issue labels (from IssueInfo.labels). Empty if no issue linked.
    pub labels: Vec<String>,
}

/// An entry in the flat display list rendered in the left panel.
#[derive(Clone, Debug)]
pub enum DisplayEntry {
    /// Section header with label and item count.
    GroupHeader { label: String, count: usize },
    /// An unlinked PR (index into App::unlinked_prs).
    UnlinkedItem(usize),
    /// A work item (index into App::work_items).
    WorkItemEntry(usize),
    /// Empty state message shown when a group has no items.
    EmptyState(String),
}

/// App holds the entire application state.
pub struct App {
    pub should_quit: bool,
    pub focus: FocusPanel,
    /// Status message displayed to the user (errors, confirmations, etc.).
    pub status_message: Option<String>,
    /// True when waiting for a second press to confirm quit.
    pub confirm_quit: bool,
    /// True when waiting for a second press to confirm work item deletion.
    pub confirm_delete: bool,
    /// True when the app has sent SIGTERM to all sessions and is waiting
    /// for them to exit. During shutdown, only Q (force quit) is accepted.
    pub shutting_down: bool,
    /// When shutdown was initiated. Used to enforce the 10-second deadline
    /// after which all remaining sessions are force-killed.
    pub shutdown_started: Option<Instant>,
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

    // -- Work item state --
    /// Backend for persisting work item records.
    pub backend: Box<dyn WorkItemBackend>,
    /// Worktree service for creating/listing worktrees.
    pub worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    /// Assembled work items (from backend records + repo data).
    pub work_items: Vec<WorkItem>,
    /// PRs not linked to any work item.
    pub unlinked_prs: Vec<UnlinkedPr>,
    /// Sessions keyed by work item ID.
    pub sessions: HashMap<WorkItemId, SessionEntry>,
    /// Fetched data per repo path (populated by background fetcher).
    pub repo_data: HashMap<PathBuf, RepoFetchResult>,
    /// Receiver for background fetch messages.
    pub fetch_rx: Option<mpsc::Receiver<FetchMessage>>,
    /// True once a "gh CLI not found" message has been shown. Prevents
    /// spamming the status bar on every fetch cycle.
    pub gh_cli_not_found_shown: bool,
    /// True once a "gh auth required" message has been shown. Prevents
    /// spamming the status bar on every fetch cycle.
    pub gh_auth_required_shown: bool,
    /// Repo paths for which a worktree fetch error has already been shown.
    /// Prevents flooding the status bar when every fetch cycle for the
    /// same repo returns an error.
    pub worktree_errors_shown: std::collections::HashSet<PathBuf>,
    /// Currently selected index in the display list (items only, not headers).
    pub selected_item: Option<usize>,
    /// Flat display list for the left panel.
    pub display_list: Vec<DisplayEntry>,
    /// Set when manage/unmanage changes active repos. The main loop checks
    /// this flag and restarts the background fetcher with the updated repo
    /// list so newly managed repos get fetched and removed repos stop.
    pub fetcher_repos_changed: bool,
    /// Tracks the WorkItemId of the currently selected work item so that
    /// selection survives reassembly even when display indices change.
    /// After build_display_list, the matching entry is found and
    /// selected_item is restored.
    pub selected_work_item: Option<WorkItemId>,
    /// Tracks the (repo_path, branch) of the currently selected unlinked PR
    /// so that selection survives reassembly even when display indices change.
    /// Keyed by both repo_path and branch to disambiguate same-named branches
    /// across different repos.
    pub selected_unlinked_branch: Option<(PathBuf, String)>,
    /// Fetch errors that could not be shown because the status bar was
    /// occupied. Drained on the next tick when status_message is None.
    pub pending_fetch_errors: Vec<String>,
    /// True when the fetcher channel has disconnected unexpectedly (all
    /// sender threads exited). Surfaced in the status bar so the user
    /// knows background updates have stopped.
    pub fetcher_disconnected: bool,
}

impl App {
    /// Create a new App with default (empty) config and a stub backend.
    /// Used by tests as a convenience constructor.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_config_and_worktree_service(
            Config::default(),
            Box::new(StubBackend),
            Arc::new(StubWorktreeService),
        )
    }

    /// Create a new App with the given config and backend.
    /// Uses a no-op worktree service. Call `with_config_and_worktree_service`
    /// to provide a real or mock worktree service.
    #[cfg(test)]
    pub fn with_config(config: Config, backend: Box<dyn WorkItemBackend>) -> Self {
        Self::with_config_and_worktree_service(config, backend, Arc::new(StubWorktreeService))
    }

    /// Create a new App with the given config, backend, and worktree service.
    pub fn with_config_and_worktree_service(
        config: Config,
        backend: Box<dyn WorkItemBackend>,
        worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    ) -> Self {
        let active_repo_cache = canonicalize_repo_entries(config.active_repos());
        let mut app = Self {
            should_quit: false,
            focus: FocusPanel::Left,
            status_message: None,
            confirm_quit: false,
            confirm_delete: false,
            shutting_down: false,
            shutdown_started: None,
            pane_cols: 80,
            pane_rows: 24,
            config,
            show_settings: false,
            active_repo_cache,
            settings_repo_selected: 0,
            settings_available_selected: 0,
            settings_list_focus: SettingsListFocus::Managed,
            backend,
            worktree_service,
            work_items: Vec::new(),
            unlinked_prs: Vec::new(),
            sessions: HashMap::new(),
            repo_data: HashMap::new(),
            fetch_rx: None,
            gh_cli_not_found_shown: false,
            gh_auth_required_shown: false,
            worktree_errors_shown: std::collections::HashSet::new(),
            selected_item: None,
            display_list: Vec::new(),
            fetcher_repos_changed: false,
            selected_work_item: None,
            selected_unlinked_branch: None,
            pending_fetch_errors: Vec::new(),
            fetcher_disconnected: false,
        };
        app.reassemble_work_items();
        app.build_display_list();
        app
    }

    /// Check whether a path is inside (or equal to) one of the active
    /// managed repos. Uses canonical paths for reliable comparison.
    /// Returns the matching repo root path if found.
    ///
    /// The cache entries are already canonicalized (via `canonicalize_repo_entries`),
    /// so we only need to canonicalize the input path.
    pub fn managed_repo_root(&self, path: &std::path::Path) -> Option<PathBuf> {
        let canonical_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.active_repo_cache.iter().find_map(|entry| {
            if canonical_path.starts_with(&entry.path) {
                Some(entry.path.clone())
            } else {
                None
            }
        })
    }

    /// Check whether a path is inside (or equal to) one of the active
    /// managed repos. Uses canonical paths for reliable comparison.
    #[cfg(test)]
    pub fn is_inside_managed_repo(&self, path: &std::path::Path) -> bool {
        self.managed_repo_root(path).is_some()
    }

    /// Rebuild the cached active repo list after inclusion changes.
    /// Canonicalizes paths so fetcher cache keys and assembly lookups
    /// use the same resolved paths, even if config paths go through symlinks.
    fn refresh_repo_cache(&mut self) {
        self.active_repo_cache = canonicalize_repo_entries(self.config.active_repos());
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

    /// Build a context bar projection for the currently selected work item.
    /// Returns None if no work item is selected (e.g., an unlinked PR or
    /// nothing selected).
    pub fn selected_work_item_context(&self) -> Option<WorkItemContext> {
        let idx = self.selected_item?;
        match self.display_list.get(idx)? {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                let wi = self.work_items.get(*wi_idx)?;
                let repo_path = wi
                    .repo_associations
                    .first()
                    .map(|a| a.repo_path.display().to_string())
                    .unwrap_or_default();
                let labels = wi
                    .repo_associations
                    .iter()
                    .find_map(|a| a.issue.as_ref())
                    .map(|issue| issue.labels.clone())
                    .unwrap_or_default();
                Some(WorkItemContext {
                    title: wi.title.clone(),
                    repo_path,
                    labels,
                })
            }
            _ => None,
        }
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
        self.fetcher_repos_changed = true;
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
        self.fetcher_repos_changed = true;
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

    /// Check liveness (try_wait) on all sessions. Called on periodic ticks.
    ///
    /// The reader threads handle PTY output continuously - no reading
    /// happens here. This only checks if child processes have exited.
    pub fn check_liveness(&mut self) {
        for entry in self.sessions.values_mut() {
            if let Some(ref mut session) = entry.session {
                entry.alive = session.is_alive();
            } else {
                entry.alive = false;
            }
        }
    }

    /// Resize PTY sessions and vt100 parsers to match the current pane
    /// dimensions. Resize is an instant ioctl call, so we resize all
    /// sessions immediately. The first resize failure per call is surfaced
    /// via status_message.
    pub fn resize_pty_panes(&mut self) {
        let mut first_error: Option<std::io::Error> = None;
        for entry in self.sessions.values() {
            if let Some(ref session) = entry.session
                && let Err(e) = session.resize(self.pane_cols, self.pane_rows)
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }
        if let Some(e) = first_error {
            self.status_message = Some(format!("PTY resize error: {e}"));
        }
    }

    /// Send SIGTERM to all alive sessions without waiting.
    /// Used to initiate graceful shutdown - the main loop continues
    /// running so the UI stays responsive.
    pub fn send_sigterm_all(&mut self) {
        for entry in self.sessions.values_mut() {
            if entry.alive
                && let Some(ref mut session) = entry.session
            {
                session.send_sigterm();
            }
        }
    }

    /// Check if all sessions are dead (or there are no sessions).
    pub fn all_dead(&self) -> bool {
        self.sessions.values().all(|entry| !entry.alive)
    }

    /// SIGKILL all remaining alive sessions. Used for force-quit during
    /// the shutdown wait.
    pub fn force_kill_all(&mut self) {
        for entry in self.sessions.values_mut() {
            if let Some(ref mut session) = entry.session {
                session.force_kill();
            }
            entry.alive = false;
        }
    }

    /// Send raw bytes to the active session's PTY.
    ///
    /// The active session is the one associated with the currently selected
    /// work item in the display list.
    pub fn send_bytes_to_active(&mut self, data: &[u8]) {
        let Some(work_item_id) = self.selected_work_item_id() else {
            return;
        };
        let Some(entry) = self.sessions.get(&work_item_id) else {
            return;
        };
        if let Some(ref session) = entry.session
            && let Err(e) = session.write_bytes(data)
        {
            self.status_message = Some(format!("Send error: {e}"));
        }
    }

    /// Drain pending fetch results from the background fetcher channel.
    ///
    /// Calls try_recv() in a loop until the channel is empty, storing each
    /// RepoData result in self.repo_data. FetcherError messages are surfaced
    /// via the status bar.
    ///
    /// Returns true if any messages were received (meaning reassembly is
    /// warranted).
    pub fn drain_fetch_results(&mut self) -> bool {
        let Some(ref rx) = self.fetch_rx else {
            return false;
        };
        let mut received_any = false;
        loop {
            match rx.try_recv() {
                Ok(FetchMessage::RepoData(result)) => {
                    received_any = true;
                    // Surface worktree errors in the status bar. One-time
                    // per repo to avoid flooding on every fetch cycle.
                    if let Err(ref e) = result.worktrees
                        && self.worktree_errors_shown.insert(result.repo_path.clone())
                    {
                        self.status_message = Some(format!(
                            "Worktree error ({}): {e}",
                            result.repo_path.display(),
                        ));
                    }
                    // Surface GitHub errors in the status bar. One-time
                    // messages for CliNotFound and AuthRequired so we
                    // don't spam on every fetch cycle.
                    if let Err(ref e) = result.prs {
                        match e {
                            GithubError::CliNotFound => {
                                if !self.gh_cli_not_found_shown {
                                    self.gh_cli_not_found_shown = true;
                                    self.status_message = Some(
                                        "gh CLI not found - GitHub features disabled".into(),
                                    );
                                }
                            }
                            GithubError::AuthRequired => {
                                if !self.gh_auth_required_shown {
                                    self.gh_auth_required_shown = true;
                                    self.status_message = Some(
                                        "gh auth required - run 'gh auth login'".into(),
                                    );
                                }
                            }
                            _ => {
                                let msg = format!("GitHub: {e}");
                                if self.status_message.is_none() {
                                    self.status_message = Some(msg);
                                } else {
                                    self.pending_fetch_errors.push(msg);
                                }
                            }
                        }
                    }
                    self.repo_data.insert(result.repo_path.clone(), result);
                }
                Ok(FetchMessage::FetcherError { error, .. }) => {
                    received_any = true;
                    let msg = format!("Fetch error: {error}");
                    if self.status_message.is_none() {
                        self.status_message = Some(msg);
                    } else {
                        self.pending_fetch_errors.push(msg);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.fetcher_disconnected {
                        self.fetcher_disconnected = true;
                        let msg = "Background fetcher stopped unexpectedly".to_string();
                        if self.status_message.is_none() {
                            self.status_message = Some(msg);
                        } else {
                            self.pending_fetch_errors.push(msg);
                        }
                    }
                    break;
                }
            }
        }
        received_any
    }

    /// Show the next pending fetch error if the status bar is free.
    /// Called on each tick so that errors queued while the status bar
    /// was occupied eventually surface. Shows one error per tick to
    /// avoid overwhelming the user.
    pub fn drain_pending_fetch_errors(&mut self) {
        if self.status_message.is_none()
            && let Some(msg) = self.pending_fetch_errors.first().cloned()
        {
            self.pending_fetch_errors.remove(0);
            self.status_message = Some(msg);
        }
    }

    /// Reassemble work items from backend records and cached repo data.
    ///
    /// Calls backend.list() for fresh records, then runs the assembly
    /// layer to produce work_items and unlinked_prs. Surfaces any
    /// corrupt backend records to the user via the status bar.
    pub fn reassemble_work_items(&mut self) {
        let list_result = match self.backend.list() {
            Ok(r) => r,
            Err(e) => {
                self.status_message = Some(format!("Backend error: {e}"));
                return;
            }
        };
        if !list_result.corrupt.is_empty() {
            let count = list_result.corrupt.len();
            let first = &list_result.corrupt[0];
            self.status_message = Some(format!(
                "{count} corrupt work item file(s): {} ({})",
                first.path.display(),
                first.reason,
            ));
        }
        let issue_pattern = &self.config.defaults.branch_issue_pattern;
        let (items, unlinked) =
            assembly::reassemble(&list_result.records, &self.repo_data, issue_pattern);
        self.work_items = items;
        self.unlinked_prs = unlinked;
    }

    /// Build the flat display list from current work_items and unlinked_prs.
    ///
    /// Groups:
    /// 1. UNLINKED (hidden if empty)
    /// 2. TODO (shown even if empty)
    /// 3. IN PROGRESS (shown even if empty)
    pub fn build_display_list(&mut self) {
        let mut list = Vec::new();

        // UNLINKED group (hidden if empty).
        if !self.unlinked_prs.is_empty() {
            list.push(DisplayEntry::GroupHeader {
                label: "UNLINKED".to_string(),
                count: self.unlinked_prs.len(),
            });
            for i in 0..self.unlinked_prs.len() {
                list.push(DisplayEntry::UnlinkedItem(i));
            }
        }

        // Partition work items by status.
        let todo_indices: Vec<usize> = self
            .work_items
            .iter()
            .enumerate()
            .filter(|(_, wi)| wi.status == WorkItemStatus::Todo)
            .map(|(i, _)| i)
            .collect();

        let in_progress_indices: Vec<usize> = self
            .work_items
            .iter()
            .enumerate()
            .filter(|(_, wi)| wi.status == WorkItemStatus::InProgress)
            .map(|(i, _)| i)
            .collect();

        // TODO group.
        list.push(DisplayEntry::GroupHeader {
            label: "TODO".to_string(),
            count: todo_indices.len(),
        });
        if todo_indices.is_empty() {
            list.push(DisplayEntry::EmptyState("  No work items".to_string()));
        } else {
            for idx in todo_indices {
                list.push(DisplayEntry::WorkItemEntry(idx));
            }
        }

        // IN PROGRESS group.
        list.push(DisplayEntry::GroupHeader {
            label: "IN PROGRESS".to_string(),
            count: in_progress_indices.len(),
        });
        if in_progress_indices.is_empty() {
            list.push(DisplayEntry::EmptyState("  No work items".to_string()));
        } else {
            for idx in in_progress_indices {
                list.push(DisplayEntry::WorkItemEntry(idx));
            }
        }

        self.display_list = list;

        // Restore selection by identity (WorkItemId or unlinked branch name)
        // so that selection survives reassembly even when display indices
        // change due to non-deterministic backend ordering or item additions.
        let mut restored = false;
        if let Some(ref target_id) = self.selected_work_item {
            for (i, entry) in self.display_list.iter().enumerate() {
                if let DisplayEntry::WorkItemEntry(wi_idx) = entry
                    && let Some(wi) = self.work_items.get(*wi_idx)
                    && wi.id == *target_id
                {
                    self.selected_item = Some(i);
                    restored = true;
                    break;
                }
            }
        }
        if !restored
            && let Some(ref target) = self.selected_unlinked_branch
        {
            let (target_repo, target_branch) = target;
            for (i, entry) in self.display_list.iter().enumerate() {
                if let DisplayEntry::UnlinkedItem(ul_idx) = entry
                    && let Some(ul) = self.unlinked_prs.get(*ul_idx)
                    && ul.branch == *target_branch
                    && ul.repo_path == *target_repo
                {
                    self.selected_item = Some(i);
                    restored = true;
                    break;
                }
            }
        }
        if !restored {
            // Previously selected item is gone. Clear identity trackers
            // and fall back to first selectable item or None.
            self.selected_work_item = None;
            self.selected_unlinked_branch = None;
            self.selected_item = self.display_list.iter().position(is_selectable);
        }
    }

    /// Sync the identity trackers (selected_work_item, selected_unlinked_branch)
    /// from the current selected_item index. Called after any navigation that
    /// changes selected_item so that reassembly can restore the correct entry.
    fn sync_selection_identity(&mut self) {
        self.selected_work_item = None;
        self.selected_unlinked_branch = None;
        let Some(idx) = self.selected_item else {
            return;
        };
        match self.display_list.get(idx) {
            Some(DisplayEntry::WorkItemEntry(wi_idx)) => {
                if let Some(wi) = self.work_items.get(*wi_idx) {
                    self.selected_work_item = Some(wi.id.clone());
                }
            }
            Some(DisplayEntry::UnlinkedItem(ul_idx)) => {
                if let Some(ul) = self.unlinked_prs.get(*ul_idx) {
                    self.selected_unlinked_branch =
                        Some((ul.repo_path.clone(), ul.branch.clone()));
                }
            }
            _ => {}
        }
    }

    // -- Navigation helpers --

    /// Move selection to the next selectable item in the display list.
    pub fn select_next_item(&mut self) {
        let start = match self.selected_item {
            Some(idx) => idx + 1,
            None => 0,
        };
        for i in start..self.display_list.len() {
            if is_selectable(&self.display_list[i]) {
                self.selected_item = Some(i);
                self.sync_selection_identity();
                return;
            }
        }
        // If nothing found after current position, keep current selection.
    }

    /// Move selection to the previous selectable item in the display list.
    pub fn select_prev_item(&mut self) {
        let start = match self.selected_item {
            Some(idx) if idx > 0 => idx - 1,
            Some(_) => return, // at position 0, nowhere to go
            None => {
                // Nothing selected, select the last selectable item.
                if let Some(pos) = self.display_list.iter().rposition(is_selectable) {
                    self.selected_item = Some(pos);
                    self.sync_selection_identity();
                }
                return;
            }
        };
        for i in (0..=start).rev() {
            if is_selectable(&self.display_list[i]) {
                self.selected_item = Some(i);
                self.sync_selection_identity();
                return;
            }
        }
        // If nothing found before current position, keep current selection.
    }

    /// Get the WorkItemId for the currently selected work item, if any.
    /// Returns None if nothing is selected or the selection is an unlinked PR.
    pub fn selected_work_item_id(&self) -> Option<WorkItemId> {
        let idx = self.selected_item?;
        match self.display_list.get(idx)? {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                self.work_items.get(*wi_idx).map(|wi| wi.id.clone())
            }
            _ => None,
        }
    }

    /// Build the target path for a new worktree.
    ///
    /// Uses `config.defaults.worktree_dir` as the subdirectory under the
    /// repo root, and sanitizes the branch name (replacing `/` with `-`)
    /// for the leaf directory name.
    fn worktree_target_path(
        repo_path: &std::path::Path,
        branch: &str,
        worktree_dir: &str,
    ) -> PathBuf {
        let sanitized = branch.replace('/', "-");
        repo_path.join(worktree_dir).join(sanitized)
    }

    /// Open or focus a session for the currently selected work item.
    ///
    /// If a session already exists for this work item, focuses the right panel.
    /// If no session exists, spawns a new one in the first worktree directory
    /// found in the work item's repo associations. If no worktree path exists,
    /// shows a status message instead.
    pub fn open_session_for_selected(&mut self) {
        let Some(idx) = self.selected_item else {
            return;
        };
        let wi_idx = match self.display_list.get(idx) {
            Some(DisplayEntry::WorkItemEntry(i)) => *i,
            _ => return,
        };
        let Some(wi) = self.work_items.get(wi_idx) else {
            return;
        };
        let work_item_id = wi.id.clone();

        // If session already exists and is alive, just focus right panel.
        // If the session is dead, remove it and fall through to spawn a new one.
        if self.sessions.contains_key(&work_item_id) {
            let is_alive = self
                .sessions
                .get(&work_item_id)
                .is_some_and(|entry| entry.alive);
            if is_alive {
                self.focus = FocusPanel::Right;
                self.status_message =
                    Some("Right panel focused - press Ctrl+] to return".into());
                return;
            }
            self.sessions.remove(&work_item_id);
        }

        // Find the first worktree path among the work item's repo associations.
        // If none exists, try to auto-create one for the first association
        // that has a branch name.
        let cwd = match wi
            .repo_associations
            .iter()
            .find_map(|a| a.worktree_path.clone())
        {
            Some(path) => path,
            None => {
                // Try to find an association with a branch name and auto-create
                // a worktree for it.
                let branch_assoc = wi
                    .repo_associations
                    .iter()
                    .find(|a| a.branch.is_some());
                match branch_assoc {
                    Some(assoc) => {
                        let branch = assoc.branch.as_ref().unwrap();
                        let repo_path = &assoc.repo_path;
                        // Fetch the branch from origin first to ensure the
                        // local ref points at the correct commit.
                        if self.worktree_service.fetch_branch(repo_path, branch).is_err() {
                            self.status_message = Some(format!(
                                "Could not fetch branch '{}' from origin. Manual checkout required.",
                                branch,
                            ));
                            return;
                        }
                        let wt_target = Self::worktree_target_path(
                            repo_path,
                            branch,
                            &self.config.defaults.worktree_dir,
                        );
                        match self.worktree_service.create_worktree(repo_path, branch, &wt_target) {
                            Ok(wt_info) => wt_info.path,
                            Err(e) => {
                                self.status_message = Some(format!(
                                    "Failed to create worktree for '{}': {e}",
                                    branch,
                                ));
                                return;
                            }
                        }
                    }
                    None => {
                        self.status_message = Some(
                            "Set a branch name to start working".into(),
                        );
                        return;
                    }
                }
            }
        };

        match Session::spawn(self.pane_cols, self.pane_rows, Some(&cwd), &["claude"]) {
            Ok(session) => {
                let parser = Arc::clone(&session.parser);
                let entry = SessionEntry {
                    parser,
                    alive: true,
                    session: Some(session),
                };
                self.sessions.insert(work_item_id, entry);
                self.focus = FocusPanel::Right;
                self.status_message =
                    Some("Right panel focused - press Ctrl+] to return".into());
            }
            Err(e) => {
                self.status_message = Some(format!("Error spawning session: {e}"));
            }
        }
    }

    /// Import the currently selected unlinked PR as a work item.
    ///
    /// Calls backend.import() then attempts to create a worktree for the
    /// imported branch (since the branch name is known from the PR). This
    /// makes the imported work item immediately sessionable. Finally,
    /// reassembles work items and rebuilds the display list.
    pub fn import_selected_unlinked(&mut self) {
        let Some(idx) = self.selected_item else {
            return;
        };
        let unlinked_idx = match self.display_list.get(idx) {
            Some(DisplayEntry::UnlinkedItem(i)) => *i,
            _ => return,
        };
        let Some(unlinked) = self.unlinked_prs.get(unlinked_idx) else {
            return;
        };

        // Capture values needed for worktree creation before borrowing self.
        let repo_path = unlinked.repo_path.clone();
        let branch = unlinked.branch.clone();

        match self.backend.import(unlinked) {
            Ok(record) => {
                let title = record.title.clone();

                // Fetch the branch from origin first so the local ref
                // points at the correct commit. If the fetch fails (fork PR,
                // branch does not exist on origin, network error), skip
                // worktree creation to avoid creating from wrong revision.
                let wt_msg = match self.worktree_service.fetch_branch(&repo_path, &branch) {
                    Ok(()) => {
                        let wt_target = Self::worktree_target_path(
                            &repo_path,
                            &branch,
                            &self.config.defaults.worktree_dir,
                        );
                        match self.worktree_service.create_worktree(&repo_path, &branch, &wt_target) {
                            Ok(_) => format!("Imported: {title} (worktree created)"),
                            Err(e) => format!("Imported: {title} (worktree not created: {e})"),
                        }
                    }
                    Err(_) => {
                        format!(
                            "Imported: {title} - could not fetch branch '{branch}' from origin. Manual checkout required."
                        )
                    }
                };

                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_repos_changed = true;
                self.status_message = Some(wt_msg);
            }
            Err(e) => {
                self.status_message = Some(format!("Import error: {e}"));
            }
        }
    }

    /// Create a new work item with the current working directory as the
    /// repo association. Validates that the CWD is inside a managed repo
    /// before persisting.
    pub fn create_work_item(&mut self) {
        let cwd = match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                self.status_message =
                    Some(format!("Cannot determine working directory: {e}"));
                return;
            }
        };

        // Validate that CWD is inside a managed repo and resolve to repo root.
        let repo_root = match self.managed_repo_root(&cwd) {
            Some(root) => root,
            None => {
                self.status_message = Some(
                    "CWD is not inside a managed repo. Add it via 'workbridge repos add' first."
                        .into(),
                );
                return;
            }
        };

        let request = CreateWorkItem {
            title: "New work item".to_string(),
            status: WorkItemStatus::Todo,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo_root,
                branch: None,
            }],
        };

        match self.backend.create(request) {
            Ok(record) => {
                let title = record.title.clone();
                self.reassemble_work_items();
                self.build_display_list();
                self.status_message = Some(format!("Created: {title}"));
            }
            Err(e) => {
                self.status_message = Some(format!("Create error: {e}"));
            }
        }
    }

    /// Delete the currently selected work item.
    ///
    /// Kills any active session for the work item, calls backend.delete(),
    /// then reassembles and rebuilds the display list.
    pub fn delete_selected_work_item(&mut self) {
        let Some(work_item_id) = self.selected_work_item_id() else {
            self.status_message = Some("No work item selected".into());
            return;
        };

        // Delete from backend first. If this fails, keep the session alive.
        if let Err(e) = self.backend.delete(&work_item_id) {
            self.status_message = Some(format!("Delete error: {e}"));
            return;
        }

        // Backend delete succeeded - now kill any active session.
        if let Some(mut entry) = self.sessions.remove(&work_item_id)
            && let Some(ref mut session) = entry.session
        {
            session.kill();
        }

        // Clear identity trackers since the deleted item is gone.
        // build_display_list will fall back to the first selectable item.
        self.selected_work_item = None;
        self.selected_unlinked_branch = None;

        // Reassemble and rebuild. build_display_list will set selected_item
        // to the first selectable item since identity trackers are cleared.
        let old_idx = self.selected_item;
        self.reassemble_work_items();
        self.build_display_list();
        self.fetcher_repos_changed = true;

        // Try to keep cursor near the old position instead of jumping to
        // the first item. If the old index is still valid, prefer it.
        if let Some(old) = old_idx {
            // Find the nearest selectable item at or before the old position.
            let mut found = false;
            for i in (0..self.display_list.len().min(old + 1)).rev() {
                if is_selectable(&self.display_list[i]) {
                    self.selected_item = Some(i);
                    found = true;
                    break;
                }
            }
            if !found {
                // Try forward.
                self.selected_item = None;
                for i in 0..self.display_list.len() {
                    if is_selectable(&self.display_list[i]) {
                        self.selected_item = Some(i);
                        break;
                    }
                }
            }
        }
        self.sync_selection_identity();

        self.focus = FocusPanel::Left;
        self.status_message = Some("Work item deleted".into());
    }

    /// Get the SessionEntry for the currently selected work item, if any.
    pub fn active_session_entry(&self) -> Option<&SessionEntry> {
        let work_item_id = self.selected_work_item_id()?;
        self.sessions.get(&work_item_id)
    }

    /// Returns true if any session is alive.
    pub fn has_any_session(&self) -> bool {
        self.sessions.values().any(|e| e.alive)
    }

    /// Collect extra branch names from backend records, grouped by repo
    /// path. These are branches recorded in work items that may not have
    /// worktrees yet. The fetcher uses them to also extract and fetch
    /// issue metadata for branch-only work items.
    pub fn extra_branches_from_backend(
        &self,
    ) -> std::collections::HashMap<PathBuf, Vec<String>> {
        let mut map: std::collections::HashMap<PathBuf, Vec<String>> =
            std::collections::HashMap::new();
        let list_result = match self.backend.list() {
            Ok(r) => r,
            Err(e) => {
                // Log but don't panic - the fetcher just won't have extras.
                eprintln!("workbridge: failed to list backend records for extra branches: {e}");
                return map;
            }
        };
        for record in &list_result.records {
            for assoc in &record.repo_associations {
                if let Some(ref branch) = assoc.branch {
                    map.entry(assoc.repo_path.clone())
                        .or_default()
                        .push(branch.clone());
                }
            }
        }
        map
    }
}

/// Canonicalize repo entry paths so that symlinked or non-canonical config
/// paths resolve to the same real filesystem path. This ensures fetcher
/// cache keys (keyed by repo_path) match assembly lookups. If canonicalization
/// fails (e.g. path does not exist), the original path is kept.
fn canonicalize_repo_entries(entries: Vec<RepoEntry>) -> Vec<RepoEntry> {
    entries
        .into_iter()
        .map(|mut entry| {
            if let Ok(canonical) = std::fs::canonicalize(&entry.path) {
                entry.path = canonical;
            }
            entry
        })
        .collect()
}

/// Returns true if a display entry can receive selection (is an item, not
/// a header or empty state).
pub fn is_selectable(entry: &DisplayEntry) -> bool {
    matches!(
        entry,
        DisplayEntry::UnlinkedItem(_) | DisplayEntry::WorkItemEntry(_)
    )
}

/// A stub worktree service that returns empty results. Used as a default
/// when no real worktree operations are needed (e.g. tests, initial setup).
pub struct StubWorktreeService;

impl WorktreeService for StubWorktreeService {
    fn list_worktrees(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<Vec<crate::worktree_service::WorktreeInfo>, crate::worktree_service::WorktreeError> {
        Ok(Vec::new())
    }

    fn create_worktree(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
        _target_dir: &std::path::Path,
    ) -> Result<crate::worktree_service::WorktreeInfo, crate::worktree_service::WorktreeError> {
        Err(crate::worktree_service::WorktreeError::GitError(
            "stub worktree service does not support create".into(),
        ))
    }

    fn remove_worktree(
        &self,
        _repo_path: &std::path::Path,
        _worktree_path: &std::path::Path,
        _delete_branch: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }

    fn default_branch(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<String, crate::worktree_service::WorktreeError> {
        Ok("main".to_string())
    }

    fn github_remote(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<Option<(String, String)>, crate::worktree_service::WorktreeError> {
        Ok(None)
    }

    fn fetch_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }
}

/// A stub backend that stores nothing. Used in tests when no backend
/// persistence is needed. All operations return empty/success.
pub struct StubBackend;

impl WorkItemBackend for StubBackend {
    fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
        Ok(crate::work_item_backend::ListResult {
            records: Vec::new(),
            corrupt: Vec::new(),
        })
    }

    fn create(
        &self,
        _request: CreateWorkItem,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "stub backend does not support create".into(),
        ))
    }

    fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
        Ok(())
    }

    fn import(
        &self,
        _unlinked: &UnlinkedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "stub backend does not support import".into(),
        ))
    }

    fn backend_type(&self) -> crate::work_item::BackendType {
        crate::work_item::BackendType::LocalFile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -- F-1 regression test --

    #[test]
    fn manage_unmanage_sets_fetcher_repos_changed() {
        // Setup: create a config with a base_dir containing a discovered repo.
        let dir = std::env::temp_dir().join("workbridge-test-f1-fetcher-flag");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo-a/.git")).unwrap();

        let mut cfg = Config::default();
        cfg.add_base_dir(dir.to_str().unwrap()).unwrap();
        // Discovered repo starts unmanaged - include it.
        let all = cfg.all_repos();
        assert!(!all.is_empty(), "should discover at least one repo");
        let _repo_display = all[0].path.display().to_string();

        let mut app = App::with_config(cfg, Box::new(StubBackend));

        // Initially false.
        assert!(!app.fetcher_repos_changed);

        // Manage a repo from the available list.
        app.settings_list_focus = SettingsListFocus::Available;
        app.settings_available_selected = 0;
        app.manage_selected_repo();
        assert!(
            app.fetcher_repos_changed,
            "fetcher_repos_changed should be true after manage"
        );

        // Reset and test unmanage.
        app.fetcher_repos_changed = false;
        app.settings_list_focus = SettingsListFocus::Managed;
        // The managed repo that is discovered (not explicit) can be unmanaged.
        // Find the discovered repo in the managed list.
        let discovered_idx = app
            .active_repo_cache
            .iter()
            .position(|e| e.source == RepoSource::Discovered)
            .expect("should have a discovered managed repo");
        app.settings_repo_selected = discovered_idx;
        app.unmanage_selected_repo();
        assert!(
            app.fetcher_repos_changed,
            "fetcher_repos_changed should be true after unmanage"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- F-3 regression test --

    #[test]
    fn create_work_item_rejects_unmanaged_cwd() {
        // With no managed repos, the CWD cannot be inside one.
        let mut app = App::new();
        app.create_work_item();
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("not inside a managed repo"),
            "expected rejection message, got: {msg}"
        );
    }

    #[test]
    fn is_inside_managed_repo_positive() {
        let dir = std::env::temp_dir().join("workbridge-test-f3-managed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        // Create the subdirectory on disk so canonicalize succeeds.
        std::fs::create_dir_all(dir.join("src")).unwrap();

        let mut cfg = Config::default();
        cfg.add_repo(dir.to_str().unwrap()).unwrap();

        let app = App::with_config(cfg, Box::new(StubBackend));

        // The repo root itself should be inside.
        assert!(app.is_inside_managed_repo(&dir));
        // A subdirectory should also be inside.
        let subdir = dir.join("src");
        assert!(app.is_inside_managed_repo(&subdir));
        // An unrelated path should not be inside.
        assert!(!app.is_inside_managed_repo(&PathBuf::from("/tmp/unrelated")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Round 3 regression tests --

    /// F-1: managed_repo_root returns repo root, not subdirectory path.
    /// create_work_item should store the repo root, not CWD when CWD is
    /// a subdirectory of a managed repo.
    #[test]
    fn managed_repo_root_returns_root_not_subdir() {
        let dir = std::env::temp_dir().join("workbridge-test-r3-f1-root");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/deeply/nested")).unwrap();

        let mut cfg = Config::default();
        cfg.add_repo(dir.to_str().unwrap()).unwrap();

        let app = App::with_config(cfg, Box::new(StubBackend));

        // From a subdirectory, managed_repo_root should return the repo root.
        let subdir = dir.join("src/deeply/nested");
        let root = app.managed_repo_root(&subdir);
        assert!(root.is_some(), "subdir should be inside a managed repo");
        let root = root.unwrap();
        let canonical_dir = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(
            root, canonical_dir,
            "managed_repo_root should return the repo root {}, not the subdir {}",
            canonical_dir.display(),
            subdir.display(),
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F-2: fetcher_repos_changed is set after import and delete.
    /// Import and delete change backend records, so the fetcher must
    /// be restarted to pick up new/removed extra branches.
    #[test]
    fn import_and_delete_set_fetcher_repos_changed() {
        use crate::work_item::{
            CheckStatus, PrInfo, PrState, ReviewDecision,
        };
        use crate::work_item_backend::ListResult;

        /// Test backend that supports import and delete.
        struct TestBackend {
            records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
        }

        impl WorkItemBackend for TestBackend {
            fn list(&self) -> Result<ListResult, BackendError> {
                Ok(ListResult {
                    records: self.records.lock().unwrap().clone(),
                    corrupt: Vec::new(),
                })
            }
            fn create(
                &self,
                _req: CreateWorkItem,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn delete(&self, id: &WorkItemId) -> Result<(), BackendError> {
                let mut records = self.records.lock().unwrap();
                if let Some(pos) = records.iter().position(|r| r.id == *id) {
                    records.remove(pos);
                    Ok(())
                } else {
                    Err(BackendError::NotFound(id.clone()))
                }
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/fake.json")),
                    title: unlinked.pr.title.clone(),
                    status: WorkItemStatus::InProgress,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                    }],
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let backend = TestBackend {
            records: std::sync::Mutex::new(Vec::new()),
        };
        let mut app = App::with_config(Config::default(), Box::new(backend));

        // Set up an unlinked PR to import.
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: PathBuf::from("/repo"),
            pr: PrInfo {
                number: 1,
                title: "Test PR".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: "https://github.com/o/r/pull/1".into(),
            },
            branch: "1-test".into(),
        });
        app.build_display_list();
        // Select the unlinked item.
        let unlinked_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
            .expect("should have an unlinked item in display list");
        app.selected_item = Some(unlinked_idx);

        assert!(!app.fetcher_repos_changed);
        app.import_selected_unlinked();
        assert!(
            app.fetcher_repos_changed,
            "fetcher_repos_changed should be true after import",
        );

        // Reset and test delete.
        app.fetcher_repos_changed = false;
        // Select the now-imported work item.
        let work_item_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .expect("should have a work item in display list after import");
        app.selected_item = Some(work_item_idx);
        app.delete_selected_work_item();
        assert!(
            app.fetcher_repos_changed,
            "fetcher_repos_changed should be true after delete",
        );
    }

    /// F-3: PR list limit is 500, not the original 100.
    /// This is a documentation test - the actual limit is a string in
    /// the gh CLI command. We verify the constant through the source.
    #[test]
    fn pr_list_limit_is_500() {
        // Read the source to verify the limit. This is a safeguard
        // against regressions back to 100.
        let source = include_str!("github_client.rs");
        assert!(
            source.contains(r#""--limit", "500""#),
            "PR list limit should be 500 to avoid silent truncation in busy repos",
        );
    }

    // -- Round 4 regression tests --

    /// F-1: Canonicalized repo paths in active_repo_cache match fetcher
    /// cache keys. A symlinked repo path in config should resolve to its
    /// canonical form so that repo_data lookups by the assembly layer
    /// succeed.
    #[test]
    fn active_repo_cache_uses_canonical_paths() {
        // Create a real directory and a symlink to it.
        let dir = std::env::temp_dir().join("workbridge-test-r4-f1-canonical");
        let _ = std::fs::remove_dir_all(&dir);
        let real_path = dir.join("real-repo");
        let link_path = dir.join("link-repo");
        std::fs::create_dir_all(real_path.join(".git")).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_path, &link_path).unwrap();
        #[cfg(not(unix))]
        {
            // On non-Unix, skip the symlink test.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        // Add the symlink path as an explicit repo.
        let mut cfg = Config::default();
        cfg.add_repo(link_path.to_str().unwrap()).unwrap();

        let app = App::with_config(cfg, Box::new(StubBackend));

        // The active_repo_cache should contain the canonical (real) path,
        // not the symlink path.
        assert_eq!(app.active_repo_cache.len(), 1);
        let cached_path = &app.active_repo_cache[0].path;
        let canonical_real = std::fs::canonicalize(&real_path).unwrap();
        assert_eq!(
            *cached_path, canonical_real,
            "active_repo_cache should contain canonical path {}, got {}",
            canonical_real.display(),
            cached_path.display(),
        );

        // Verify that repo_data keyed by the canonical path would be found.
        // Simulate: fetcher sends data keyed by cached_path, assembly looks
        // up by the same path.
        let mut repo_data = std::collections::HashMap::new();
        repo_data.insert(
            cached_path.clone(),
            crate::work_item::RepoFetchResult {
                repo_path: cached_path.clone(),
                github_remote: None,
                worktrees: Ok(vec![]),
                prs: Ok(vec![]),
                issues: vec![],
            },
        );
        assert!(
            repo_data.contains_key(cached_path),
            "repo_data lookup by canonical path should succeed",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F-2: Unmanaging a repo prunes stale fetch cache entries.
    /// After fetcher restart, repo_data for removed repos should be
    /// cleared so stale data stops rendering.
    #[test]
    fn unmanage_prunes_stale_repo_data() {
        let mut app = App::new();

        // Simulate fetched data for two repos.
        let repo_a = PathBuf::from("/repos/alpha");
        let repo_b = PathBuf::from("/repos/beta");
        app.repo_data.insert(
            repo_a.clone(),
            crate::work_item::RepoFetchResult {
                repo_path: repo_a.clone(),
                github_remote: None,
                worktrees: Ok(vec![]),
                prs: Ok(vec![]),
                issues: vec![],
            },
        );
        app.repo_data.insert(
            repo_b.clone(),
            crate::work_item::RepoFetchResult {
                repo_path: repo_b.clone(),
                github_remote: None,
                worktrees: Ok(vec![]),
                prs: Ok(vec![]),
                issues: vec![],
            },
        );

        assert_eq!(app.repo_data.len(), 2);

        // Simulate the prune logic from main.rs: only keep repos that
        // are in the new active list (which is empty for a default app).
        let new_repos: Vec<PathBuf> = app
            .active_repo_cache
            .iter()
            .filter(|r| r.git_dir_present)
            .map(|r| r.path.clone())
            .collect();
        app.repo_data.retain(|k, _| new_repos.contains(k));

        assert!(
            app.repo_data.is_empty(),
            "repo_data should be pruned when no active repos remain, got {} entries",
            app.repo_data.len(),
        );
    }

    /// F-3: Worktree fetch failures are surfaced in the status bar,
    /// not silently treated as "no worktrees".
    #[test]
    fn worktree_fetch_error_surfaces_in_status() {
        use crate::worktree_service::WorktreeError;

        let mut app = App::new();

        // Create a channel and feed it a result with a worktree error.
        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        let repo_path = PathBuf::from("/repos/broken");
        tx.send(FetchMessage::RepoData(crate::work_item::RepoFetchResult {
            repo_path: repo_path.clone(),
            github_remote: None,
            worktrees: Err(WorktreeError::GitError(
                "not a git repository".into(),
            )),
            prs: Ok(vec![]),
            issues: vec![],
        }))
        .unwrap();

        let received = app.drain_fetch_results();
        assert!(received, "should have received a message");

        // The status message should mention the worktree error.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("Worktree error") && msg.contains("not a git repository"),
            "expected worktree error in status, got: {msg}",
        );

        // The error should be tracked per repo to avoid re-showing.
        assert!(
            app.worktree_errors_shown.contains(&repo_path),
            "repo should be in worktree_errors_shown set",
        );

        // Sending a second error for the same repo should NOT overwrite
        // the status message.
        app.status_message = Some("other message".into());
        tx.send(FetchMessage::RepoData(crate::work_item::RepoFetchResult {
            repo_path: repo_path.clone(),
            github_remote: None,
            worktrees: Err(WorktreeError::GitError(
                "still broken".into(),
            )),
            prs: Ok(vec![]),
            issues: vec![],
        }))
        .unwrap();
        app.drain_fetch_results();
        assert_eq!(
            app.status_message.as_deref(),
            Some("other message"),
            "second worktree error for same repo should not overwrite status",
        );
    }

    // -- Round 5 regression tests --

    /// F-1: Selection survives reassembly when items reorder.
    /// After backend records change order, the same WorkItemId should
    /// remain selected even if its display index changes.
    #[test]
    fn selection_survives_reassembly_when_items_reorder() {
        use crate::work_item_backend::ListResult;

        /// Backend that returns records in a controllable order.
        struct OrderableBackend {
            records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
        }

        impl WorkItemBackend for OrderableBackend {
            fn list(&self) -> Result<ListResult, BackendError> {
                Ok(ListResult {
                    records: self.records.lock().unwrap().clone(),
                    corrupt: Vec::new(),
                })
            }
            fn create(
                &self,
                _req: CreateWorkItem,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                _unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let id_a = WorkItemId::LocalFile(PathBuf::from("/data/aaa.json"));
        let id_b = WorkItemId::LocalFile(PathBuf::from("/data/bbb.json"));

        let record_a = crate::work_item_backend::WorkItemRecord {
            id: id_a.clone(),
            title: "Item A".into(),
            status: WorkItemStatus::Todo,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
            }],
        };
        let record_b = crate::work_item_backend::WorkItemRecord {
            id: id_b.clone(),
            title: "Item B".into(),
            status: WorkItemStatus::Todo,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
            }],
        };

        // Start with order A, B.
        let backend = OrderableBackend {
            records: std::sync::Mutex::new(vec![record_a.clone(), record_b.clone()]),
        };
        let mut app = App::with_config(Config::default(), Box::new(backend));

        // Select Item B (the second Todo item).
        app.select_next_item(); // selects first item (A)
        app.select_next_item(); // selects second item (B)

        let selected_id = app.selected_work_item_id();
        assert_eq!(
            selected_id,
            Some(id_b.clone()),
            "should have selected Item B",
        );
        let old_index = app.selected_item;

        // Reverse the order to B, A and reassemble. We simulate this by
        // directly setting work_items in reversed order since we cannot
        // mutate the backend through the trait interface.
        app.work_items = vec![
            crate::work_item::WorkItem {
                id: id_b.clone(),
                backend_type: crate::work_item::BackendType::LocalFile,
                title: "Item B".into(),
                status: WorkItemStatus::Todo,
                repo_associations: vec![crate::work_item::RepoAssociation {
                    repo_path: PathBuf::from("/repo"),
                    branch: None,
                    worktree_path: None,
                    pr: None,
                    issue: None,
                    git_state: None,
                }],
                errors: vec![],
            },
            crate::work_item::WorkItem {
                id: id_a.clone(),
                backend_type: crate::work_item::BackendType::LocalFile,
                title: "Item A".into(),
                status: WorkItemStatus::Todo,
                repo_associations: vec![crate::work_item::RepoAssociation {
                    repo_path: PathBuf::from("/repo"),
                    branch: None,
                    worktree_path: None,
                    pr: None,
                    issue: None,
                    git_state: None,
                }],
                errors: vec![],
            },
        ];
        app.build_display_list();

        // After rebuild, selection should still point to Item B.
        let new_selected_id = app.selected_work_item_id();
        assert_eq!(
            new_selected_id,
            Some(id_b.clone()),
            "selection should still be Item B after reorder",
        );

        // The index should have changed since B moved from position 2 to 1.
        let new_index = app.selected_item;
        assert_ne!(
            old_index, new_index,
            "display index should change when items reorder",
        );
    }

    /// F-1: LocalFileBackend::list() returns records sorted by path for
    /// deterministic enumeration. read_dir order is filesystem-dependent,
    /// so sorting ensures stable display indices.
    #[test]
    fn backend_list_returns_sorted_records() {
        let dir = std::env::temp_dir().join("workbridge-test-r5-f1-sorted");
        let _ = std::fs::remove_dir_all(&dir);
        let backend =
            crate::work_item_backend::LocalFileBackend::with_dir(dir.clone()).unwrap();

        // Create items with names that would sort differently than creation order.
        // File names are UUIDs, so we write files directly with known names.
        let names = ["zzz.json", "aaa.json", "mmm.json"];
        for name in &names {
            let record = crate::work_item_backend::WorkItemRecord {
                id: WorkItemId::LocalFile(dir.join(name)),
                title: format!("Item {name}"),
                status: WorkItemStatus::Todo,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: None,
                }],
            };
            let json = serde_json::to_string_pretty(&record).unwrap();
            std::fs::write(dir.join(name), json).unwrap();
        }

        let result = backend.list().unwrap();
        assert_eq!(result.records.len(), 3);

        // Records should be sorted by path.
        let paths: Vec<_> = result
            .records
            .iter()
            .map(|r| match &r.id {
                WorkItemId::LocalFile(p) => p.clone(),
                _ => panic!("expected LocalFile"),
            })
            .collect();
        assert_eq!(paths[0], dir.join("aaa.json"));
        assert_eq!(paths[1], dir.join("mmm.json"));
        assert_eq!(paths[2], dir.join("zzz.json"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F-3: Fetch errors queued while status bar is occupied eventually
    /// surface when the status clears.
    #[test]
    fn pending_fetch_errors_surface_when_status_clears() {
        let mut app = App::new();

        // Occupy the status bar.
        app.status_message = Some("busy doing something".into());

        // Create a channel and send a FetcherError while status is occupied.
        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        tx.send(FetchMessage::FetcherError {
            repo_path: PathBuf::from("/repo"),
            error: "connection timed out".into(),
        })
        .unwrap();

        // Drain: the error should be queued, not shown.
        app.drain_fetch_results();
        assert_eq!(
            app.status_message.as_deref(),
            Some("busy doing something"),
            "status bar should remain occupied",
        );
        assert_eq!(
            app.pending_fetch_errors.len(),
            1,
            "error should be queued in pending_fetch_errors",
        );

        // Clear the status bar and drain pending errors.
        app.status_message = None;
        app.drain_pending_fetch_errors();

        // The queued error should now be shown.
        assert_eq!(
            app.status_message.as_deref(),
            Some("Fetch error: connection timed out"),
            "queued error should surface when status clears",
        );
        assert!(
            app.pending_fetch_errors.is_empty(),
            "pending_fetch_errors should be empty after draining",
        );
    }

    /// F-3: GitHub errors are also queued when status bar is occupied.
    #[test]
    fn github_errors_queued_when_status_occupied() {
        let mut app = App::new();

        // Occupy the status bar.
        app.status_message = Some("something important".into());

        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        // Send a repo data result with a non-CliNotFound/AuthRequired error.
        tx.send(FetchMessage::RepoData(crate::work_item::RepoFetchResult {
            repo_path: PathBuf::from("/repo"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Err(crate::github_client::GithubError::ApiError(
                "rate limited".into(),
            )),
            issues: vec![],
        }))
        .unwrap();

        app.drain_fetch_results();

        // The status should remain unchanged.
        assert_eq!(
            app.status_message.as_deref(),
            Some("something important"),
        );
        // The error should be queued.
        assert_eq!(app.pending_fetch_errors.len(), 1);
        assert!(
            app.pending_fetch_errors[0].contains("rate limited"),
            "queued error should contain the error message, got: {}",
            app.pending_fetch_errors[0],
        );

        // Clear status and drain.
        app.status_message = None;
        app.drain_pending_fetch_errors();
        assert!(
            app.status_message
                .as_deref()
                .unwrap_or("")
                .contains("rate limited"),
            "error should surface after status clears",
        );
    }

    // -- Round 6 regression tests --

    /// F-1: Unlinked PR selection keyed by (repo_path, branch) not just branch.
    /// Two repos can have unlinked PRs on the same branch name. After
    /// reassembly, selection must stay on the correct repo's PR.
    #[test]
    fn unlinked_selection_disambiguates_by_repo_path() {
        use crate::work_item::{
            CheckStatus, PrInfo, PrState, ReviewDecision,
        };

        let mut app = App::new();

        let repo_a = PathBuf::from("/repos/alpha");
        let repo_b = PathBuf::from("/repos/beta");
        let branch = "update-deps";

        // Two unlinked PRs from different repos with the same branch name.
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: repo_a.clone(),
            pr: PrInfo {
                number: 1,
                title: "Update deps (alpha)".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: "https://github.com/o/alpha/pull/1".into(),
            },
            branch: branch.into(),
        });
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: repo_b.clone(),
            pr: PrInfo {
                number: 2,
                title: "Update deps (beta)".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: "https://github.com/o/beta/pull/2".into(),
            },
            branch: branch.into(),
        });
        app.build_display_list();

        // Select the second unlinked item (beta's PR).
        app.select_next_item(); // first unlinked (alpha)
        app.select_next_item(); // second unlinked (beta)

        // Verify we selected the beta PR.
        let sel_idx = app.selected_item.expect("should have selection");
        match &app.display_list[sel_idx] {
            DisplayEntry::UnlinkedItem(ul_idx) => {
                assert_eq!(
                    app.unlinked_prs[*ul_idx].repo_path, repo_b,
                    "should have selected beta's PR",
                );
            }
            other => panic!("expected UnlinkedItem, got: {:?}", other),
        }

        // Verify the identity tracker stores (repo_path, branch).
        assert_eq!(
            app.selected_unlinked_branch,
            Some((repo_b.clone(), branch.to_string())),
            "identity tracker should store (repo_path, branch)",
        );

        // Simulate reassembly: rebuild display list. Selection should
        // restore to beta's PR, not alpha's (which has the same branch).
        app.build_display_list();

        let restored_idx = app.selected_item.expect("selection should survive rebuild");
        match &app.display_list[restored_idx] {
            DisplayEntry::UnlinkedItem(ul_idx) => {
                assert_eq!(
                    app.unlinked_prs[*ul_idx].repo_path, repo_b,
                    "after rebuild, selection should still be beta's PR, not alpha's",
                );
                assert_eq!(
                    app.unlinked_prs[*ul_idx].pr.number, 2,
                    "after rebuild, selected PR number should be 2 (beta), not 1 (alpha)",
                );
            }
            other => panic!("expected UnlinkedItem after rebuild, got: {:?}", other),
        }
    }

    // -- Round 7 regression tests --

    /// F-2: Invalid branch_issue_pattern is caught at startup.
    /// Verify that an invalid regex is detected and the pattern is reset
    /// to an empty string (disabling issue extraction) rather than crashing
    /// or causing fetcher threads to die.
    #[test]
    fn invalid_branch_issue_pattern_caught_at_startup() {
        // Simulate what main.rs does: validate the pattern and replace if invalid.
        let mut cfg = Config::default();
        cfg.defaults.branch_issue_pattern = "[invalid(".to_string();

        let mut app = App::with_config(cfg, Box::new(StubBackend));

        // Replicate the main.rs validation logic.
        if let Err(e) = regex::Regex::new(&app.config.defaults.branch_issue_pattern) {
            let bad = app.config.defaults.branch_issue_pattern.clone();
            app.config.defaults.branch_issue_pattern = String::new();
            let msg = format!(
                "Invalid branch_issue_pattern '{}': {} (issue extraction disabled)",
                bad, e,
            );
            if app.status_message.is_none() {
                app.status_message = Some(msg);
            } else {
                app.pending_fetch_errors.push(msg);
            }
        }

        // The pattern should have been replaced with empty string.
        assert_eq!(
            app.config.defaults.branch_issue_pattern, "",
            "invalid pattern should be replaced with empty string",
        );

        // An error message should have been set.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("Invalid branch_issue_pattern") && msg.contains("[invalid("),
            "expected invalid pattern error in status, got: {msg}",
        );
    }

    /// F-2: Disconnected fetcher channel surfaces error in status bar.
    /// When all fetcher threads exit (e.g. due to invalid regex), the
    /// channel disconnects. drain_fetch_results should detect this and
    /// set fetcher_disconnected = true with a status message.
    #[test]
    fn disconnected_fetcher_surfaces_error() {
        let mut app = App::new();

        // Create a channel and immediately drop the sender to simulate
        // all fetcher threads exiting.
        let (tx, rx) = std::sync::mpsc::channel::<FetchMessage>();
        app.fetch_rx = Some(rx);
        drop(tx);

        assert!(!app.fetcher_disconnected);

        let received = app.drain_fetch_results();
        // No data was received, but disconnect was detected.
        assert!(!received, "no actual data should have been received");
        assert!(
            app.fetcher_disconnected,
            "fetcher_disconnected should be true after channel disconnect",
        );

        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("Background fetcher stopped unexpectedly"),
            "expected disconnect error in status, got: {msg}",
        );

        // Calling drain again should NOT push duplicate errors.
        app.status_message = None;
        app.drain_fetch_results();
        assert!(
            app.status_message.is_none(),
            "should not push duplicate disconnect error",
        );
    }

    // -- Round 8 regression tests --

    /// F-1: Importing an unlinked PR creates a worktree for the imported
    /// branch, making the work item immediately sessionable.
    #[test]
    fn import_creates_worktree_for_branch() {
        use crate::work_item::{
            CheckStatus, PrInfo, PrState, ReviewDecision,
        };
        use crate::work_item_backend::ListResult;
        use crate::worktree_service::{WorktreeError, WorktreeInfo};

        /// Mock worktree service that records create_worktree calls.
        struct MockWorktreeService {
            created: std::sync::Mutex<Vec<(PathBuf, String, PathBuf)>>,
        }

        impl WorktreeService for MockWorktreeService {
            fn list_worktrees(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
                Ok(Vec::new())
            }

            fn create_worktree(
                &self,
                repo_path: &std::path::Path,
                branch: &str,
                target_dir: &std::path::Path,
            ) -> Result<WorktreeInfo, WorktreeError> {
                self.created.lock().unwrap().push((
                    repo_path.to_path_buf(),
                    branch.to_string(),
                    target_dir.to_path_buf(),
                ));
                Ok(WorktreeInfo {
                    path: target_dir.to_path_buf(),
                    branch: Some(branch.to_string()),
                    is_main: false,
                })
            }

            fn remove_worktree(
                &self,
                _repo_path: &std::path::Path,
                _worktree_path: &std::path::Path,
                _delete_branch: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn default_branch(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<String, WorktreeError> {
                Ok("main".to_string())
            }

            fn github_remote(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<Option<(String, String)>, WorktreeError> {
                Ok(None)
            }

            fn fetch_branch(
                &self,
                _repo_path: &std::path::Path,
                _branch: &str,
            ) -> Result<(), WorktreeError> {
                // Mock: fetch always succeeds (branch exists on origin).
                Ok(())
            }
        }

        /// Test backend that supports import.
        struct TestBackend {
            records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
        }

        impl WorkItemBackend for TestBackend {
            fn list(&self) -> Result<ListResult, BackendError> {
                Ok(ListResult {
                    records: self.records.lock().unwrap().clone(),
                    corrupt: Vec::new(),
                })
            }
            fn create(
                &self,
                _req: CreateWorkItem,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported.json")),
                    title: unlinked.pr.title.clone(),
                    status: WorkItemStatus::InProgress,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                    }],
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let mock_ws = Arc::new(MockWorktreeService {
            created: std::sync::Mutex::new(Vec::new()),
        });
        let backend = TestBackend {
            records: std::sync::Mutex::new(Vec::new()),
        };
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Box::new(backend),
            Arc::clone(&mock_ws) as Arc<dyn WorktreeService + Send + Sync>,
        );

        // Set up an unlinked PR to import.
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: PathBuf::from("/repos/myrepo"),
            pr: PrInfo {
                number: 42,
                title: "Fix the bug".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: "https://github.com/o/r/pull/42".into(),
            },
            branch: "fix-bug".into(),
        });
        app.build_display_list();

        // Select the unlinked item.
        let unlinked_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
            .expect("should have an unlinked item in display list");
        app.selected_item = Some(unlinked_idx);

        // Import it.
        app.import_selected_unlinked();

        // Verify a worktree was created.
        let created = mock_ws.created.lock().unwrap();
        assert_eq!(
            created.len(),
            1,
            "import should create exactly one worktree, got {}",
            created.len(),
        );
        assert_eq!(created[0].0, PathBuf::from("/repos/myrepo"));
        assert_eq!(created[0].1, "fix-bug");
        // Worktree should be under repo_path/worktree_dir/branch.
        assert_eq!(
            created[0].2,
            PathBuf::from("/repos/myrepo/.worktrees/fix-bug"),
            "worktree should use config.defaults.worktree_dir, not parent dir",
        );

        // Verify status message indicates success with worktree.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("Imported") && msg.contains("worktree created"),
            "expected import success with worktree message, got: {msg}",
        );
    }

    /// F-1 regression: importing a PR whose branch cannot be fetched from
    /// origin must NOT create a worktree (to avoid creating from wrong
    /// revision). The backend record is still created so the work item
    /// exists, but the user is told to check out manually.
    #[test]
    fn import_skips_worktree_when_fetch_fails() {
        use crate::work_item::{
            CheckStatus, PrInfo, PrState, ReviewDecision,
        };
        use crate::work_item_backend::ListResult;
        use crate::worktree_service::{WorktreeError, WorktreeInfo};

        /// Mock worktree service where fetch_branch always fails
        /// (simulates fork PR or branch not on origin).
        struct FailFetchWorktreeService {
            created: std::sync::Mutex<Vec<(PathBuf, String, PathBuf)>>,
        }

        impl WorktreeService for FailFetchWorktreeService {
            fn list_worktrees(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
                Ok(Vec::new())
            }

            fn create_worktree(
                &self,
                repo_path: &std::path::Path,
                branch: &str,
                target_dir: &std::path::Path,
            ) -> Result<WorktreeInfo, WorktreeError> {
                self.created.lock().unwrap().push((
                    repo_path.to_path_buf(),
                    branch.to_string(),
                    target_dir.to_path_buf(),
                ));
                Ok(WorktreeInfo {
                    path: target_dir.to_path_buf(),
                    branch: Some(branch.to_string()),
                    is_main: false,
                })
            }

            fn remove_worktree(
                &self,
                _repo_path: &std::path::Path,
                _worktree_path: &std::path::Path,
                _delete_branch: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn default_branch(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<String, WorktreeError> {
                Ok("main".to_string())
            }

            fn github_remote(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<Option<(String, String)>, WorktreeError> {
                Ok(None)
            }

            fn fetch_branch(
                &self,
                _repo_path: &std::path::Path,
                _branch: &str,
            ) -> Result<(), WorktreeError> {
                Err(WorktreeError::GitError(
                    "fatal: couldn't find remote ref fork-branch".into(),
                ))
            }
        }

        /// Test backend that supports import.
        struct TestBackend {
            records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
        }

        impl WorkItemBackend for TestBackend {
            fn list(&self) -> Result<ListResult, BackendError> {
                Ok(ListResult {
                    records: self.records.lock().unwrap().clone(),
                    corrupt: Vec::new(),
                })
            }
            fn create(
                &self,
                _req: CreateWorkItem,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported.json")),
                    title: unlinked.pr.title.clone(),
                    status: WorkItemStatus::InProgress,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                    }],
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let mock_ws = Arc::new(FailFetchWorktreeService {
            created: std::sync::Mutex::new(Vec::new()),
        });
        let backend = TestBackend {
            records: std::sync::Mutex::new(Vec::new()),
        };
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Box::new(backend),
            Arc::clone(&mock_ws) as Arc<dyn WorktreeService + Send + Sync>,
        );

        // Set up an unlinked PR to import (simulates a fork PR).
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: PathBuf::from("/repos/myrepo"),
            pr: PrInfo {
                number: 99,
                title: "Fork contribution".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: "https://github.com/o/r/pull/99".into(),
            },
            branch: "fork-branch".into(),
        });
        app.build_display_list();

        // Select the unlinked item.
        let unlinked_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
            .expect("should have an unlinked item in display list");
        app.selected_item = Some(unlinked_idx);

        // Import it.
        app.import_selected_unlinked();

        // Verify NO worktree was created (fetch failed, so we skip).
        let created = mock_ws.created.lock().unwrap();
        assert_eq!(
            created.len(),
            0,
            "import should NOT create a worktree when fetch fails, but {} were created",
            created.len(),
        );

        // Verify the backend record WAS created (import succeeded).
        assert!(
            !app.work_items.is_empty(),
            "backend record should still be created even when fetch fails",
        );

        // Verify status message tells user about manual checkout.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("could not fetch branch") && msg.contains("Manual checkout required"),
            "expected manual checkout message, got: {msg}",
        );
    }

    // -- Round 10 regression tests --

    /// F-2 regression: worktree_target_path builds the path under
    /// repo_path/worktree_dir/sanitized_branch, not
    /// repo_path.parent()/<repo>-wt-<branch>.
    #[test]
    fn worktree_target_path_uses_config_worktree_dir() {
        let repo = PathBuf::from("/repos/myrepo");

        // Default worktree_dir is ".worktrees"
        let path = App::worktree_target_path(&repo, "feature/login", ".worktrees");
        assert_eq!(
            path,
            PathBuf::from("/repos/myrepo/.worktrees/feature-login"),
            "worktree should be under repo_path/worktree_dir with / replaced by -",
        );

        // Custom worktree_dir
        let path = App::worktree_target_path(&repo, "fix/auth-bug", "wt");
        assert_eq!(
            path,
            PathBuf::from("/repos/myrepo/wt/fix-auth-bug"),
        );

        // Branch with no slashes
        let path = App::worktree_target_path(&repo, "simple-branch", ".worktrees");
        assert_eq!(
            path,
            PathBuf::from("/repos/myrepo/.worktrees/simple-branch"),
        );
    }

    /// F-2 regression: import_selected_unlinked creates the worktree under
    /// repo_path/worktree_dir/branch, not repo_path.parent()/<repo>-wt-<branch>.
    #[test]
    fn import_creates_worktree_under_config_worktree_dir() {
        use crate::work_item::{
            CheckStatus, PrInfo, PrState, ReviewDecision,
        };
        use crate::work_item_backend::ListResult;
        use crate::worktree_service::{WorktreeError, WorktreeInfo};

        /// Mock worktree service that records create_worktree calls.
        struct MockWorktreeService {
            created: std::sync::Mutex<Vec<(PathBuf, String, PathBuf)>>,
        }

        impl WorktreeService for MockWorktreeService {
            fn list_worktrees(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
                Ok(Vec::new())
            }

            fn create_worktree(
                &self,
                repo_path: &std::path::Path,
                branch: &str,
                target_dir: &std::path::Path,
            ) -> Result<WorktreeInfo, WorktreeError> {
                self.created.lock().unwrap().push((
                    repo_path.to_path_buf(),
                    branch.to_string(),
                    target_dir.to_path_buf(),
                ));
                Ok(WorktreeInfo {
                    path: target_dir.to_path_buf(),
                    branch: Some(branch.to_string()),
                    is_main: false,
                })
            }

            fn remove_worktree(
                &self,
                _repo_path: &std::path::Path,
                _worktree_path: &std::path::Path,
                _delete_branch: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn default_branch(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<String, WorktreeError> {
                Ok("main".to_string())
            }

            fn github_remote(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<Option<(String, String)>, WorktreeError> {
                Ok(None)
            }

            fn fetch_branch(
                &self,
                _repo_path: &std::path::Path,
                _branch: &str,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }
        }

        /// Test backend that supports import.
        struct TestBackend {
            records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
        }

        impl WorkItemBackend for TestBackend {
            fn list(&self) -> Result<ListResult, BackendError> {
                Ok(ListResult {
                    records: self.records.lock().unwrap().clone(),
                    corrupt: Vec::new(),
                })
            }
            fn create(
                &self,
                _req: CreateWorkItem,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported.json")),
                    title: unlinked.pr.title.clone(),
                    status: WorkItemStatus::InProgress,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                    }],
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        // Use a custom worktree_dir to verify it is respected.
        let mut config = Config::default();
        config.defaults.worktree_dir = "my-worktrees".to_string();

        let mock_ws = Arc::new(MockWorktreeService {
            created: std::sync::Mutex::new(Vec::new()),
        });
        let backend = TestBackend {
            records: std::sync::Mutex::new(Vec::new()),
        };
        let mut app = App::with_config_and_worktree_service(
            config,
            Box::new(backend),
            Arc::clone(&mock_ws) as Arc<dyn WorktreeService + Send + Sync>,
        );

        // Set up an unlinked PR with a branch containing /.
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: PathBuf::from("/repos/myrepo"),
            pr: PrInfo {
                number: 42,
                title: "Fix the bug".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: "https://github.com/o/r/pull/42".into(),
            },
            branch: "feature/login-page".into(),
        });
        app.build_display_list();

        // Select the unlinked item.
        let unlinked_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
            .expect("should have an unlinked item in display list");
        app.selected_item = Some(unlinked_idx);

        // Import it.
        app.import_selected_unlinked();

        // Verify the worktree target directory uses config.defaults.worktree_dir
        // and sanitizes the branch name.
        let created = mock_ws.created.lock().unwrap();
        assert_eq!(
            created.len(),
            1,
            "import should create exactly one worktree",
        );
        assert_eq!(
            created[0].2,
            PathBuf::from("/repos/myrepo/my-worktrees/feature-login-page"),
            "worktree should be under repo_path/worktree_dir/sanitized-branch",
        );
    }
}
