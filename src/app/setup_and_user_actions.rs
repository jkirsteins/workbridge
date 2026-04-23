//! App construction and user-action admission helpers.
//!
//! Holds the three `App` constructors (`new`, `with_config`,
//! `with_config_worktree_and_github`) and the user-action guard
//! admission API (`try_begin_user_action`, `attach_user_action_payload`,
//! `end_user_action`, etc.). Also carries the small `has_visible_status_bar`
//! cross-cutting helper because its truth value combines two subsystem
//! states (shell-level `status_message` + the `Activities` queue) and
//! no single subsystem owns it.
//!
//! The methods here are the "top of the graph" for almost every
//! other subsystem: every spawn site goes through
//! `try_begin_user_action` for single-flight admission, and every
//! test spins up the app via `new`.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{
    Activities, ActivityId, BoardCursor, CleanupFlowFlags, ClickTracking, DashboardWindow,
    DeleteFlowFlags, DisplayEntry, FetcherFlags, GhStatusFlags, GlobalDrawer, MergeFlowFlags,
    Metrics, OrphanCleanup, PrIdentityBackfill, PromptFlags, RightPanelTab, SettingsOverlay,
    SharedServices, Shell, Toasts, UserActionGuard, UserActionKey, UserActionPayload,
    UserActionState, ViewMode, WorkItemContext, canonicalize_repo_entries,
};
#[cfg(test)]
use super::{StubBackend, StubWorktreeService};
use crate::agent_backend::ClaudeCodeBackend;
use crate::config::{Config, ConfigProvider, RepoEntry, RepoSource};
use crate::create_dialog::CreateDialog;
use crate::mcp::McpEvent;
use crate::work_item::WorkItemId;
use crate::work_item_backend::WorkItemBackend;
use crate::worktree_service::WorktreeService;

impl super::App {
    /// Check if the `gh` CLI is available by running `gh --version`.
    /// Returns true if the command exits successfully, false otherwise.
    pub fn check_gh_available() -> bool {
        std::process::Command::new("gh")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Create a new App with default (empty) config and a stub backend.
    /// Uses `InMemoryConfigProvider` so tests never touch the real config.
    #[cfg(test)]
    pub fn new() -> Self {
        use crate::config::InMemoryConfigProvider;
        Self::with_config_and_worktree_service(
            Config::default(),
            Arc::new(StubBackend),
            Arc::new(StubWorktreeService),
            Box::new(InMemoryConfigProvider::new()),
        )
    }

    /// Create a new App with the given config and backend.
    /// Uses `InMemoryConfigProvider` so tests never touch the real config.
    /// Uses a no-op worktree service. Call `with_config_and_worktree_service`
    /// to provide a real or mock worktree service.
    #[cfg(test)]
    pub fn with_config(config: Config, backend: Arc<dyn WorkItemBackend>) -> Self {
        use crate::config::InMemoryConfigProvider;
        Self::with_config_and_worktree_service(
            config,
            backend,
            Arc::new(StubWorktreeService),
            Box::new(InMemoryConfigProvider::new()),
        )
    }

    /// Create a new App with the given config, backend, worktree service,
    /// and config provider. Uses a `StubGithubClient` for the merge
    /// precheck - production callers that need live GitHub queries
    /// must use `with_config_worktree_and_github` instead.
    ///
    /// `#[cfg(test)]` because production threads a real
    /// `GhCliClient` in via `with_config_worktree_and_github`; this
    /// wrapper exists so the large existing body of tests does not
    /// have to change signatures for the merge-readiness work.
    #[cfg(test)]
    pub fn with_config_and_worktree_service(
        config: Config,
        backend: Arc<dyn WorkItemBackend>,
        worktree_service: Arc<dyn WorktreeService + Send + Sync>,
        config_provider: Box<dyn ConfigProvider>,
    ) -> Self {
        Self::with_config_worktree_and_github(
            config,
            backend,
            worktree_service,
            Arc::new(crate::github_client::StubGithubClient),
            config_provider,
        )
    }

    /// Primary constructor. Same as `with_config_and_worktree_service`
    /// but also accepts a `GithubClient` used by the merge precheck
    /// to re-fetch the live PR mergeable flag and CI rollup.
    /// Production `main.rs` passes `GhCliClient`; tests that exercise
    /// the conflict / CI-failing / no-PR / error branches pass a
    /// `MockGithubClient` with `live_pr_state` configured.
    pub fn with_config_worktree_and_github(
        config: Config,
        backend: Arc<dyn WorkItemBackend>,
        worktree_service: Arc<dyn WorktreeService + Send + Sync>,
        github_client: Arc<dyn crate::github_client::GithubClient + Send + Sync>,
        config_provider: Box<dyn ConfigProvider>,
    ) -> Self {
        let active_repo_cache = canonicalize_repo_entries(config.active_repos());
        let (mcp_tx, mcp_rx) = crossbeam_channel::unbounded();
        let services = SharedServices {
            backend,
            worktree_service,
            github_client,
            pr_closer: crate::pr_service::default_pr_closer(),
            agent_backend: Arc::new(ClaudeCodeBackend),
            config,
            config_provider,
        };
        let mut app = Self::new_from_parts(services, active_repo_cache, mcp_tx, mcp_rx);
        app.reassemble_work_items();
        app.build_display_list();
        app
    }

    /// Build the `App` struct from its already-prepared shared services,
    /// repo cache, and MCP channels. Kept private so every public
    /// constructor (`with_config_worktree_and_github` and any future
    /// variants) routes through the same field-initialization list.
    fn new_from_parts(
        services: SharedServices,
        active_repo_cache: Vec<RepoEntry>,
        mcp_tx: crossbeam_channel::Sender<McpEvent>,
        mcp_rx: crossbeam_channel::Receiver<McpEvent>,
    ) -> Self {
        Self {
            services,
            shell: Shell::new(),
            delete_flow: DeleteFlowFlags::default(),
            delete_target_wi_id: None,
            delete_target_title: None,
            delete_sync_warnings: Vec::new(),
            set_branch_dialog: None,
            merge_flow: MergeFlowFlags::default(),
            merge_wi_id: None,
            rework_prompt_input: rat_widget::text_input::TextInputState::new(),
            rework_prompt_wi: None,
            rework_reasons: HashMap::new(),
            review_gate_findings: HashMap::new(),
            cleanup_flow: CleanupFlowFlags::default(),
            cleanup_reason_input: rat_widget::text_input::TextInputState::new(),
            cleanup_unlinked_target: None,
            cleanup_progress_pr_number: None,
            cleanup_progress_repo_path: None,
            cleanup_progress_branch: None,
            cleanup_evicted_branches: Vec::new(),
            alert_message: None,
            branch_gone_prompt: None,
            stale_worktree_prompt: None,
            prompt_flags: PromptFlags::default(),
            no_plan_prompt_queue: VecDeque::new(),
            settings: SettingsOverlay::new(),
            active_repo_cache,
            create_dialog: CreateDialog::new(),
            work_items: Vec::new(),
            unlinked_prs: Vec::new(),
            review_requested_prs: Vec::new(),
            current_user_login: None,
            sessions: HashMap::new(),
            repo_data: HashMap::new(),
            fetch_rx: None,
            gh_status: GhStatusFlags {
                available: Self::check_gh_available(),
                cli_not_found_shown: false,
                auth_required_shown: false,
            },
            worktree_errors_shown: std::collections::HashSet::new(),
            selected_item: None,
            list_scroll_offset: Cell::new(0),
            recenter_viewport_on_selection: Cell::new(false),
            work_item_list_body: Cell::new(None),
            list_max_item_offset: Cell::new(0),
            display_list: Vec::new(),
            view_mode: ViewMode::FlatList,
            board_cursor: BoardCursor {
                column: 0,
                row: None,
            },
            board_drill_down: false,
            dashboard_window: DashboardWindow::Month,
            metrics: Metrics::new(),
            board_drill_stage: None,
            fetcher_flags: FetcherFlags::default(),
            selected_work_item: None,
            selected_unlinked_branch: None,
            selected_review_request_branch: None,
            pending_fetch_errors: Vec::new(),
            fetcher_handle: None,
            harness_choice: HashMap::new(),
            last_k_press: None,
            first_run_global_harness_modal: None,
            mcp_servers: HashMap::new(),
            agent_working: std::collections::HashSet::new(),
            mcp_rx: Some(mcp_rx),
            mcp_tx,
            review_gates: HashMap::new(),
            rebase_gates: HashMap::new(),
            activities: Activities::new(),
            user_actions: UserActionGuard::default(),
            pr_create_pending: VecDeque::new(),
            review_reopen_suppress: std::collections::HashSet::new(),
            mergequeue_watches: Vec::new(),
            mergequeue_polls: HashMap::new(),
            mergequeue_poll_errors: HashMap::new(),
            review_request_merge_watches: Vec::new(),
            review_request_merge_polls: HashMap::new(),
            review_request_merge_poll_errors: HashMap::new(),
            pr_identity_backfill: PrIdentityBackfill::new(),
            session_open_rx: HashMap::new(),
            session_spawn_rx: HashMap::new(),
            orphan_cleanup: OrphanCleanup::new(),
            global_drawer: GlobalDrawer::new(),
            pending_active_pty_bytes: Vec::new(),
            right_panel_tab: RightPanelTab::ClaudeCode,
            terminal_sessions: HashMap::new(),
            pending_terminal_pty_bytes: Vec::new(),
            click_tracking: ClickTracking::new(),
            toasts: Toasts::new(),
        }
    }

    // -- Activity indicator API --

    /// Whether the status bar row should be visible. True when either
    /// a status message or an activity indicator is present. Lives on
    /// `App` (not on `Activities`) because it combines state from two
    /// subsystems (shell-level `status_message` + activities queue).
    #[must_use]
    pub const fn has_visible_status_bar(&self) -> bool {
        self.shell.status_message.is_some() || !self.activities.entries.is_empty()
    }

    // -- User action guard API --

    /// Attempt to admit a user-initiated remote-I/O action. Returns
    /// `Some(activity_id)` on success (status-bar spinner started,
    /// helper entry inserted with `UserActionPayload::Empty`) or `None`
    /// if another entry for `key` is already in flight OR the debounce
    /// window from the previous attempt has not elapsed.
    ///
    /// `debounce` should be `Duration::ZERO` for most callers; only
    /// key-spam-prone handlers (currently Ctrl+R) pass a nonzero value.
    /// Tests override the debounce with a short `Duration::from_millis`
    /// to avoid real sleeps.
    ///
    /// The helper deliberately does NOT emit any status message or
    /// alert on rejection - every caller owns its rejection UX and the
    /// wording is caller-specific. See `docs/UI.md` "User action guard"
    /// for the contract and `CLAUDE.md` severity overrides for the
    /// review policy that requires user-initiated remote I/O to go
    /// through this helper.
    pub fn try_begin_user_action(
        &mut self,
        key: UserActionKey,
        debounce: Duration,
        message: impl Into<String>,
    ) -> Option<ActivityId> {
        let now = crate::side_effects::clock::instant_now();
        if self.user_actions.in_flight.contains_key(&key) {
            return None;
        }
        if !debounce.is_zero()
            && let Some(last) = self.user_actions.last_attempted.get(&key)
            && now.saturating_duration_since(*last) < debounce
        {
            return None;
        }
        self.user_actions.last_attempted.insert(key.clone(), now);
        let activity_id = self.activities.start(message);
        self.user_actions.in_flight.insert(
            key,
            UserActionState {
                activity_id,
                payload: UserActionPayload::Empty,
            },
        );
        Some(activity_id)
    }

    /// Attach a receiver/metadata payload to an already-admitted user
    /// action. Unconditionally panics (in both debug and release) if no
    /// entry for `key` exists - this is a programming error; every
    /// `attach` must be preceded by a successful
    /// `try_begin_user_action` call. Silently skipping the attach in
    /// release builds would leave the helper map entry pinned to
    /// `UserActionPayload::Empty` forever, which `is_user_action_in_flight`
    /// would still report as "in flight" until something else cleared
    /// it - exactly the latent-state bug the helper is meant to
    /// eliminate.
    pub fn attach_user_action_payload(&mut self, key: &UserActionKey, payload: UserActionPayload) {
        if let Some(state) = self.user_actions.in_flight.get_mut(key) {
            state.payload = payload;
        } else {
            // Calling `attach_user_action_payload` without a prior
            // successful `try_begin_user_action` is a caller-side
            // invariant violation: every attach must be preceded by
            // an admit that returned `Some(_)`. Hard-panic in debug
            // so the bug surfaces in tests; in release, fall through
            // silently so production never crashes the whole TUI on
            // what should be a recoverable wiring mistake.
            debug_assert!(
                false,
                "attach_user_action_payload called without a prior successful \
                 try_begin_user_action for {key:?}",
            );
        }
    }

    /// End a user action: remove the map entry and clear the status-bar
    /// spinner. Idempotent - calling twice (or calling without a prior
    /// begin) is a no-op, because early-return cancel paths (delete,
    /// retreat) use this as a best-effort cleanup.
    pub fn end_user_action(&mut self, key: &UserActionKey) {
        if let Some(state) = self.user_actions.in_flight.remove(key) {
            self.activities.end(state.activity_id);
        }
    }

    /// Returns true while a user action for `key` is still in flight.
    /// Pure in-memory check - no I/O, safe on the UI thread.
    pub fn is_user_action_in_flight(&self, key: &UserActionKey) -> bool {
        self.user_actions.in_flight.contains_key(key)
    }

    /// Borrow the payload stored under `key`, if any.
    pub fn user_action_payload(&self, key: &UserActionKey) -> Option<&UserActionPayload> {
        self.user_actions.in_flight.get(key).map(|s| &s.payload)
    }

    /// Return the `WorkItemId` the in-flight action is targeting, if
    /// the payload carries one. Used by delete/retreat cancel paths.
    pub fn user_action_work_item(&self, key: &UserActionKey) -> Option<&WorkItemId> {
        match self.user_action_payload(key)? {
            UserActionPayload::PrCreate { wi_id, .. }
            | UserActionPayload::ReviewSubmit { wi_id, .. }
            | UserActionPayload::WorktreeCreate { wi_id, .. }
            | UserActionPayload::RebaseOnMain { wi_id, .. } => Some(wi_id),
            _ => None,
        }
    }

    /// Reset all fetcher-derived UI state to a clean slate. Called from
    /// the salsa structural-restart block (src/salsa.rs) when the repo
    /// set changes and the old fetcher thread is being torn down and
    /// replaced with a new one against a different repo list.
    ///
    /// The restart drops `fetch_rx`, which severs the channel any
    /// mid-flight fetcher thread would otherwise use to deliver its
    /// paired `RepoData` / `FetcherError` terminal messages. Without
    /// resetting the derived state, any `FetchStarted` whose count was
    /// already incremented on a prior tick (via `drain_fetch_results`)
    /// would be stranded forever: `activities.pending_fetch_count` would stay
    /// non-zero for the rest of the process lifetime, which the Ctrl+R
    /// hard gate in `src/event.rs` interprets as "a fetch cycle is
    /// still running" and rejects every user-initiated refresh from
    /// that point on. The dangling `activities.structural_fetch` id would
    /// similarly leave a stuck spinner on the status bar.
    ///
    /// This helper groups the three invariants that must always move
    /// together on a structural restart:
    ///   1. `fetch_rx = None` - the channel the old threads write into
    ///      is torn down.
    ///   2. `activities.pending_fetch_count = 0` - any counted-but-unpaired
    ///      `FetchStarted` from the old channel is reset so the Ctrl+R
    ///      gate does not permanently lock out the user.
    ///   3. `activities.structural_fetch` / `UserActionKey::GithubRefresh`
    ///      activities are ended so no stuck spinner remains.
    ///
    /// Keeping them in a single method makes the structural ownership
    /// explicit: there is exactly one site that tears down fetcher
    /// state, and every derived field is visible there.
    pub fn reset_fetch_state(&mut self) {
        // 1. Drop the old channel so no stale terminal messages from
        //    the previous fetcher thread are drained into the new
        //    accounting.
        self.fetch_rx = None;
        // 2. Reset the count so any previously-counted `FetchStarted`
        //    whose paired `RepoData` / `FetcherError` will never
        //    arrive cannot strand the Ctrl+R gate.
        self.activities.pending_fetch_count = 0;
        // 3. End both possible owners of the current fetch spinner.
        //    Both are idempotent no-ops when already clear.
        self.end_user_action(&UserActionKey::GithubRefresh);
        if let Some(id) = self.activities.structural_fetch.take() {
            self.activities.end(id);
        }
    }

    /// Check whether a path is inside (or equal to) one of the active
    /// managed repos. Uses canonical paths for reliable comparison.
    /// Returns the matching repo root path if found.
    ///
    /// The cache entries are already canonicalized (via `canonicalize_repo_entries`),
    /// so we only need to canonicalize the input path.
    pub fn managed_repo_root(&self, path: &std::path::Path) -> Option<PathBuf> {
        let canonical_path =
            crate::config::canonicalize_path(path).unwrap_or_else(|_| path.to_path_buf());
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
    pub(super) fn refresh_repo_cache(&mut self) {
        self.active_repo_cache = canonicalize_repo_entries(self.services.config.active_repos());
    }

    /// Total number of active repos for scroll bounds.
    pub const fn total_repos(&self) -> usize {
        self.active_repo_cache.len()
    }

    /// Build the list of available (unmanaged) repos: all repos minus active.
    /// Used by the settings overlay to show what can be managed.
    pub fn available_repos(&self) -> Vec<RepoEntry> {
        let active_paths: Vec<_> = self.active_repo_cache.iter().map(|e| &e.path).collect();
        self.services
            .config
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
                let repo_name = wi
                    .repo_associations
                    .first()
                    .map(|a| {
                        a.repo_path.file_name().map_or_else(
                            || a.repo_path.display().to_string(),
                            |s| s.to_string_lossy().into_owned(),
                        )
                    })
                    .unwrap_or_default();
                let labels = wi
                    .repo_associations
                    .iter()
                    .find_map(|a| a.issue.as_ref())
                    .map(|issue| issue.labels.clone())
                    .unwrap_or_default();
                Some(WorkItemContext {
                    title: wi.title.clone(),
                    stage: format!("{:?}", wi.status),
                    repo_name,
                    labels,
                })
            }
            _ => None,
        }
    }

    /// Unmanage the currently selected managed repo and save config.
    /// Removes from `included_repos`. Explicit repos cannot be unmanaged
    /// this way (they must be removed via `remove_path`).
    /// If the save fails, the in-memory mutation is rolled back so the
    /// UI stays consistent with what is persisted on disk.
    pub fn unmanage_selected_repo(&mut self) {
        if self.active_repo_cache.is_empty() {
            return;
        }
        let idx = self
            .settings
            .repo_selected
            .min(self.active_repo_cache.len().saturating_sub(1));
        let entry = &self.active_repo_cache[idx];
        if entry.source == RepoSource::Explicit {
            self.shell.status_message =
                Some("Explicit repos cannot be unmanaged (use 'repos remove')".into());
            return;
        }
        let path = entry.path.display().to_string();
        self.services.config.uninclude_repo(&path);
        if let Err(e) = self.services.config_provider.save(&self.services.config) {
            // Rollback: re-add the inclusion since save failed.
            self.services.config.include_repo(&path);
            self.shell.status_message = Some(format!("Error saving config: {e}"));
            return;
        }
        self.shell.status_message = Some(format!("Unmanaged: {path}"));
        self.fetcher_flags.repos_changed = true;
        self.refresh_repo_cache();
        // Adjust cursor if it went past the end.
        if self.active_repo_cache.is_empty() {
            self.settings.repo_selected = 0;
        } else {
            self.settings.repo_selected = self
                .settings
                .repo_selected
                .min(self.active_repo_cache.len() - 1);
        }
    }

    /// Manage the currently selected available repo and save config.
    /// Adds to `included_repos`.
    /// If the save fails, the in-memory mutation is rolled back.
    pub fn manage_selected_repo(&mut self) {
        let available = self.available_repos();
        if available.is_empty() {
            return;
        }
        let idx = self
            .settings
            .available_selected
            .min(available.len().saturating_sub(1));
        let path = available[idx].path.display().to_string();
        self.services.config.include_repo(&path);
        if let Err(e) = self.services.config_provider.save(&self.services.config) {
            // Rollback: remove the inclusion since save failed.
            self.services.config.uninclude_repo(&path);
            self.shell.status_message = Some(format!("Error saving config: {e}"));
            return;
        }
        self.shell.status_message = Some(format!("Managed: {path}"));
        self.fetcher_flags.repos_changed = true;
        self.refresh_repo_cache();
        // Adjust cursor if it went past the end.
        let new_available = self.available_repos();
        let new_len = new_available.len();
        if new_len > 0 {
            self.settings.available_selected = self.settings.available_selected.min(new_len - 1);
        } else {
            self.settings.available_selected = 0;
        }
    }
}
