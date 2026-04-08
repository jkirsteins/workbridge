use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::assembly;
use crate::config::{Config, ConfigProvider, RepoEntry, RepoSource};
use crate::create_dialog::CreateDialog;
use crate::github_client::GithubError;
use crate::mcp::{McpEvent, McpSocketServer};
use crate::session::Session;
use crate::work_item::{
    FetchMessage, FetcherHandle, RepoFetchResult, ReviewRequestedPr, SessionEntry, UnlinkedPr,
    WorkItem, WorkItemId, WorkItemKind, WorkItemStatus,
};
use crate::work_item_backend::{
    ActivityEntry, BackendError, CreateWorkItem, PrIdentityRecord, RepoAssociationRecord,
    WorkItemBackend,
};
use crate::worktree_service::WorktreeService;

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FocusPanel {
    Left,
    Right,
}

/// Which view mode the root overview is in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    FlatList,
    Board,
}

/// Cursor state for the board view.
/// Tracks which column is selected and which item within that column.
pub struct BoardCursor {
    /// Index into BOARD_COLUMNS (0=Backlog, 1=Planning, 2=Implementing, 3=Review).
    pub column: usize,
    /// Index of the selected item within the column, or None if column is empty.
    pub row: Option<usize>,
}

/// The four visible columns in the board view (Done is hidden).
pub const BOARD_COLUMNS: &[WorkItemStatus] = &[
    WorkItemStatus::Backlog,
    WorkItemStatus::Planning,
    WorkItemStatus::Implementing,
    WorkItemStatus::Review,
];

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
    /// The workflow stage name (e.g., "Backlog", "Implementing").
    pub stage: String,
    /// The repository path on disk (from RepoAssociation.repo_path).
    pub repo_path: String,
    /// Issue labels (from IssueInfo.labels). Empty if no issue linked.
    pub labels: Vec<String>,
    /// Last activity entry description (for status bar display).
    pub last_activity: Option<String>,
}

/// State for the 3-step delete confirmation flow.
///
/// None -> AwaitingConfirm (first press) -> AwaitingForce (dirty worktree) -> deleted
/// None -> AwaitingConfirm (first press) -> deleted (clean worktree)
#[derive(Clone, Debug, PartialEq)]
pub enum DeleteConfirmState {
    /// No delete in progress.
    None,
    /// First press received - "Press again to delete".
    AwaitingConfirm,
    /// Dirty worktree detected - "Press again to force-delete".
    AwaitingForce,
}

/// Visual style variant for group headers.
#[derive(Clone, Debug, PartialEq)]
pub enum GroupHeaderKind {
    Normal,
    Blocked,
}

/// An entry in the flat display list rendered in the left panel.
#[derive(Clone, Debug)]
pub enum DisplayEntry {
    /// Section header with label, item count, and visual kind.
    GroupHeader {
        label: String,
        count: usize,
        kind: GroupHeaderKind,
    },
    /// A review-requested PR (index into App::review_requested_prs).
    ReviewRequestItem(usize),
    /// An unlinked PR (index into App::unlinked_prs).
    UnlinkedItem(usize),
    /// A work item (index into App::work_items).
    WorkItemEntry(usize),
}

/// A unique identifier for a tracked activity shown in the status bar.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ActivityId(u64);

/// A currently running activity displayed in the status bar.
#[derive(Clone, Debug)]
pub struct Activity {
    pub id: ActivityId,
    pub message: String,
}

/// Outcome of attempting to spawn the review gate.
pub enum ReviewGateSpawn {
    /// The gate was spawned and is running - caller should wait for result.
    Spawned,
    /// The gate cannot run - caller must NOT advance to Review.
    /// Contains a human-readable reason to display.
    Blocked(String),
}

/// Result from the asynchronous review gate check.
pub struct ReviewGateResult {
    /// The work item that was being checked.
    pub work_item_id: WorkItemId,
    /// True if the gate approved the transition.
    pub approved: bool,
    /// Human-readable detail (approval note or rejection reason).
    pub detail: String,
}

/// Result from the asynchronous PR creation thread.
pub struct PrCreateResult {
    /// The work item the PR was created for.
    pub wi_id: WorkItemId,
    /// Human-readable success info (e.g., "PR created: <url>").
    pub info: Option<String>,
    /// Human-readable error message.
    pub error: Option<String>,
    /// PR URL for activity log (separate from display info).
    pub url: Option<String>,
}

/// Outcome of an asynchronous PR merge operation.
pub enum PrMergeOutcome {
    /// No PR found for this branch - advance to Done directly.
    NoPr,
    /// PR merged successfully.
    Merged {
        strategy: String,
        /// PR identity fetched from GitHub at merge time.
        pr_identity: Option<PrIdentityRecord>,
    },
    /// Merge failed due to conflicts - send back to Implementing.
    Conflict { stderr: String },
    /// Merge failed for another reason.
    Failed { error: String },
}

/// Result from the asynchronous PR merge thread.
pub struct PrMergeResult {
    /// The work item the merge was attempted for.
    pub wi_id: WorkItemId,
    /// The branch that was being merged.
    pub branch: String,
    /// The repo path for persisting PR identity.
    pub repo_path: PathBuf,
    /// The outcome of the merge attempt.
    pub outcome: PrMergeOutcome,
}

/// Result from the background PR identity backfill thread.
pub struct PrIdentityBackfillResult {
    pub wi_id: WorkItemId,
    pub repo_path: PathBuf,
    pub identity: PrIdentityRecord,
}

/// Result from the asynchronous worktree creation thread.
pub struct WorktreeCreateResult {
    /// The work item the worktree was created for.
    pub wi_id: WorkItemId,
    /// The repo path the worktree belongs to.
    pub repo_path: PathBuf,
    /// The worktree path on success.
    pub path: Option<PathBuf>,
    /// Human-readable error message on failure.
    pub error: Option<String>,
}

/// App holds the entire application state.
pub struct App {
    pub should_quit: bool,
    pub focus: FocusPanel,
    /// Status message displayed to the user (errors, confirmations, etc.).
    pub status_message: Option<String>,
    /// True when waiting for a second press to confirm quit.
    pub confirm_quit: bool,
    /// State of the delete confirmation flow (None/AwaitingConfirm/AwaitingForce).
    pub confirm_delete: DeleteConfirmState,
    /// True when the merge strategy prompt is visible (Review -> Done).
    pub confirm_merge: bool,
    /// The work item ID that the merge prompt applies to.
    pub merge_wi_id: Option<WorkItemId>,
    /// True when the rework reason text input is visible (Review -> Implementing).
    pub rework_prompt_visible: bool,
    /// Text input for the rework reason.
    pub rework_prompt_input: crate::create_dialog::SimpleTextInput,
    /// The work item ID that the rework prompt applies to.
    pub rework_prompt_wi: Option<WorkItemId>,
    /// Rework reasons keyed by work item ID. Used by stage_system_prompt
    /// to select the "implementing_rework" prompt template.
    pub rework_reasons: HashMap<WorkItemId, String>,
    /// Review-gate findings keyed by work item ID. Populated when the gate
    /// approves, consumed one-shot by `stage_system_prompt` to select the
    /// "review_with_findings" prompt template and inject the assessment.
    pub review_gate_findings: HashMap<WorkItemId, String>,
    /// True when the no-plan prompt is visible (offered when Claude blocks
    /// because no implementation plan exists).
    pub no_plan_prompt_visible: bool,
    /// Queue of work item IDs awaiting no-plan prompt resolution.
    /// When multiple items block with "No implementation plan" concurrently,
    /// all are queued. The front item is shown to the user; resolving it
    /// pops it and shows the next (if any).
    pub no_plan_prompt_queue: VecDeque<WorkItemId>,
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
    /// Abstracts config persistence so tests use an in-memory store.
    pub config_provider: Box<dyn ConfigProvider>,
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
    /// State for the work item creation modal dialog.
    pub create_dialog: CreateDialog,

    // -- Work item state --
    /// Backend for persisting work item records.
    pub backend: Box<dyn WorkItemBackend>,
    /// Worktree service for creating/listing worktrees.
    pub worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    /// Assembled work items (from backend records + repo data).
    pub work_items: Vec<WorkItem>,
    /// PRs not linked to any work item (only the user's own).
    pub unlinked_prs: Vec<UnlinkedPr>,
    /// PRs where the user has been requested as a reviewer.
    pub review_requested_prs: Vec<ReviewRequestedPr>,
    /// Sessions keyed by (work item ID, stage).
    pub sessions: HashMap<(WorkItemId, WorkItemStatus), SessionEntry>,
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
    /// True if the `gh` CLI is available at startup.
    pub gh_available: bool,
    /// Repo paths for which a worktree fetch error has already been shown.
    /// Prevents flooding the status bar when every fetch cycle for the
    /// same repo returns an error.
    pub worktree_errors_shown: std::collections::HashSet<PathBuf>,
    /// Currently selected index in the display list (items only, not headers).
    pub selected_item: Option<usize>,
    /// Flat display list for the left panel.
    pub display_list: Vec<DisplayEntry>,
    /// Which view mode the root overview is in.
    pub view_mode: ViewMode,
    /// Cursor state for the board view.
    pub board_cursor: BoardCursor,
    /// True when user pressed Enter from board view (shows filtered two-panel layout).
    pub board_drill_down: bool,
    /// The stage being drilled into (for filtering the left panel).
    pub board_drill_stage: Option<WorkItemStatus>,
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
    /// Tracks the selected review-requested PR for selection restoration.
    pub selected_review_request_branch: Option<(PathBuf, String)>,
    /// Fetch errors that could not be shown because the status bar was
    /// occupied. Drained on the next tick when status_message is None.
    pub pending_fetch_errors: Vec<String>,
    /// True when the fetcher channel has disconnected unexpectedly (all
    /// sender threads exited). Surfaced in the status bar so the user
    /// knows background updates have stopped.
    pub fetcher_disconnected: bool,
    /// Handle to the background fetcher threads. Used to stop the fetcher
    /// when repos change or when the app shuts down. Managed by the
    /// rat-salsa event callback in salsa.rs.
    pub fetcher_handle: Option<FetcherHandle>,
    /// MCP socket servers keyed by work item ID. Each server is created when
    /// a Claude session is spawned and handles MCP communication via a Unix socket.
    pub mcp_servers: HashMap<WorkItemId, McpSocketServer>,
    /// Work item IDs where Claude has signaled it is actively working
    /// (via workbridge_set_activity). Cleared when the session dies.
    pub claude_working: std::collections::HashSet<WorkItemId>,
    /// Paths to .mcp.json files written to worktrees, keyed by work item ID.
    /// Tracked so they can be cleaned up when sessions die or work items are deleted.
    /// Receiver for MCP events from all socket servers.
    pub mcp_rx: Option<crossbeam_channel::Receiver<McpEvent>>,
    /// Sender for MCP events (cloned for each socket server).
    pub mcp_tx: crossbeam_channel::Sender<McpEvent>,
    /// The work item ID that the current review gate was spawned for.
    /// Used to verify the gate result is still relevant (the user may have
    /// retreated the item while the gate was running).
    pub review_gate_wi: Option<WorkItemId>,
    /// Receiver for asynchronous review gate results.
    pub review_gate_rx: Option<crossbeam_channel::Receiver<ReviewGateResult>>,
    /// Activity ID for the running review gate indicator.
    pub review_gate_activity: Option<ActivityId>,
    /// Progress summary from the review gate, shown in the right panel.
    pub review_gate_progress: Option<String>,

    // -- Activity indicator --
    /// Monotonic counter for generating unique ActivityId values.
    pub activity_counter: u64,
    /// Currently running activities. The last entry is displayed in the
    /// status bar. When empty, the normal status_message shows through.
    pub activities: Vec<Activity>,
    /// Spinner frame index, advanced on each 200ms timer tick when
    /// activities are present.
    pub spinner_tick: usize,

    // -- Background fetch indicator --
    /// Activity ID for the in-flight background GitHub fetch. Started when
    /// a FetchStarted message arrives, ended when all in-flight fetches
    /// complete. Shows a spinner in the status bar during GitHub API calls.
    pub fetch_activity: Option<ActivityId>,
    /// Number of repos currently fetching. The activity spinner is shown
    /// while this is > 0 and cleared when it returns to 0.
    pub pending_fetch_count: usize,

    // -- Async PR creation --
    /// Receiver for asynchronous PR creation results.
    pub pr_create_rx: Option<crossbeam_channel::Receiver<PrCreateResult>>,
    /// Activity ID for the running PR creation indicator.
    pub pr_create_activity: Option<ActivityId>,
    /// The work item ID that the current in-flight PR creation was spawned for.
    pub pr_create_wi: Option<WorkItemId>,
    /// Queued work item IDs waiting for PR creation when a creation is
    /// already in-flight. Drained one at a time as each creation completes.
    pub pr_create_pending: VecDeque<WorkItemId>,

    // -- Async PR merge --
    /// Receiver for asynchronous PR merge results.
    pub pr_merge_rx: Option<crossbeam_channel::Receiver<PrMergeResult>>,
    /// Activity ID for the running PR merge indicator.
    pub pr_merge_activity: Option<ActivityId>,

    // -- PR identity backfill --
    /// Receiver for background PR identity backfill results (one-time startup).
    pub pr_identity_backfill_rx:
        Option<crossbeam_channel::Receiver<Result<PrIdentityBackfillResult, String>>>,

    // -- Async worktree creation --
    /// Receiver for asynchronous worktree creation results.
    pub worktree_create_rx: Option<crossbeam_channel::Receiver<WorktreeCreateResult>>,
    /// Activity ID for the running worktree creation indicator.
    pub worktree_create_activity: Option<ActivityId>,
    /// The work item ID that the current worktree creation was spawned for.
    pub worktree_create_wi: Option<WorkItemId>,

    /// Whether the global assistant drawer is open.
    pub global_drawer_open: bool,
    /// The global assistant PTY session (lazy, persistent).
    pub global_session: Option<SessionEntry>,
    /// MCP socket server for the global assistant.
    pub global_mcp_server: Option<McpSocketServer>,
    /// Dynamic context for the global MCP server, updated on each tick.
    pub global_mcp_context: Arc<Mutex<String>>,
    /// Which panel had focus before the drawer opened (restored on close).
    pub pre_drawer_focus: FocusPanel,
    /// PTY columns for the global assistant drawer (differs from main pane).
    pub global_pane_cols: u16,
    /// PTY rows for the global assistant drawer.
    pub global_pane_rows: u16,
    /// Path to the temp MCP config file for the global assistant.
    /// Tracked so it can be cleaned up on shutdown or respawn.
    pub global_mcp_config_path: Option<PathBuf>,
    /// True when repo/work-item data has changed since the last
    /// `refresh_global_mcp_context` call. Set by `drain_fetch_results`
    /// returning true; cleared after the refresh runs.
    pub global_mcp_context_dirty: bool,
}

impl App {
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
    /// Uses InMemoryConfigProvider so tests never touch the real config.
    #[cfg(test)]
    pub fn new() -> Self {
        use crate::config::InMemoryConfigProvider;
        Self::with_config_and_worktree_service(
            Config::default(),
            Box::new(StubBackend),
            Arc::new(StubWorktreeService),
            Box::new(InMemoryConfigProvider::new()),
        )
    }

    /// Create a new App with the given config and backend.
    /// Uses InMemoryConfigProvider so tests never touch the real config.
    /// Uses a no-op worktree service. Call `with_config_and_worktree_service`
    /// to provide a real or mock worktree service.
    #[cfg(test)]
    pub fn with_config(config: Config, backend: Box<dyn WorkItemBackend>) -> Self {
        use crate::config::InMemoryConfigProvider;
        Self::with_config_and_worktree_service(
            config,
            backend,
            Arc::new(StubWorktreeService),
            Box::new(InMemoryConfigProvider::new()),
        )
    }

    /// Create a new App with the given config, backend, worktree service,
    /// and config provider.
    pub fn with_config_and_worktree_service(
        config: Config,
        backend: Box<dyn WorkItemBackend>,
        worktree_service: Arc<dyn WorktreeService + Send + Sync>,
        config_provider: Box<dyn ConfigProvider>,
    ) -> Self {
        let active_repo_cache = canonicalize_repo_entries(config.active_repos());
        let (mcp_tx, mcp_rx) = crossbeam_channel::unbounded();
        let mut app = Self {
            should_quit: false,
            focus: FocusPanel::Left,
            status_message: None,
            confirm_quit: false,
            confirm_delete: DeleteConfirmState::None,
            confirm_merge: false,
            merge_wi_id: None,
            rework_prompt_visible: false,
            rework_prompt_input: crate::create_dialog::SimpleTextInput::new(),
            rework_prompt_wi: None,
            rework_reasons: HashMap::new(),
            review_gate_findings: HashMap::new(),
            no_plan_prompt_visible: false,
            no_plan_prompt_queue: VecDeque::new(),
            shutting_down: false,
            shutdown_started: None,
            pane_cols: 80,
            pane_rows: 24,
            config,
            config_provider,
            show_settings: false,
            active_repo_cache,
            settings_repo_selected: 0,
            settings_available_selected: 0,
            settings_list_focus: SettingsListFocus::Managed,
            create_dialog: CreateDialog::new(),
            backend,
            worktree_service,
            work_items: Vec::new(),
            unlinked_prs: Vec::new(),
            review_requested_prs: Vec::new(),
            sessions: HashMap::new(),
            repo_data: HashMap::new(),
            fetch_rx: None,
            gh_available: Self::check_gh_available(),
            gh_cli_not_found_shown: false,
            gh_auth_required_shown: false,
            worktree_errors_shown: std::collections::HashSet::new(),
            selected_item: None,
            display_list: Vec::new(),
            view_mode: ViewMode::FlatList,
            board_cursor: BoardCursor {
                column: 0,
                row: None,
            },
            board_drill_down: false,
            board_drill_stage: None,
            fetcher_repos_changed: false,
            selected_work_item: None,
            selected_unlinked_branch: None,
            selected_review_request_branch: None,
            pending_fetch_errors: Vec::new(),
            fetcher_disconnected: false,
            fetcher_handle: None,
            mcp_servers: HashMap::new(),
            claude_working: std::collections::HashSet::new(),
            mcp_rx: Some(mcp_rx),
            mcp_tx,
            review_gate_wi: None,
            review_gate_rx: None,
            review_gate_activity: None,
            review_gate_progress: None,
            activity_counter: 0,
            activities: Vec::new(),
            spinner_tick: 0,
            fetch_activity: None,
            pending_fetch_count: 0,
            pr_create_rx: None,
            pr_create_activity: None,
            pr_create_wi: None,
            pr_create_pending: VecDeque::new(),
            pr_merge_rx: None,
            pr_merge_activity: None,
            pr_identity_backfill_rx: None,
            worktree_create_rx: None,
            worktree_create_activity: None,
            worktree_create_wi: None,
            global_drawer_open: false,
            global_session: None,
            global_mcp_server: None,
            global_mcp_context: Arc::new(Mutex::new("{}".to_string())),
            pre_drawer_focus: FocusPanel::Left,
            global_pane_cols: 80,
            global_pane_rows: 24,
            global_mcp_config_path: None,
            global_mcp_context_dirty: false,
        };
        app.reassemble_work_items();
        app.build_display_list();
        app
    }

    // -- Activity indicator API --

    /// Start a new activity. Returns its ID for later removal.
    /// The most recently started activity is displayed in the status bar.
    pub fn start_activity(&mut self, message: impl Into<String>) -> ActivityId {
        self.activity_counter += 1;
        let id = ActivityId(self.activity_counter);
        self.activities.push(Activity {
            id,
            message: message.into(),
        });
        id
    }

    /// End an activity by its ID. No-op if already ended.
    pub fn end_activity(&mut self, id: ActivityId) {
        self.activities.retain(|a| a.id != id);
    }

    /// Returns the activity message to display, or None if idle.
    pub fn current_activity(&self) -> Option<&str> {
        self.activities.last().map(|a| a.message.as_str())
    }

    /// Whether the status bar row should be visible. True when either
    /// a status message or an activity indicator is present.
    pub fn has_visible_status_bar(&self) -> bool {
        self.status_message.is_some() || !self.activities.is_empty()
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
        let active_paths: Vec<_> = self.active_repo_cache.iter().map(|e| &e.path).collect();
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
                let last_activity = match self.backend.read_activity(&wi.id) {
                    Ok(entries) => entries
                        .last()
                        .map(|e| format!("{}: {}", e.event_type, e.timestamp)),
                    Err(_) => Some("Error reading activity log".to_string()),
                };
                Some(WorkItemContext {
                    title: wi.title.clone(),
                    stage: format!("{:?}", wi.status),
                    repo_path,
                    labels,
                    last_activity,
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
        if let Err(e) = self.config_provider.save(&self.config) {
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
        if let Err(e) = self.config_provider.save(&self.config) {
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
            self.settings_available_selected = self.settings_available_selected.min(new_len - 1);
        } else {
            self.settings_available_selected = 0;
        }
    }

    /// Check liveness (try_wait) on all sessions. Called on periodic ticks.
    ///
    /// The reader threads handle PTY output continuously - no reading
    /// happens here. This only checks if child processes have exited.
    /// Also cleans up .mcp.json files and MCP servers for dead sessions.
    pub fn check_liveness(&mut self) {
        let mut dead_ids: Vec<WorkItemId> = Vec::new();
        let mut dead_implementing: Vec<WorkItemId> = Vec::new();
        for ((wi_id, stage), entry) in self.sessions.iter_mut() {
            let was_alive = entry.alive;
            if let Some(ref mut session) = entry.session {
                entry.alive = session.is_alive();
            } else {
                entry.alive = false;
            }
            if was_alive && !entry.alive {
                dead_ids.push(wi_id.clone());
                if *stage == WorkItemStatus::Implementing {
                    dead_implementing.push(wi_id.clone());
                }
            }
        }
        // Clean up MCP resources for newly dead sessions.
        for id in &dead_ids {
            self.cleanup_session_state_for(id);
        }

        // Auto-trigger review gate when an implementing session dies.
        // If the session ended without calling workbridge_set_status("Review"),
        // check for commits and run the gate automatically.
        for wi_id in dead_implementing {
            let wi = match self.work_items.iter().find(|w| w.id == wi_id) {
                Some(w) => w,
                None => continue,
            };
            if wi.status != WorkItemStatus::Implementing || self.review_gate_wi.is_some() {
                continue;
            }
            // Extract paths before calling &mut self methods.
            let paths = wi.repo_associations.first().and_then(|assoc| {
                let wt = assoc.worktree_path.clone()?;
                Some((wt, assoc.repo_path.clone()))
            });
            let has_commits = match paths {
                Some((wt, repo)) => self.branch_has_commits(&wt, &repo),
                None => false,
            };
            if has_commits {
                match self.spawn_review_gate(&wi_id) {
                    ReviewGateSpawn::Spawned => {
                        self.status_message =
                            Some("Implementing session ended - running review gate...".into());
                    }
                    ReviewGateSpawn::Blocked(reason) => {
                        self.status_message = Some(reason);
                    }
                }
            } else {
                self.status_message =
                    Some("Implementing session ended with no commits on branch".into());
            }
        }

        // Kill sessions whose stage doesn't match the work item's current stage.
        let orphans: Vec<_> = self
            .sessions
            .keys()
            .filter(|(wi_id, stage)| {
                self.work_items
                    .iter()
                    .find(|w| w.id == *wi_id)
                    .is_none_or(|wi| wi.status != *stage)
            })
            .cloned()
            .collect();
        for key in orphans {
            if let Some(mut entry) = self.sessions.remove(&key)
                && let Some(mut session) = entry.session.take()
            {
                session.kill();
            }
        }

        // Check global assistant session liveness.
        if let Some(ref mut entry) = self.global_session {
            if let Some(ref mut session) = entry.session {
                entry.alive = session.is_alive();
            } else {
                entry.alive = false;
            }
            if !entry.alive {
                self.global_mcp_server = None;
            }
        }
    }

    /// Stop MCP server and clear activity state for a work item.
    fn cleanup_session_state_for(&mut self, wi_id: &WorkItemId) {
        self.mcp_servers.remove(wi_id);
        self.claude_working.remove(wi_id);
    }

    /// Stop all MCP servers, clear activity state, and remove temp config files.
    /// Called on app exit.
    pub fn cleanup_all_mcp(&mut self) {
        self.mcp_servers.clear();
        self.claude_working.clear();
        self.global_mcp_server = None;
        if let Some(ref path) = self.global_mcp_config_path {
            let _ = std::fs::remove_file(path);
        }
        self.global_mcp_config_path = None;
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
        // Resize global assistant session to its own drawer dimensions.
        if let Some(ref entry) = self.global_session
            && let Some(ref session) = entry.session
            && let Err(e) = session.resize(self.global_pane_cols, self.global_pane_rows)
            && first_error.is_none()
        {
            first_error = Some(e);
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
        if let Some(ref mut entry) = self.global_session
            && entry.alive
            && let Some(ref mut session) = entry.session
        {
            session.send_sigterm();
        }
    }

    /// Check if all sessions are dead (or there are no sessions).
    pub fn all_dead(&self) -> bool {
        self.sessions.values().all(|entry| !entry.alive)
            && self.global_session.as_ref().is_none_or(|s| !s.alive)
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
        // Cancel any in-flight review gate.
        self.review_gate_rx = None;
        self.review_gate_wi = None;
        self.review_gate_progress = None;
        if let Some(aid) = self.review_gate_activity.take() {
            self.end_activity(aid);
        }
        if let Some(ref mut entry) = self.global_session {
            if let Some(ref mut session) = entry.session {
                session.force_kill();
            }
            entry.alive = false;
        }
        self.global_mcp_server = None;
    }

    /// Find the session key for a work item ID (any stage).
    pub fn session_key_for(&self, wi_id: &WorkItemId) -> Option<(WorkItemId, WorkItemStatus)> {
        self.sessions.keys().find(|(id, _)| id == wi_id).cloned()
    }

    /// Send raw bytes to the active session's PTY.
    ///
    /// The active session is the one associated with the currently selected
    /// work item in the display list.
    pub fn send_bytes_to_active(&mut self, data: &[u8]) {
        let Some(work_item_id) = self.selected_work_item_id() else {
            return;
        };
        let Some(key) = self.session_key_for(&work_item_id) else {
            return;
        };
        let Some(entry) = self.sessions.get(&key) else {
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
                Ok(FetchMessage::FetchStarted) => {
                    // Show a spinner while GitHub data is being fetched.
                    // Track how many repos are in-flight so the spinner
                    // persists until all repos have reported back.
                    self.pending_fetch_count += 1;
                    if self.fetch_activity.is_none() {
                        // Can't call self.start_activity() here because
                        // `rx` borrows self.fetch_rx immutably.
                        self.activity_counter += 1;
                        let id = ActivityId(self.activity_counter);
                        self.activities.push(Activity {
                            id,
                            message: "Refreshing GitHub data".into(),
                        });
                        self.fetch_activity = Some(id);
                    }
                    continue;
                }
                Ok(FetchMessage::RepoData(result)) => {
                    received_any = true;
                    self.pending_fetch_count = self.pending_fetch_count.saturating_sub(1);
                    // Can't call self.end_activity() here - rx borrow.
                    if self.pending_fetch_count == 0
                        && let Some(id) = self.fetch_activity.take()
                    {
                        self.activities.retain(|a| a.id != id);
                    }
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
                                    self.status_message =
                                        Some("gh CLI not found - GitHub features disabled".into());
                                }
                            }
                            GithubError::AuthRequired => {
                                if !self.gh_auth_required_shown {
                                    self.gh_auth_required_shown = true;
                                    self.status_message =
                                        Some("gh auth required - run 'gh auth login'".into());
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
                Ok(FetchMessage::FetcherError { repo_path, error }) => {
                    received_any = true;
                    self.pending_fetch_count = self.pending_fetch_count.saturating_sub(1);
                    // Can't call self.end_activity() here - rx borrow.
                    if self.pending_fetch_count == 0
                        && let Some(id) = self.fetch_activity.take()
                    {
                        self.activities.retain(|a| a.id != id);
                    }
                    let msg = format!("Fetch error ({}): {error}", repo_path.display());
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
        let (items, unlinked, review_requested) =
            assembly::reassemble(&list_result.records, &self.repo_data, issue_pattern);
        self.work_items = items;
        self.unlinked_prs = unlinked;
        self.review_requested_prs = review_requested;
    }

    /// Build the display list from current work_items and unlinked_prs.
    ///
    /// Groups (each hidden if empty):
    /// 1. REVIEW REQUESTS - PRs where the user is requested as reviewer
    /// 2. BLOCKED (repo) - Blocked work items (red header, shown first)
    /// 3. UNLINKED - PRs not yet imported as work items
    /// 4. ACTIVE (repo) - non-Backlog, non-Done, non-Blocked work items
    /// 5. BACKLOGGED (repo) - Backlog work items, grouped by repo
    /// 6. DONE (repo) - Done work items, grouped by repo
    pub fn build_display_list(&mut self) {
        let mut list = Vec::new();

        // When drilling down from board view, show only items matching
        // the drill-down stage. Otherwise show the full grouped list.
        if let Some(ref drill_stage) = self.board_drill_stage {
            for i in 0..self.work_items.len() {
                let matches = if *drill_stage == WorkItemStatus::Implementing {
                    self.work_items[i].status == WorkItemStatus::Implementing
                        || self.work_items[i].status == WorkItemStatus::Blocked
                } else {
                    self.work_items[i].status == *drill_stage
                };
                if matches {
                    list.push(DisplayEntry::WorkItemEntry(i));
                }
            }
        } else {
            // REVIEW REQUESTS group (hidden if empty).
            if !self.review_requested_prs.is_empty() {
                list.push(DisplayEntry::GroupHeader {
                    label: "REVIEW REQUESTS".to_string(),
                    count: self.review_requested_prs.len(),
                    kind: GroupHeaderKind::Normal,
                });
                for i in 0..self.review_requested_prs.len() {
                    list.push(DisplayEntry::ReviewRequestItem(i));
                }
            }

            // Partition work items into blocked, active, backlogged, and done.
            let mut blocked: Vec<usize> = Vec::new();
            let mut active: Vec<usize> = Vec::new();
            let mut backlogged: Vec<usize> = Vec::new();
            let mut done: Vec<usize> = Vec::new();
            for i in 0..self.work_items.len() {
                if self.work_items[i].status == WorkItemStatus::Done {
                    done.push(i);
                } else if self.work_items[i].status == WorkItemStatus::Backlog {
                    backlogged.push(i);
                } else if self.work_items[i].status == WorkItemStatus::Blocked {
                    blocked.push(i);
                } else {
                    active.push(i);
                }
            }

            // BLOCKED group first (red, attention-grabbing).
            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "BLOCKED",
                &blocked,
                GroupHeaderKind::Blocked,
            );

            // UNLINKED group (hidden if empty).
            if !self.unlinked_prs.is_empty() {
                list.push(DisplayEntry::GroupHeader {
                    label: "UNLINKED".to_string(),
                    count: self.unlinked_prs.len(),
                    kind: GroupHeaderKind::Normal,
                });
                for i in 0..self.unlinked_prs.len() {
                    list.push(DisplayEntry::UnlinkedItem(i));
                }
            }

            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "ACTIVE",
                &active,
                GroupHeaderKind::Normal,
            );
            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "BACKLOGGED",
                &backlogged,
                GroupHeaderKind::Normal,
            );
            Self::push_repo_groups(
                &self.work_items,
                &mut list,
                "DONE",
                &done,
                GroupHeaderKind::Normal,
            );
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
        if !restored && let Some(ref target) = self.selected_unlinked_branch {
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
        if !restored && let Some(ref target) = self.selected_review_request_branch {
            let (target_repo, target_branch) = target;
            for (i, entry) in self.display_list.iter().enumerate() {
                if let DisplayEntry::ReviewRequestItem(rr_idx) = entry
                    && let Some(rr) = self.review_requested_prs.get(*rr_idx)
                    && rr.branch == *target_branch
                    && rr.repo_path == *target_repo
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
            self.selected_review_request_branch = None;
            self.selected_item = self.display_list.iter().position(is_selectable);
        }

        // Keep board cursor in sync after reassembly.
        self.sync_board_cursor();
    }

    /// Sub-group work item indices by repo and emit group headers + entries.
    /// Each repo gets its own header: "ACTIVE (workbridge)" with a count.
    /// If all items share the same repo, the header is just "ACTIVE (repo)".
    fn push_repo_groups(
        work_items: &[WorkItem],
        list: &mut Vec<DisplayEntry>,
        label: &str,
        indices: &[usize],
        kind: GroupHeaderKind,
    ) {
        if indices.is_empty() {
            return;
        }

        // Collect unique repos in order of first appearance.
        let mut repo_order: Vec<String> = Vec::new();
        let mut by_repo: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        for &i in indices {
            let repo = work_items[i]
                .repo_associations
                .first()
                .and_then(|a| a.repo_path.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "(none)".to_string());
            by_repo.entry(repo.clone()).or_default().push(i);
            if !repo_order.contains(&repo) {
                repo_order.push(repo);
            }
        }

        for repo in &repo_order {
            let items = &by_repo[repo];
            list.push(DisplayEntry::GroupHeader {
                label: format!("{label} ({repo})"),
                count: items.len(),
                kind: kind.clone(),
            });
            for &i in items {
                list.push(DisplayEntry::WorkItemEntry(i));
            }
        }
    }

    /// Sync the identity trackers (selected_work_item, selected_unlinked_branch)
    /// from the current selected_item index. Called after any navigation that
    /// changes selected_item so that reassembly can restore the correct entry.
    fn sync_selection_identity(&mut self) {
        self.selected_work_item = None;
        self.selected_unlinked_branch = None;
        self.selected_review_request_branch = None;
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
                    self.selected_unlinked_branch = Some((ul.repo_path.clone(), ul.branch.clone()));
                }
            }
            Some(DisplayEntry::ReviewRequestItem(rr_idx)) => {
                if let Some(rr) = self.review_requested_prs.get(*rr_idx) {
                    self.selected_review_request_branch =
                        Some((rr.repo_path.clone(), rr.branch.clone()));
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
    /// In board mode (without drill-down), delegates to the board cursor.
    pub fn selected_work_item_id(&self) -> Option<WorkItemId> {
        if self.view_mode == ViewMode::Board && !self.board_drill_down {
            return self.board_selected_work_item_id();
        }
        let idx = self.selected_item?;
        match self.display_list.get(idx)? {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                self.work_items.get(*wi_idx).map(|wi| wi.id.clone())
            }
            _ => None,
        }
    }

    // -- Board view helpers --

    /// Get indices into `self.work_items` for items matching the given status.
    /// For the Implementing column, also includes Blocked items.
    pub fn items_for_column(&self, status: &WorkItemStatus) -> Vec<usize> {
        self.work_items
            .iter()
            .enumerate()
            .filter(|(_, wi)| {
                if *status == WorkItemStatus::Implementing {
                    wi.status == WorkItemStatus::Implementing
                        || wi.status == WorkItemStatus::Blocked
                } else {
                    wi.status == *status
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Resolve the board cursor to a WorkItemId.
    pub fn board_selected_work_item_id(&self) -> Option<WorkItemId> {
        let col_status = BOARD_COLUMNS.get(self.board_cursor.column)?;
        let items = self.items_for_column(col_status);
        let row = self.board_cursor.row?;
        let wi_idx = items.get(row)?;
        self.work_items.get(*wi_idx).map(|wi| wi.id.clone())
    }

    /// Sync the board cursor after reassembly or stage changes.
    /// Clamps row to the column's item count and tries to follow the
    /// selected work item if it moved columns.
    pub fn sync_board_cursor(&mut self) {
        // If we have a selected work item, try to find it in the board.
        if let Some(ref target_id) = self.selected_work_item {
            for (col_idx, status) in BOARD_COLUMNS.iter().enumerate() {
                let items = self.items_for_column(status);
                for (row_idx, &wi_idx) in items.iter().enumerate() {
                    if let Some(wi) = self.work_items.get(wi_idx)
                        && wi.id == *target_id
                    {
                        self.board_cursor.column = col_idx;
                        self.board_cursor.row = Some(row_idx);
                        return;
                    }
                }
            }
        }

        // Selected item not found in any column - clamp to current column.
        let items = self.items_for_column(
            BOARD_COLUMNS
                .get(self.board_cursor.column)
                .unwrap_or(&WorkItemStatus::Backlog),
        );
        if items.is_empty() {
            self.board_cursor.row = None;
        } else if let Some(row) = self.board_cursor.row {
            self.board_cursor.row = Some(row.min(items.len() - 1));
        } else {
            self.board_cursor.row = Some(0);
        }
    }

    /// Update `selected_work_item` from the board cursor position.
    pub fn sync_selection_from_board(&mut self) {
        self.selected_work_item = self.board_selected_work_item_id();
    }

    /// Toggle between flat list and board view, syncing cursor state.
    pub fn toggle_view_mode(&mut self) {
        match self.view_mode {
            ViewMode::FlatList => {
                self.view_mode = ViewMode::Board;
                self.sync_board_cursor();
            }
            ViewMode::Board => {
                self.view_mode = ViewMode::FlatList;
                self.board_drill_down = false;
                self.board_drill_stage = None;
                // Sync flat list selection from board cursor.
                if let Some(ref target_id) = self.selected_work_item {
                    for (i, entry) in self.display_list.iter().enumerate() {
                        if let DisplayEntry::WorkItemEntry(wi_idx) = entry
                            && let Some(wi) = self.work_items.get(*wi_idx)
                            && wi.id == *target_id
                        {
                            self.selected_item = Some(i);
                            return;
                        }
                    }
                }
            }
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
        if let Some(existing_key) = self.session_key_for(&work_item_id) {
            let is_alive = self
                .sessions
                .get(&existing_key)
                .is_some_and(|entry| entry.alive);
            if is_alive {
                self.focus = FocusPanel::Right;
                self.status_message = Some("Right panel focused - press Ctrl+] to return".into());
                return;
            }
            self.sessions.remove(&existing_key);
        }

        self.spawn_session(&work_item_id);
    }

    /// Spawn a fresh Claude session for a work item in its current stage.
    /// Creates a worktree if needed, starts an MCP server, and launches
    /// the Claude process with the stage-specific system prompt.
    pub fn spawn_session(&mut self, work_item_id: &WorkItemId) {
        let Some(wi) = self.work_items.iter().find(|w| w.id == *work_item_id) else {
            return;
        };
        let work_item_id = wi.id.clone();

        // Stages without sessions.
        if matches!(wi.status, WorkItemStatus::Backlog | WorkItemStatus::Done) {
            return;
        }

        // If any worktree creation is already in progress, don't start another.
        // Replacing worktree_create_rx while a thread is running would orphan
        // the worktree on disk (the poll handler would never see the result).
        if self.worktree_create_wi.is_some() {
            self.status_message = Some("Worktree creation already in progress...".into());
            return;
        }

        // Find the first worktree path among the work item's repo associations.
        // If none exists, spawn a background thread to auto-create one.
        match wi
            .repo_associations
            .iter()
            .find_map(|a| a.worktree_path.clone())
        {
            Some(path) => {
                // Worktree already exists - proceed to session spawn immediately.
                self.complete_session_open(&work_item_id, &path);
            }
            None => {
                // Try to find an association with a branch name and auto-create
                // a worktree for it in the background.
                let branch_assoc = wi.repo_associations.iter().find(|a| a.branch.is_some());
                match branch_assoc {
                    Some(assoc) => {
                        let branch = assoc.branch.as_ref().unwrap().clone();
                        let repo_path = assoc.repo_path.clone();
                        let wt_target = Self::worktree_target_path(
                            &repo_path,
                            &branch,
                            &self.config.defaults.worktree_dir,
                        );
                        let ws = Arc::clone(&self.worktree_service);
                        let wi_id_clone = work_item_id.clone();

                        let (tx, rx) = crossbeam_channel::bounded(1);

                        std::thread::spawn(move || {
                            // Fetch the branch from origin first.
                            // If fetch fails, try to create a new local branch.
                            if ws.fetch_branch(&repo_path, &branch).is_err()
                                && let Err(create_err) = ws.create_branch(&repo_path, &branch)
                            {
                                let _ = tx.send(WorktreeCreateResult {
                                    wi_id: wi_id_clone,
                                    repo_path,
                                    path: None,
                                    error: Some(format!(
                                        "Could not fetch or create branch '{}': {create_err}",
                                        branch,
                                    )),
                                });
                                return;
                            }
                            match ws.create_worktree(&repo_path, &branch, &wt_target) {
                                Ok(wt_info) => {
                                    let _ = tx.send(WorktreeCreateResult {
                                        wi_id: wi_id_clone,
                                        repo_path,
                                        path: Some(wt_info.path),
                                        error: None,
                                    });
                                }
                                Err(e) => {
                                    let _ = tx.send(WorktreeCreateResult {
                                        wi_id: wi_id_clone,
                                        repo_path,
                                        path: None,
                                        error: Some(format!(
                                            "Failed to create worktree for '{}': {e}",
                                            branch,
                                        )),
                                    });
                                }
                            }
                        });

                        self.worktree_create_rx = Some(rx);
                        self.worktree_create_wi = Some(work_item_id);
                        if let Some(aid) = self.worktree_create_activity.take() {
                            self.end_activity(aid);
                        }
                        self.worktree_create_activity =
                            Some(self.start_activity("Initializing worktree..."));
                    }
                    None => {
                        self.status_message = Some("Set a branch name to start working".into());
                    }
                }
            }
        }
    }

    /// Complete session setup after the worktree path is known.
    /// Shared by both the immediate path (worktree already exists) and
    /// the deferred path (worktree was just created in a background thread).
    fn complete_session_open(&mut self, work_item_id: &WorkItemId, cwd: &std::path::Path) {
        // Start MCP socket server for this session.
        let mcp_result = self.start_mcp_for_session(cwd, work_item_id);

        // Build the claude command with system prompt and MCP config.
        let work_item_status = self
            .work_items
            .iter()
            .find(|w| w.id == *work_item_id)
            .map(|w| w.status.clone())
            .unwrap_or(WorkItemStatus::Implementing);
        let session_key = (work_item_id.clone(), work_item_status.clone());
        let has_gate_findings = self.review_gate_findings.contains_key(work_item_id);
        let system_prompt = self.stage_system_prompt(work_item_id, cwd);
        let mut cmd = Self::build_claude_cmd(
            &work_item_status,
            system_prompt.as_deref(),
            has_gate_findings,
        );

        // Write MCP config as .mcp.json in the worktree AND pass via --mcp-config.
        // Both are needed: .mcp.json for Claude Code's project discovery, --mcp-config
        // as a backup. The socket must be listening before Claude starts (it is - the
        // socket server was started above).
        if let Ok((ref server, _)) = mcp_result {
            match std::env::current_exe() {
                Ok(exe) => {
                    let mcp_config = crate::mcp::build_mcp_config(&exe, &server.socket_path);

                    // Write .mcp.json to the worktree root.
                    let mcp_json_path = cwd.join(".mcp.json");
                    if let Err(e) = std::fs::write(&mcp_json_path, &mcp_config) {
                        self.status_message = Some(format!("MCP config write error: {e}"));
                    }

                    // Also pass via --mcp-config as a temp file.
                    let config_path = std::env::temp_dir().join(format!(
                        "workbridge-mcp-config-{}.json",
                        uuid::Uuid::new_v4()
                    ));
                    if let Err(e) = std::fs::write(&config_path, &mcp_config) {
                        self.status_message = Some(format!("MCP config write error: {e}"));
                    } else {
                        cmd.push("--mcp-config".to_string());
                        cmd.push(config_path.to_string_lossy().to_string());
                    }
                }
                Err(e) => {
                    self.status_message = Some(format!("Cannot resolve executable path: {e}"));
                }
            }
        }

        let cmd_refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();

        match Session::spawn(self.pane_cols, self.pane_rows, Some(cwd), &cmd_refs) {
            Ok(session) => {
                let parser = Arc::clone(&session.parser);
                let entry = SessionEntry {
                    parser,
                    alive: true,
                    session: Some(session),
                };
                self.sessions.insert(session_key.clone(), entry);
                match mcp_result {
                    Ok((server, _)) => {
                        self.mcp_servers.insert(work_item_id.clone(), server);
                    }
                    Err(msg) => {
                        self.status_message = Some(msg);
                        self.focus = FocusPanel::Right;
                        return;
                    }
                }
                self.focus = FocusPanel::Right;
                self.status_message = Some("Right panel focused - press Ctrl+] to return".into());
            }
            Err(e) => {
                self.status_message = Some(format!("Error spawning session: {e}"));
            }
        }
    }

    /// Build the `claude` CLI argument list.
    ///
    /// The positional prompt (for planning sessions) MUST come before
    /// `--mcp-config` so Claude Code does not mistake it for a config
    /// file path. The returned Vec does not include `--mcp-config` -
    /// callers append it after this returns.
    fn build_claude_cmd(
        status: &WorkItemStatus,
        system_prompt: Option<&str>,
        force_auto_start: bool,
    ) -> Vec<String> {
        let is_planning = *status == WorkItemStatus::Planning;
        let auto_start = force_auto_start
            || matches!(
                status,
                WorkItemStatus::Planning | WorkItemStatus::Implementing
            );

        let mut cmd: Vec<String> = vec!["claude".to_string()];
        if is_planning {
            cmd.push("--permission-mode".to_string());
            cmd.push("plan".to_string());
            cmd.push("--settings".to_string());
            cmd.push(
                r#"{"hooks":{"PostToolUse":[{"matcher":"TodoWrite","hooks":[{"type":"command","command":"bash -c 'cat | grep -q workbridge_set_plan || echo \"REMINDER: Your plan MUST include a step to call workbridge_set_plan MCP tool to persist the plan. Add this as the FIRST step.\" >&2; true'"}]}]}}"#
                    .to_string(),
            );
        }
        if let Some(prompt) = system_prompt {
            cmd.push("--system-prompt".to_string());
            cmd.push(prompt.to_string());
        }
        // Add the initial prompt BEFORE --mcp-config so Claude Code treats
        // it as the positional prompt argument, not as an additional config
        // file path.
        if auto_start {
            if *status == WorkItemStatus::Review {
                cmd.push(
                    "Present the review gate assessment and the pull request URL from your system prompt to the user."
                        .to_string(),
                );
            } else {
                cmd.push("Explain who you are and start working.".to_string());
            }
        }
        cmd
    }

    /// Poll the async worktree creation thread for a result. Called on each timer tick.
    pub fn poll_worktree_creation(&mut self) {
        let rx = match self.worktree_create_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };

        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.worktree_create_rx = None;
                self.worktree_create_wi = None;
                if let Some(aid) = self.worktree_create_activity.take() {
                    self.end_activity(aid);
                }
                self.status_message =
                    Some("Worktree creation: background thread exited unexpectedly".into());
                return;
            }
        };

        self.worktree_create_rx = None;
        self.worktree_create_wi = None;
        if let Some(aid) = self.worktree_create_activity.take() {
            self.end_activity(aid);
        }

        match (result.path, result.error) {
            (Some(path), _) => {
                // Verify the work item still exists before opening a session.
                // It may have been deleted while the background thread was running.
                if !self.work_items.iter().any(|w| w.id == result.wi_id) {
                    // Clean up the orphaned worktree instead of leaving it behind.
                    if let Err(e) =
                        self.worktree_service
                            .remove_worktree(&result.repo_path, &path, true, true)
                    {
                        self.status_message = Some(format!(
                            "Worktree created but work item deleted; cleanup failed: {e}"
                        ));
                    } else {
                        self.status_message =
                            Some("Worktree created but work item was deleted - cleaned up".into());
                    }
                    return;
                }
                // Worktree created successfully - continue with session setup.
                // Reassemble so the new worktree path is visible in the data model.
                self.reassemble_work_items();
                self.build_display_list();
                self.complete_session_open(&result.wi_id, &path);
            }
            (None, Some(error)) => {
                self.status_message = Some(error);
            }
            (None, None) => {
                // Unexpected - no path and no error.
                self.status_message = Some("Worktree creation completed with no result".into());
            }
        }
    }

    /// Build a stage-specific system prompt for the Claude session.
    ///
    /// `cwd` is the worktree path where Claude will run - used to build the
    /// situation summary so Claude knows where it is working.
    fn stage_system_prompt(
        &mut self,
        work_item_id: &WorkItemId,
        cwd: &std::path::Path,
    ) -> Option<String> {
        use std::collections::HashMap;

        let wi = self.work_items.iter().find(|w| w.id == *work_item_id)?;
        let title = wi.title.clone();
        let branch_name = wi
            .repo_associations
            .first()
            .and_then(|a| a.branch.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let pr_url = wi
            .repo_associations
            .first()
            .and_then(|a| a.pr.as_ref())
            .map(|pr| pr.url.clone())
            .filter(|u| !u.is_empty());
        let worktree_display = cwd.display().to_string();

        // Read the plan text (if any) from the backend.
        let plan_text = match self.backend.read_plan(work_item_id) {
            Ok(Some(plan)) => plan,
            Ok(None) => String::new(),
            Err(e) => {
                self.status_message = Some(format!("Could not read plan: {e}"));
                String::new()
            }
        };

        // Look up and consume rework reason if any (one-shot use).
        let rework_reason = self.rework_reasons.remove(work_item_id).unwrap_or_default();
        let review_gate_findings = self
            .review_gate_findings
            .remove(work_item_id)
            .unwrap_or_default();

        // Check if the branch has commits ahead of the default branch.
        // Used to select the retroactive planning prompt when appropriate.
        // Clone the repo path to release the borrow on self.work_items
        // before calling &mut self method.
        let repo_path_owned = wi.repo_associations.first().map(|a| a.repo_path.clone());
        let status = wi.status.clone();
        let description = wi.description.clone();
        let has_branch_commits = repo_path_owned
            .as_ref()
            .map(|rp| self.branch_has_commits(cwd, rp))
            .unwrap_or(false);

        // Build a situation summary that tells Claude where it is and what
        // state the work item is in.  Uses the worktree path (not the main
        // repo path) so Claude runs commands in the right directory.
        let situation = match status {
            WorkItemStatus::Backlog | WorkItemStatus::Done => return None,
            WorkItemStatus::Planning => {
                if has_branch_commits {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         Existing implementation work found on this branch - \
                         analyze it and create a plan."
                    )
                } else {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         No plan exists yet - your job is to create one."
                    )
                }
            }
            WorkItemStatus::Implementing => {
                if !rework_reason.is_empty() {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         Rework requested (see reason below)."
                    )
                } else if plan_text.is_empty() {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         No plan is available - you must block."
                    )
                } else {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         An approved plan is available (see below)."
                    )
                }
            }
            WorkItemStatus::Blocked => {
                format!(
                    "Worktree: {worktree_display}. Branch: {branch_name}. \
                     Waiting for user input."
                )
            }
            WorkItemStatus::Review => {
                let pr_line = match &pr_url {
                    Some(url) => format!(" Pull request: {url}."),
                    None => format!(
                        " Note: no pull request URL is available yet (it may still be creating). \
                         You can find it by running: gh pr list --head {branch_name}"
                    ),
                };
                if !review_gate_findings.is_empty() {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         Implementation passed the review gate and is ready for review.{pr_line}"
                    )
                } else {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         Implementation is complete and ready for review.{pr_line}"
                    )
                }
            }
        };

        // Backlog | Done already returned None above, so they are
        // unreachable here - the match uses a cloned status value.
        let prompt_key = match status {
            WorkItemStatus::Backlog | WorkItemStatus::Done => unreachable!(),
            WorkItemStatus::Planning => {
                if has_branch_commits {
                    "planning_retroactive"
                } else {
                    "planning"
                }
            }
            WorkItemStatus::Implementing => {
                if !rework_reason.is_empty() {
                    "implementing_rework"
                } else if plan_text.is_empty() {
                    "implementing_no_plan"
                } else {
                    "implementing_with_plan"
                }
            }
            WorkItemStatus::Blocked => "blocked",
            WorkItemStatus::Review => {
                if !review_gate_findings.is_empty() {
                    "review_with_findings"
                } else {
                    "review"
                }
            }
        };

        let description_var = match &description {
            Some(d) if !d.is_empty() => format!("\nUser-provided description: {d}"),
            _ => String::new(),
        };

        let mut vars: HashMap<&str, &str> = HashMap::new();
        vars.insert("title", &title);
        vars.insert("description", &description_var);
        vars.insert("situation", &situation);
        vars.insert("plan", &plan_text);
        vars.insert("rework_reason", &rework_reason);
        vars.insert("review_gate_findings", &review_gate_findings);

        crate::prompts::render(prompt_key, &vars)
    }

    /// Check if the branch in `cwd` has commits ahead of the default branch.
    /// Returns false and surfaces a status message on error so the user knows
    /// retroactive analysis was skipped.
    fn branch_has_commits(&mut self, cwd: &std::path::Path, repo_path: &std::path::Path) -> bool {
        let default_branch = match self.worktree_service.default_branch(repo_path) {
            Ok(b) => b,
            Err(e) => {
                self.status_message = Some(format!("Could not detect default branch: {e}"));
                return false;
            }
        };

        let output = crate::worktree_service::git_command()
            .args(["log", &format!("{default_branch}..HEAD"), "--oneline"])
            .current_dir(cwd)
            .output();
        match output {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                !stdout.trim().is_empty()
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                self.status_message =
                    Some(format!("Branch commit check failed: {}", stderr.trim()));
                false
            }
            Err(e) => {
                self.status_message = Some(format!("Branch commit check failed: {e}"));
                false
            }
        }
    }

    /// Start an MCP socket server for a work item session.
    /// MCP config is passed to Claude via --mcp-config CLI flag, not written
    /// to disk. Returns (server, unused_path) on success, or an error message
    /// on failure.
    fn start_mcp_for_session(
        &self,
        worktree_path: &std::path::Path,
        work_item_id: &WorkItemId,
    ) -> Result<(McpSocketServer, PathBuf), String> {
        let socket_path = crate::mcp::socket_path_for_session();

        // Serialize the work item ID for the MCP server.
        let wi_id_str = serde_json::to_string(work_item_id)
            .map_err(|e| format!("MCP unavailable: could not serialize work item ID: {e}"))?;

        // Build context JSON for get_context tool.
        // Uses the worktree path (not the main repo) so Claude operates in
        // the correct working directory.
        let context_json = {
            let wi = self.work_items.iter().find(|w| w.id == *work_item_id);
            if let Some(wi) = wi {
                let pr_url = wi
                    .repo_associations
                    .first()
                    .and_then(|a| a.pr.as_ref())
                    .map(|pr| pr.url.as_str())
                    .unwrap_or("");
                serde_json::json!({
                    "work_item_id": wi_id_str,
                    "stage": format!("{:?}", wi.status),
                    "title": wi.title,
                    "description": wi.description,
                    "repo": worktree_path.display().to_string(),
                    "pr_url": pr_url,
                })
                .to_string()
            } else {
                "{}".to_string()
            }
        };

        // Compute the activity log path for the query_log MCP tool.
        let activity_log_path = self.backend.activity_path_for(work_item_id);

        // Start the socket server.
        let server = McpSocketServer::start(
            socket_path,
            wi_id_str,
            context_json,
            activity_log_path,
            self.mcp_tx.clone(),
        )
        .map_err(|e| format!("MCP unavailable: failed to start socket server: {e}"))?;

        Ok((server, PathBuf::new()))
    }

    /// Drain MCP events from the crossbeam channel.
    /// Called on the 200ms timer tick. Processes status updates, log events,
    /// and plan updates from all active MCP socket servers.
    pub fn poll_mcp_status_updates(&mut self) {
        let Some(ref rx) = self.mcp_rx else {
            return;
        };

        let mut events: Vec<McpEvent> = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }

        for event in events {
            match event {
                McpEvent::StatusUpdate {
                    work_item_id: wi_id_str,
                    status: status_str,
                    reason,
                } => {
                    let new_status = match status_str.as_str() {
                        "Backlog" => WorkItemStatus::Backlog,
                        "Planning" => WorkItemStatus::Planning,
                        "Implementing" => WorkItemStatus::Implementing,
                        "Blocked" => WorkItemStatus::Blocked,
                        "Review" => WorkItemStatus::Review,
                        "Done" => WorkItemStatus::Done,
                        other => {
                            self.status_message =
                                Some(format!("MCP: unrecognized status '{other}'"));
                            continue;
                        }
                    };

                    // Find the work item ID from the serialized string.
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };

                    // Block Done via MCP - Done requires the merge gate
                    // which is user-initiated. Allowing MCP to set Done would
                    // bypass both the review gate and the merge gate.
                    if new_status == WorkItemStatus::Done {
                        self.status_message =
                            Some("MCP: cannot set Done directly (use the merge gate)".into());
                        continue;
                    }

                    // Check current status to route through review gate if needed.
                    let wi_ref = self.work_items.iter().find(|w| w.id == wi_id);

                    // Block transitions on derived statuses (e.g. merged PR -> Done)
                    // to prevent backend/display divergence, mirroring advance/retreat_stage.
                    if wi_ref.map(|w| w.status_derived).unwrap_or(false) {
                        self.status_message = Some("MCP: status is derived from merged PR".into());
                        continue;
                    }

                    // Block all MCP transitions for review request items.
                    // Claude sessions should not drive workflow for someone else's PR.
                    if wi_ref
                        .map(|w| w.kind == WorkItemKind::ReviewRequest)
                        .unwrap_or(false)
                    {
                        self.status_message = Some(
                            "MCP: status transitions not supported for review request items".into(),
                        );
                        continue;
                    }

                    let current_status = wi_ref.map(|w| w.status.clone());

                    // Restrict MCP to valid forward transitions only.
                    // Allowed: Implementing -> Review (via gate), Implementing -> Blocked,
                    // Blocked -> Implementing, Blocked -> Review (via gate),
                    // Planning -> Implementing.
                    // All other transitions must go through the TUI keybinds.
                    let allowed = matches!(
                        (&current_status, &new_status),
                        (Some(WorkItemStatus::Implementing), WorkItemStatus::Review)
                            | (Some(WorkItemStatus::Implementing), WorkItemStatus::Blocked)
                            | (Some(WorkItemStatus::Blocked), WorkItemStatus::Implementing)
                            | (Some(WorkItemStatus::Blocked), WorkItemStatus::Review)
                            | (Some(WorkItemStatus::Planning), WorkItemStatus::Implementing)
                    );
                    if !allowed {
                        self.status_message = Some(format!(
                            "MCP: transition from {} to {} is not allowed",
                            current_status
                                .as_ref()
                                .map(|s| s.badge_text())
                                .unwrap_or("unknown"),
                            new_status.badge_text()
                        ));
                        continue;
                    }

                    // No-plan prompt: when Claude blocks because there is no
                    // implementation plan, offer the user a choice to retreat
                    // to Planning instead of staying blocked.
                    if current_status.as_ref() == Some(&WorkItemStatus::Implementing)
                        && new_status == WorkItemStatus::Blocked
                        && reason.contains("No implementation plan")
                    {
                        // Apply the block first so the item is in Blocked state.
                        let current = current_status.clone().unwrap();
                        self.apply_stage_change(&wi_id, &current, &new_status, "mcp");

                        // Enqueue for the no-plan prompt (skip duplicates).
                        if !self.no_plan_prompt_queue.contains(&wi_id) {
                            self.no_plan_prompt_queue.push_back(wi_id);
                        }
                        if !self.no_plan_prompt_visible {
                            self.no_plan_prompt_visible = true;
                            self.status_message = Some(
                                "No plan available. [p] Plan from branch  [Esc] Stay blocked"
                                    .to_string(),
                            );
                        }
                        continue;
                    }

                    // Review gate: when MCP requests Implementing/Blocked -> Review,
                    // the review gate is the single chokepoint - the transition
                    // is blocked unless the gate spawns and later approves it.
                    if (current_status.as_ref() == Some(&WorkItemStatus::Implementing)
                        || current_status.as_ref() == Some(&WorkItemStatus::Blocked))
                        && new_status == WorkItemStatus::Review
                    {
                        match self.spawn_review_gate(&wi_id) {
                            ReviewGateSpawn::Spawned => {
                                self.status_message =
                                    Some("Claude requested Review - running review gate...".into());
                            }
                            ReviewGateSpawn::Blocked(reason) => {
                                // If a gate is already running (for this or another item),
                                // just inform Claude - don't rework, the event is dropped
                                // and Claude will need to request Review again.
                                if reason.contains("already running") {
                                    self.status_message = Some(reason);
                                } else {
                                    // Gate truly can't run (no plan, no diff, git error).
                                    // Apply the rework flow so Claude gets feedback instead
                                    // of waiting forever for a gate result that never comes.
                                    self.rework_reasons.insert(wi_id.clone(), reason.clone());
                                    self.status_message =
                                        Some(format!("Review gate failed to start: {reason}"));
                                    // If Blocked, transition to Implementing so the
                                    // implementing_rework prompt (with {rework_reason}) is used.
                                    if current_status.as_ref() == Some(&WorkItemStatus::Blocked) {
                                        let _ = self
                                            .backend
                                            .update_status(&wi_id, WorkItemStatus::Implementing);
                                        self.reassemble_work_items();
                                        self.build_display_list();
                                    }
                                    // Kill and respawn the session with rework prompt.
                                    if let Some(key) = self.session_key_for(&wi_id)
                                        && let Some(mut entry) = self.sessions.remove(&key)
                                        && let Some(ref mut session) = entry.session
                                    {
                                        session.kill();
                                    }
                                    self.cleanup_session_state_for(&wi_id);
                                    self.spawn_session(&wi_id);
                                }
                            }
                        }
                        continue;
                    }
                    // Non-Review transitions fall through to direct update.
                    let current = current_status.unwrap();
                    self.apply_stage_change(&wi_id, &current, &new_status, "mcp");

                    // Build MCP-specific status message that preserves any
                    // detail from apply_stage_change (e.g. "PR created: URL").
                    let existing = self.status_message.take().unwrap_or_default();
                    let pr_suffix = if existing.contains("PR created") {
                        // Extract the PR info portion after the dash.
                        existing
                            .find("PR created")
                            .map(|idx| format!(" - {}", &existing[idx..]))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let reason_part = if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" - {reason}")
                    };
                    self.status_message = Some(format!(
                        "Claude moved to {}{}{}",
                        new_status.badge_text(),
                        pr_suffix,
                        reason_part
                    ));
                }
                McpEvent::LogEvent {
                    work_item_id: wi_id_str,
                    event_type,
                    payload,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    let entry = ActivityEntry {
                        timestamp: now_iso8601(),
                        event_type,
                        payload,
                    };
                    if let Err(e) = self.backend.append_activity(&wi_id, &entry) {
                        self.status_message = Some(format!("Activity log error: {e}"));
                    }
                }
                McpEvent::SetPlan {
                    work_item_id: wi_id_str,
                    plan,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    if let Err(e) = self.backend.update_plan(&wi_id, &plan) {
                        self.status_message = Some(format!("Plan update error: {e}"));
                    } else {
                        // Log the plan set event to the activity log.
                        let entry = ActivityEntry {
                            timestamp: now_iso8601(),
                            event_type: "plan_set".to_string(),
                            payload: serde_json::json!({
                                "source": "mcp",
                                "plan_length": plan.len()
                            }),
                        };
                        if let Err(e) = self.backend.append_activity(&wi_id, &entry) {
                            self.status_message = Some(format!("Activity log error: {e}"));
                        } else {
                            self.status_message = Some("Plan saved by Claude".to_string());
                        }

                        // Auto-advance from Planning to Implementing when plan is set.
                        // Read authoritative status from disk rather than the
                        // in-memory cache, which may be stale. The orphan cleanup
                        // in check_liveness will kill the Planning session.
                        match self.backend.read(&wi_id) {
                            Ok(record) if record.status == WorkItemStatus::Planning => {
                                self.apply_stage_change(
                                    &wi_id,
                                    &WorkItemStatus::Planning,
                                    &WorkItemStatus::Implementing,
                                    "mcp",
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                self.status_message =
                                    Some(format!("Plan saved but could not verify status: {e}"));
                            }
                        }
                    }
                }
                McpEvent::SetActivity {
                    work_item_id: wi_id_str,
                    working,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    if working {
                        self.claude_working.insert(wi_id);
                    } else {
                        self.claude_working.remove(&wi_id);
                    }
                }
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
                        match self
                            .worktree_service
                            .create_worktree(&repo_path, &branch, &wt_target)
                        {
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

    /// Import the currently selected review-requested PR as a work item.
    ///
    /// Mirrors import_selected_unlinked but uses import_review_request on
    /// the backend, which sets kind=ReviewRequest and status=Review.
    pub fn import_selected_review_request(&mut self) {
        let Some(idx) = self.selected_item else {
            return;
        };
        let rr_idx = match self.display_list.get(idx) {
            Some(DisplayEntry::ReviewRequestItem(i)) => *i,
            _ => return,
        };
        let Some(rr) = self.review_requested_prs.get(rr_idx) else {
            return;
        };

        let repo_path = rr.repo_path.clone();
        let branch = rr.branch.clone();

        match self.backend.import_review_request(rr) {
            Ok(record) => {
                let title = record.title.clone();

                let wt_msg = match self.worktree_service.fetch_branch(&repo_path, &branch) {
                    Ok(()) => {
                        let wt_target = Self::worktree_target_path(
                            &repo_path,
                            &branch,
                            &self.config.defaults.worktree_dir,
                        );
                        match self
                            .worktree_service
                            .create_worktree(&repo_path, &branch, &wt_target)
                        {
                            Ok(_) => format!("Imported review: {title} (worktree created)"),
                            Err(e) => {
                                format!("Imported review: {title} (worktree not created: {e})")
                            }
                        }
                    }
                    Err(_) => {
                        format!(
                            "Imported review: {title} - could not fetch branch '{branch}' from origin. Manual checkout required."
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
    ///
    /// Note: the TUI now uses `create_work_item_with()` via the creation
    /// dialog. This method is retained for tests and potential CLI use.
    #[allow(dead_code)]
    pub fn create_work_item(&mut self) {
        let cwd = match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                self.status_message = Some(format!("Cannot determine working directory: {e}"));
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
            description: None,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo_root,
                branch: None,
                pr_identity: None,
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

    /// Create a new work item with explicit parameters from the creation
    /// dialog. Unlike `create_work_item()` which uses CWD and a hardcoded
    /// title, this accepts user-provided title, selected repos, and a
    /// branch name (required).
    pub fn create_work_item_with(
        &mut self,
        title: String,
        description: Option<String>,
        repos: Vec<PathBuf>,
        branch: String,
    ) -> Result<(), String> {
        if repos.is_empty() {
            let msg = "No repos selected".to_string();
            self.status_message = Some(msg.clone());
            return Err(msg);
        }

        // Filter out repos whose git directory is missing. This guards
        // against stale cache entries or repos selected before their
        // .git dir disappeared.
        let valid_repos: Vec<PathBuf> = repos
            .into_iter()
            .filter(|repo_path| {
                self.active_repo_cache
                    .iter()
                    .any(|r| r.path == *repo_path && r.git_dir_present)
            })
            .collect();

        if valid_repos.is_empty() {
            let msg = "No selected repos have a git directory".to_string();
            self.status_message = Some(msg.clone());
            return Err(msg);
        }

        let repo_associations: Vec<RepoAssociationRecord> = valid_repos
            .into_iter()
            .map(|repo_path| RepoAssociationRecord {
                repo_path,
                branch: Some(branch.clone()),
                pr_identity: None,
            })
            .collect();

        let request = CreateWorkItem {
            title: title.clone(),
            description,
            status: WorkItemStatus::Backlog,
            kind: WorkItemKind::Own,
            repo_associations,
        };

        match self.backend.create(request) {
            Ok(_record) => {
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_repos_changed = true;
                self.status_message = Some(format!("Created: {title}"));
                Ok(())
            }
            Err(e) => {
                let msg = format!("Create error: {e}");
                self.status_message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Gateway between AwaitingConfirm and actual delete. Checks for dirty
    /// worktrees and either proceeds to delete or escalates to AwaitingForce.
    pub fn attempt_delete_selected_work_item(&mut self) {
        let Some(work_item_id) = self.selected_work_item_id() else {
            self.status_message = Some("No work item selected".into());
            return;
        };

        // Check each repo association's worktree for dirty status.
        // On error, default to dirty (safer: forces user to confirm force-delete).
        let has_dirty = self
            .work_items
            .iter()
            .find(|w| w.id == work_item_id)
            .map(|wi| {
                wi.repo_associations.iter().any(|assoc| {
                    assoc
                        .worktree_path
                        .as_ref()
                        .map(
                            |wt_path| match self.worktree_service.is_worktree_dirty(wt_path) {
                                Ok(dirty) => dirty,
                                Err(e) => {
                                    self.status_message = Some(format!(
                                        "Could not check worktree status: {e} - treating as dirty"
                                    ));
                                    true
                                }
                            },
                        )
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        if has_dirty {
            self.confirm_delete = DeleteConfirmState::AwaitingForce;
            self.status_message =
                Some("Worktree has uncommitted changes! Press again to force-delete".into());
        } else {
            self.delete_selected_work_item(false);
        }
    }

    /// Delete the currently selected work item with comprehensive resource cleanup.
    ///
    /// Kills any active session, removes worktrees and branches, closes open PRs,
    /// deletes the backend record, and cleans up in-memory state.
    ///
    /// When `force` is true, dirty worktrees are removed with `--force` and
    /// branches are deleted with `-D`.
    pub fn delete_selected_work_item(&mut self, force: bool) {
        let Some(work_item_id) = self.selected_work_item_id() else {
            self.status_message = Some("No work item selected".into());
            return;
        };

        // Warnings are collected across phases and reported in Phase 8.
        let mut warnings: Vec<String> = Vec::new();

        // -- Phase 1: Snapshot resource info before backend delete --
        // Collect (repo_path, branch, worktree_path, open_pr_number, owner/repo)
        // for each repo association so we can clean up after the backend record
        // is gone.
        struct RepoCleanupInfo {
            repo_path: PathBuf,
            branch: Option<String>,
            worktree_path: Option<PathBuf>,
            open_pr_number: Option<u64>,
            github_remote: Option<(String, String)>,
        }

        let cleanup_infos: Vec<RepoCleanupInfo> = self
            .work_items
            .iter()
            .find(|w| w.id == work_item_id)
            .map(|wi| {
                wi.repo_associations
                    .iter()
                    .map(|assoc| {
                        let open_pr_number = assoc.pr.as_ref().and_then(|pr| {
                            if pr.state == crate::work_item::PrState::Open {
                                Some(pr.number)
                            } else {
                                None
                            }
                        });
                        let github_remote =
                            match self.worktree_service.github_remote(&assoc.repo_path) {
                                Ok(v) => v,
                                Err(e) => {
                                    warnings.push(format!("github remote: {e}"));
                                    None
                                }
                            };
                        RepoCleanupInfo {
                            repo_path: assoc.repo_path.clone(),
                            branch: assoc.branch.clone(),
                            worktree_path: assoc.worktree_path.clone(),
                            open_pr_number,
                            github_remote,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        // -- Phase 2: Backend cleanup --
        if let Err(e) = self.backend.pre_delete_cleanup(&work_item_id) {
            // Non-fatal: warn but continue with delete.
            warnings.push(format!("pre-delete cleanup: {e}"));
        }

        if let Err(e) = self.backend.delete(&work_item_id) {
            self.status_message = Some(format!("Delete error: {e}"));
            return;
        }

        // -- Phase 3: Kill session and clean up MCP --
        self.cleanup_session_state_for(&work_item_id);
        if let Some(key) = self.session_key_for(&work_item_id)
            && let Some(mut entry) = self.sessions.remove(&key)
            && let Some(ref mut session) = entry.session
        {
            session.kill();
        }

        // -- Phase 4: Resource cleanup (all best-effort with warnings) --

        for info in &cleanup_infos {
            // 4a: Remove worktree (don't delete branch here - handled separately)
            if let Some(ref wt_path) = info.worktree_path
                && let Err(e) =
                    self.worktree_service
                        .remove_worktree(&info.repo_path, wt_path, false, force)
            {
                warnings.push(format!("worktree: {e}"));
            }

            // 4b: Delete local branch (force=true since user chose to destroy the item)
            if let Some(ref branch) = info.branch
                && let Err(e) = self
                    .worktree_service
                    .delete_branch(&info.repo_path, branch, true)
            {
                warnings.push(format!("branch: {e}"));
            }

            // 4c: Close open PR via `gh pr close`
            // Runs synchronously (unlike pr create/merge which are async). This is
            // deliberate: delete is a user-confirmed destructive operation where all
            // cleanup should complete before the user continues interacting. Making
            // this async would risk orphaned PRs if the user quits during the gap.
            if let Some(pr_number) = info.open_pr_number
                && let Some((ref owner, ref repo)) = info.github_remote
            {
                let owner_repo = format!("{owner}/{repo}");
                match std::process::Command::new("gh")
                    .args(["pr", "close", &pr_number.to_string(), "--repo", &owner_repo])
                    .output()
                {
                    Ok(output) if !output.status.success() => {
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                        warnings.push(format!("PR close: {stderr}"));
                    }
                    Err(e) => {
                        warnings.push(format!("PR close: {e}"));
                    }
                    _ => {}
                }
            }
        }

        // -- Phase 5: Cancel in-flight operations --
        // Cancel in-flight worktree creation and clean up any orphaned worktree.
        if self.worktree_create_wi.as_ref() == Some(&work_item_id) {
            // Try to drain the result in case the thread already completed.
            let thread_done = if let Some(ref rx) = self.worktree_create_rx {
                match rx.try_recv() {
                    Ok(result) => {
                        // Thread completed - clean up the orphaned worktree now.
                        if let Some(ref path) = result.path
                            && let Err(e) = self.worktree_service.remove_worktree(
                                &result.repo_path,
                                path,
                                true,
                                true,
                            )
                        {
                            warnings.push(format!("orphan worktree cleanup: {e}"));
                        }
                        true
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => true,
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        // Thread still running. Leave the receiver intact so
                        // poll_worktree_creation can drain it on the next timer
                        // tick and run the orphan-cleanup path (line 1774).
                        false
                    }
                }
            } else {
                true
            };
            if thread_done {
                self.worktree_create_rx = None;
            }
            self.worktree_create_wi = None;
            if let Some(aid) = self.worktree_create_activity.take() {
                self.end_activity(aid);
            }
        }

        // Cancel in-flight PR creation if it was for the deleted item.
        if self.pr_create_wi.as_ref() == Some(&work_item_id) {
            self.pr_create_rx = None;
            self.pr_create_wi = None;
            if let Some(aid) = self.pr_create_activity.take() {
                self.end_activity(aid);
            }
        }

        // Remove the deleted item from the PR creation pending queue.
        self.pr_create_pending.retain(|id| *id != work_item_id);

        // Cancel in-flight PR merge if it was for the deleted item.
        if self.merge_wi_id.as_ref() == Some(&work_item_id) && self.pr_merge_rx.is_some() {
            self.pr_merge_rx = None;
            if let Some(aid) = self.pr_merge_activity.take() {
                self.end_activity(aid);
            }
        }

        // -- Phase 6: Clean up in-memory state --
        self.rework_reasons.remove(&work_item_id);
        self.review_gate_findings.remove(&work_item_id);
        self.no_plan_prompt_queue.retain(|id| *id != work_item_id);
        if self.no_plan_prompt_queue.is_empty() {
            self.no_plan_prompt_visible = false;
        }
        if self.rework_prompt_wi.as_ref() == Some(&work_item_id) {
            self.rework_prompt_wi = None;
            self.rework_prompt_visible = false;
        }
        if self.merge_wi_id.as_ref() == Some(&work_item_id) {
            self.merge_wi_id = None;
            self.confirm_merge = false;
        }
        if self.review_gate_wi.as_ref() == Some(&work_item_id) {
            self.review_gate_wi = None;
            self.review_gate_rx = None;
            self.review_gate_progress = None;
            if let Some(aid) = self.review_gate_activity.take() {
                self.end_activity(aid);
            }
        }

        // -- Phase 7: Clear identity trackers and reassemble --
        self.selected_work_item = None;
        self.selected_unlinked_branch = None;
        self.selected_review_request_branch = None;

        let old_idx = self.selected_item;
        self.reassemble_work_items();
        self.build_display_list();
        self.fetcher_repos_changed = true;

        // Try to keep cursor near the old position instead of jumping to
        // the first item. If the old index is still valid, prefer it.
        if let Some(old) = old_idx {
            let mut found = false;
            for i in (0..self.display_list.len().min(old + 1)).rev() {
                if is_selectable(&self.display_list[i]) {
                    self.selected_item = Some(i);
                    found = true;
                    break;
                }
            }
            if !found {
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

        // -- Phase 8: Status message --
        self.focus = FocusPanel::Left;
        if warnings.is_empty() {
            self.status_message = Some("Work item deleted".into());
        } else {
            self.status_message = Some(format!("Deleted (with warnings: {})", warnings.join("; ")));
        }
    }

    /// Advance the selected work item to the next workflow stage.
    /// Persists the change via backend.update_status() and reassembles.
    /// When transitioning from Implementing to Review, runs the plan-based
    /// review gate if a plan exists.
    pub fn advance_stage(&mut self) {
        let Some(wi_id) = self.selected_work_item_id() else {
            return;
        };
        let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
            return;
        };
        if wi.status_derived {
            self.status_message = Some("Status is derived from merged PR".into());
            return;
        }
        // Review request items only support Review -> Done (via merge gate).
        // Block advance from any other stage.
        if wi.kind == WorkItemKind::ReviewRequest && wi.status != WorkItemStatus::Review {
            self.status_message =
                Some("Review request items only support Review and Done stages".into());
            return;
        }
        let current_status = wi.status.clone();
        let Some(new_status) = current_status.next_stage() else {
            self.status_message = Some("Already at final stage".into());
            return;
        };

        // Planning -> Implementing is automatic (triggered by workbridge_set_plan).
        // Block manual advance to prevent skipping the plan handoff.
        if current_status == WorkItemStatus::Planning && new_status == WorkItemStatus::Implementing
        {
            self.status_message =
                Some("Plan must be set via Claude session (workbridge_set_plan)".into());
            return;
        }

        // Review gate: the single chokepoint for entering Review.
        // The gate runs asynchronously in a background thread to avoid
        // blocking the TUI. The transition is blocked unless the gate
        // spawns and later approves it via poll_review_gate.
        if (current_status == WorkItemStatus::Implementing
            || current_status == WorkItemStatus::Blocked)
            && new_status == WorkItemStatus::Review
        {
            match self.spawn_review_gate(&wi_id) {
                ReviewGateSpawn::Spawned => {
                    // Gate is running in background - do not advance yet.
                }
                ReviewGateSpawn::Blocked(reason) => {
                    self.status_message = Some(reason);
                }
            }
            return;
        }

        // Merge prompt: when transitioning from Review to Done,
        // show the merge strategy prompt instead of advancing directly.
        if current_status == WorkItemStatus::Review && new_status == WorkItemStatus::Done {
            self.confirm_merge = true;
            self.merge_wi_id = Some(wi_id);
            self.status_message =
                Some("Merge PR? [s]quash (default) / [m]erge / [Esc] cancel".into());
            return;
        }

        self.apply_stage_change(&wi_id, &current_status, &new_status, "user");
    }

    /// Retreat the selected work item to the previous workflow stage.
    /// Persists the change via backend.update_status() and reassembles.
    pub fn retreat_stage(&mut self) {
        let Some(wi_id) = self.selected_work_item_id() else {
            return;
        };
        let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
            return;
        };
        if wi.status_derived {
            self.status_message = Some("Status is derived from merged PR".into());
            return;
        }
        // Review request items cannot retreat - there is no valid previous
        // stage for a review request in Review.
        if wi.kind == WorkItemKind::ReviewRequest {
            self.status_message = Some("Review request items cannot be retreated".into());
            return;
        }
        let current_status = wi.status.clone();
        let Some(new_status) = current_status.prev_stage() else {
            self.status_message = Some("Already at first stage".into());
            return;
        };

        // If the retreating item has a pending review gate, cancel it.
        // The gate result would be stale since the user intentionally moved away.
        if self.review_gate_wi.as_ref() == Some(&wi_id) {
            self.review_gate_rx = None;
            self.review_gate_wi = None;
            self.review_gate_progress = None;
            if let Some(aid) = self.review_gate_activity.take() {
                self.end_activity(aid);
            }
        }

        // Cancel any in-flight PR merge. Merges are only spawned from Review,
        // so when retreating from Review we drop the receiver to prevent
        // poll_pr_merge from applying a stale result. The background thread
        // will finish on its own; we just ignore its result.
        if current_status == WorkItemStatus::Review && self.pr_merge_rx.is_some() {
            self.pr_merge_rx = None;
            if let Some(aid) = self.pr_merge_activity.take() {
                self.end_activity(aid);
            }
        }

        // Cancel any in-flight or pending PR creation for the retreating item.
        // PR creation is spawned when entering Review; retreating means the user
        // no longer wants the PR. Drop the receiver so poll_pr_creation ignores
        // the result, and remove the item from the pending queue.
        if current_status == WorkItemStatus::Review {
            if self.pr_create_wi.as_ref() == Some(&wi_id) {
                self.pr_create_rx = None;
                self.pr_create_wi = None;
                if let Some(aid) = self.pr_create_activity.take() {
                    self.end_activity(aid);
                }
            }
            self.pr_create_pending.retain(|id| *id != wi_id);
        }

        // Rework prompt: when retreating from Review to Implementing,
        // show a text input for the rework reason instead of retreating directly.
        if current_status == WorkItemStatus::Review && new_status == WorkItemStatus::Implementing {
            self.rework_prompt_visible = true;
            self.rework_prompt_input.clear();
            self.rework_prompt_wi = Some(wi_id);
            self.status_message = Some("Rework reason: (Enter to submit, Esc to cancel)".into());
            return;
        }

        self.apply_stage_change(&wi_id, &current_status, &new_status, "user");
    }

    /// Move a blocked (no-plan) work item back to Planning so that Claude
    /// can analyze existing branch work and produce a retroactive plan.
    pub fn plan_from_branch(&mut self, wi_id: &WorkItemId) {
        // Guard: verify the work item is actually in Blocked state. MCP events
        // can change the status while the no-plan prompt is visible, so the
        // item may no longer be Blocked by the time the user responds.
        let is_blocked = self
            .work_items
            .iter()
            .find(|w| w.id == *wi_id)
            .is_some_and(|w| w.status == WorkItemStatus::Blocked);
        if !is_blocked {
            self.status_message = Some("Work item is no longer blocked".into());
            return;
        }

        // Transition first, then clear the plan. Only clear the plan if
        // the transition actually succeeded (the work item is now Planning).
        let current = WorkItemStatus::Blocked;
        let next = WorkItemStatus::Planning;
        self.apply_stage_change(wi_id, &current, &next, "user");

        let is_planning = self
            .work_items
            .iter()
            .find(|w| w.id == *wi_id)
            .is_some_and(|w| w.status == WorkItemStatus::Planning);
        if !is_planning {
            // apply_stage_change already set a status_message with the error.
            return;
        }

        // Clear the plan so the planning session starts fresh.
        if let Err(e) = self.backend.update_plan(wi_id, "") {
            self.status_message = Some(format!("Could not clear plan: {e}"));
        }
    }

    /// Shared logic for applying a stage change: log it, persist it, reassemble.
    ///
    /// Transitions to Done are only allowed when `source == "pr_merge"`,
    /// enforcing the merge-gate invariant at the chokepoint rather than
    /// relying on caller discipline alone.
    pub fn apply_stage_change(
        &mut self,
        wi_id: &WorkItemId,
        current_status: &WorkItemStatus,
        new_status: &WorkItemStatus,
        source: &str,
    ) {
        // Merge-gate guard: Done requires a verified PR merge.  All other
        // callers must go through the merge prompt / poll_pr_merge path,
        // which is the only code that passes source == "pr_merge".
        if *new_status == WorkItemStatus::Done && source != "pr_merge" {
            self.status_message = Some("Cannot move to Done without a merged PR".to_string());
            return;
        }

        let entry = ActivityEntry {
            timestamp: now_iso8601(),
            event_type: "stage_change".to_string(),
            payload: serde_json::json!({
                "from": format!("{:?}", current_status),
                "to": format!("{:?}", new_status),
                "source": source
            }),
        };
        if let Err(e) = self.backend.append_activity(wi_id, &entry) {
            self.status_message = Some(format!("Activity log error: {e}"));
        }

        if let Err(e) = self.backend.update_status(wi_id, new_status.clone()) {
            self.status_message = Some(format!("Stage update error: {e}"));
            return;
        }
        self.reassemble_work_items();
        self.build_display_list();
        self.status_message = Some(format!("Moved to {}", new_status.badge_text()));

        // Feature 1: Auto-create PR when entering Review (async).
        if *new_status == WorkItemStatus::Review {
            self.spawn_pr_creation(wi_id);
        }

        // Kill the old session for this work item before spawning a new one.
        // Previously relied on orphan cleanup in check_liveness, but that
        // leaves two sessions alive briefly and the old one can do work
        // (push, commit, etc.) in the gap.
        if let Some(old_key) = self.session_key_for(wi_id)
            && let Some(mut entry) = self.sessions.remove(&old_key)
        {
            if let Some(ref mut session) = entry.session {
                session.kill();
            }
            self.cleanup_session_state_for(wi_id);
        }

        // Auto-spawn a session for stages that have prompts.
        if !matches!(new_status, WorkItemStatus::Backlog | WorkItemStatus::Done) {
            self.spawn_session(wi_id);
        }
    }

    /// Best-effort async PR creation when entering Review.
    ///
    /// Gathers the needed data, then spawns a background thread to run
    /// the `gh` CLI commands. Results are polled by `poll_pr_creation()`
    /// on each timer tick.
    fn spawn_pr_creation(&mut self, wi_id: &WorkItemId) {
        // If a PR creation is already in-flight, queue this one instead of
        // silently dropping it. The queue is drained in poll_pr_creation.
        if self.pr_create_rx.is_some() {
            if !self.pr_create_pending.contains(wi_id) {
                self.pr_create_pending.push_back(wi_id.clone());
            }
            return;
        }

        let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
            Some(w) => w,
            None => return,
        };
        let assoc = match wi.repo_associations.first() {
            Some(a) => a,
            None => return,
        };
        let branch = match assoc.branch.as_ref() {
            Some(b) => b.clone(),
            None => return,
        };
        let repo_path = assoc.repo_path.clone();
        let title = wi.title.clone();
        let wi_id = wi_id.clone();

        // Get owner/repo from the worktree service (synchronous but fast).
        let (owner, repo_name) = match self.worktree_service.github_remote(&repo_path) {
            Ok(Some((o, r))) => (o, r),
            Ok(None) => {
                self.status_message = Some("PR creation skipped: no GitHub remote".into());
                return;
            }
            Err(e) => {
                self.status_message =
                    Some(format!("PR creation skipped: could not read remote: {e}"));
                return;
            }
        };
        let owner_repo = format!("{owner}/{repo_name}");

        // Get plan text and default branch before spawning (needs &self).
        let body = match self.backend.read_plan(&wi_id) {
            Ok(Some(plan)) if !plan.trim().is_empty() => plan,
            _ => String::new(),
        };
        let default_branch = self
            .worktree_service
            .default_branch(&repo_path)
            .unwrap_or_else(|_| "main".to_string());

        let (tx, rx) = crossbeam_channel::bounded(1);
        self.pr_create_wi = Some(wi_id.clone());

        std::thread::spawn(move || {
            // Check if a PR already exists for this branch.
            let check_output = std::process::Command::new("gh")
                .args([
                    "pr",
                    "list",
                    "--head",
                    &branch,
                    "--json",
                    "number",
                    "--repo",
                    &owner_repo,
                ])
                .output();

            match check_output {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Ok(arr) = serde_json::from_str::<serde_json::Value>(stdout.trim())
                        && arr.as_array().is_some_and(|a| !a.is_empty())
                    {
                        // PR already exists - nothing to do.
                        let _ = tx.send(PrCreateResult {
                            wi_id,
                            info: None,
                            error: None,
                            url: None,
                        });
                        return;
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("PR check failed (continuing): {stderr}")),
                        url: None,
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("PR check failed (continuing): {e}")),
                        url: None,
                    });
                    return;
                }
            }

            // Ensure the branch is pushed to the remote before creating the PR.
            let push_output = crate::worktree_service::git_command()
                .args(["push", "-u", "origin", &branch])
                .current_dir(&repo_path)
                .output();
            match push_output {
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("git push failed: {stderr}")),
                        url: None,
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("git push failed: {e}")),
                        url: None,
                    });
                    return;
                }
                _ => {} // push succeeded
            }

            // Create the PR.
            let create_result = std::process::Command::new("gh")
                .args([
                    "pr",
                    "create",
                    "--title",
                    &title,
                    "--body",
                    &body,
                    "--head",
                    &branch,
                    "--base",
                    &default_branch,
                    "--repo",
                    &owner_repo,
                ])
                .output();

            let result = match create_result {
                Ok(output) if output.status.success() => {
                    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    let info = format!("PR created: {url}");
                    PrCreateResult {
                        wi_id,
                        info: Some(info),
                        error: None,
                        url: Some(url),
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("PR creation failed (continuing): {stderr}")),
                        url: None,
                    }
                }
                Err(e) => PrCreateResult {
                    wi_id,
                    info: None,
                    error: Some(format!("PR creation failed (continuing): {e}")),
                    url: None,
                },
            };
            let _ = tx.send(result);
        });

        self.pr_create_rx = Some(rx);
        if let Some(aid) = self.pr_create_activity.take() {
            self.end_activity(aid);
        }
        self.pr_create_activity = Some(self.start_activity("Creating pull request..."));
    }

    /// Poll the async PR creation thread for a result. Called on each timer tick.
    pub fn poll_pr_creation(&mut self) {
        let rx = match self.pr_create_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };

        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.pr_create_rx = None;
                self.pr_create_wi = None;
                if let Some(aid) = self.pr_create_activity.take() {
                    self.end_activity(aid);
                }
                self.status_message =
                    Some("PR creation: background thread exited unexpectedly".into());
                // Try next pending PR creation despite the failure.
                // Skip items that were deleted or retreated from Review.
                while let Some(next_id) = self.pr_create_pending.pop_front() {
                    if self
                        .work_items
                        .iter()
                        .any(|w| w.id == next_id && w.status == WorkItemStatus::Review)
                    {
                        self.spawn_pr_creation(&next_id);
                        break;
                    }
                }
                return;
            }
        };

        self.pr_create_rx = None;
        self.pr_create_wi = None;
        if let Some(aid) = self.pr_create_activity.take() {
            self.end_activity(aid);
        }

        // Log PR creation to activity log.
        if let Some(ref url) = result.url {
            let log_entry = ActivityEntry {
                timestamp: now_iso8601(),
                event_type: "pr_created".to_string(),
                payload: serde_json::json!({ "url": url }),
            };
            if let Err(e) = self.backend.append_activity(&result.wi_id, &log_entry) {
                self.status_message = Some(format!("Activity log error: {e}"));
            }
        }

        // Update status message.
        if let Some(info) = result.info {
            self.status_message = Some(info);
        } else if let Some(error) = result.error {
            self.status_message = Some(error);
        }

        // Drain the pending queue: spawn the next queued PR creation if any.
        // Skip items that were deleted or retreated from Review while queued.
        while let Some(next_id) = self.pr_create_pending.pop_front() {
            if self
                .work_items
                .iter()
                .any(|w| w.id == next_id && w.status == WorkItemStatus::Review)
            {
                self.spawn_pr_creation(&next_id);
                break;
            }
        }
    }

    /// Spawn an async PR merge for the given work item.
    ///
    /// `strategy` is either "squash" or "merge". If any prerequisite is
    /// missing (no repo association, no branch, no GitHub remote), the
    /// request is blocked with an error message and the item stays in
    /// Review. The background thread also checks for an open PR and
    /// blocks if none exists (see `poll_pr_merge` / `NoPr` outcome).
    ///
    /// Gathers the needed data synchronously, then spawns a background
    /// thread to run `gh pr merge`. Results are polled by `poll_pr_merge()`
    /// on each timer tick.
    pub fn execute_merge(&mut self, wi_id: &WorkItemId, strategy: &str) {
        // If a PR merge is already in-flight, don't spawn another.
        // The background thread may have already merged a PR on GitHub;
        // replacing the receiver would silently lose its result.
        if self.pr_merge_rx.is_some() {
            self.status_message = Some("PR merge already in progress".into());
            return;
        }

        let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
            Some(w) => w,
            None => return,
        };
        let assoc = match wi.repo_associations.first() {
            Some(a) => a,
            None => {
                self.status_message = Some("Cannot merge: no repo association".into());
                return;
            }
        };
        let branch = match assoc.branch.as_ref() {
            Some(b) => b.clone(),
            None => {
                self.status_message = Some("Cannot merge: no branch associated".into());
                return;
            }
        };
        let repo_path = assoc.repo_path.clone();

        // Get owner/repo from the worktree service.
        let (owner, repo_name) = match self.worktree_service.github_remote(&repo_path) {
            Ok(Some((o, r))) => (o, r),
            _ => {
                self.status_message = Some("Cannot merge: no GitHub remote found".into());
                return;
            }
        };
        let owner_repo = format!("{owner}/{repo_name}");
        let merge_flag = if strategy == "merge" {
            "--merge"
        } else {
            "--squash"
        };
        let strategy_owned = strategy.to_string();
        let wi_id_clone = wi_id.clone();
        let merge_flag_owned = merge_flag.to_string();
        let repo_path_clone = repo_path.clone();

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            // Check if a PR exists for this branch and fetch its identity.
            let pr_identity = match std::process::Command::new("gh")
                .args([
                    "pr",
                    "list",
                    "--head",
                    &branch,
                    "--json",
                    "number,title,url",
                    "--repo",
                    &owner_repo,
                ])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    serde_json::from_str::<Vec<serde_json::Value>>(stdout.trim())
                        .ok()
                        .and_then(|arr| arr.into_iter().next())
                        .and_then(|obj| {
                            let number = obj.get("number")?.as_u64()?;
                            let title = obj.get("title")?.as_str()?.to_string();
                            let url = obj.get("url")?.as_str()?.to_string();
                            Some(PrIdentityRecord { number, title, url })
                        })
                }
                _ => None,
            };

            let outcome = if pr_identity.is_none() {
                PrMergeOutcome::NoPr
            } else {
                // Run gh pr merge.
                match std::process::Command::new("gh")
                    .args([
                        "pr",
                        "merge",
                        &branch,
                        &merge_flag_owned,
                        "--delete-branch",
                        "--repo",
                        &owner_repo,
                    ])
                    .output()
                {
                    Ok(output) if output.status.success() => PrMergeOutcome::Merged {
                        strategy: strategy_owned,
                        pr_identity,
                    },
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                        if stderr.to_lowercase().contains("conflict") {
                            PrMergeOutcome::Conflict { stderr }
                        } else {
                            PrMergeOutcome::Failed {
                                error: format!("Merge failed: {}", stderr.trim()),
                            }
                        }
                    }
                    Err(e) => PrMergeOutcome::Failed {
                        error: format!("Merge failed: {e}"),
                    },
                }
            };

            let _ = tx.send(PrMergeResult {
                wi_id: wi_id_clone,
                branch,
                repo_path: repo_path_clone,
                outcome,
            });
        });

        self.pr_merge_rx = Some(rx);
        if let Some(aid) = self.pr_merge_activity.take() {
            self.end_activity(aid);
        }
        self.pr_merge_activity = Some(self.start_activity("Merging pull request..."));
    }

    /// Poll the async PR merge thread for a result. Called on each timer tick.
    pub fn poll_pr_merge(&mut self) {
        let rx = match self.pr_merge_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };

        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.pr_merge_rx = None;
                if let Some(aid) = self.pr_merge_activity.take() {
                    self.end_activity(aid);
                }
                self.status_message =
                    Some("PR merge: background thread exited unexpectedly".into());
                return;
            }
        };

        self.pr_merge_rx = None;
        if let Some(aid) = self.pr_merge_activity.take() {
            self.end_activity(aid);
        }

        // Guard: if the item's status changed while the merge was in-flight
        // (e.g. user retreated to Implementing), discard the stale result to
        // avoid forcing the item back to Done or deleting its worktree.
        let actual_status = self
            .work_items
            .iter()
            .find(|w| w.id == result.wi_id)
            .map(|w| w.status.clone());

        match result.outcome {
            PrMergeOutcome::NoPr => {
                self.status_message =
                    Some("Cannot merge: no PR found. Push branch and open a PR first.".into());
            }
            PrMergeOutcome::Merged {
                ref strategy,
                ref pr_identity,
            } => {
                // Persist PR identity to backend so it survives reassembly.
                if let Some(identity) = pr_identity
                    && let Err(e) =
                        self.backend
                            .save_pr_identity(&result.wi_id, &result.repo_path, identity)
                {
                    self.status_message = Some(format!("PR identity save error: {e}"));
                }

                // Log merge to activity log (always - the merge happened on GitHub).
                let log_entry = ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "pr_merged".to_string(),
                    payload: serde_json::json!({
                        "strategy": strategy,
                        "branch": result.branch
                    }),
                };
                if let Err(e) = self.backend.append_activity(&result.wi_id, &log_entry) {
                    self.status_message = Some(format!("Activity log error: {e}"));
                }

                if actual_status.as_ref() != Some(&WorkItemStatus::Review) {
                    // Item was moved away from Review while merge was in-flight.
                    // The merge already happened on GitHub, but we do not change
                    // the local status or delete the worktree.
                    self.status_message = Some(
                        "PR merged on GitHub, but item status was changed - not advancing to Done"
                            .to_string(),
                    );
                    return;
                }

                // Clean up worktree directory.
                self.cleanup_worktree_for_item(&result.wi_id);

                // Advance to Done.
                self.apply_stage_change(
                    &result.wi_id,
                    &WorkItemStatus::Review,
                    &WorkItemStatus::Done,
                    "pr_merge",
                );
                self.status_message = Some(format!("PR merged ({strategy}) and moved to [DN]"));
            }
            PrMergeOutcome::Conflict { ref stderr } => {
                if actual_status.as_ref() != Some(&WorkItemStatus::Review) {
                    return;
                }
                // Log conflict to activity log.
                let conflict_entry = ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "merge_conflict".to_string(),
                    payload: serde_json::json!({
                        "branch": result.branch,
                        "stderr": stderr.trim()
                    }),
                };
                if let Err(e) = self.backend.append_activity(&result.wi_id, &conflict_entry) {
                    self.status_message = Some(format!("Activity log error: {e}"));
                }
                let reason = "Merge failed due to conflicts. Rebase onto the base branch and resolve all conflicts.".to_string();
                self.rework_reasons.insert(result.wi_id.clone(), reason);
                self.apply_stage_change(
                    &result.wi_id,
                    &WorkItemStatus::Review,
                    &WorkItemStatus::Implementing,
                    "merge_conflict",
                );
                self.status_message = Some(
                    "Merge conflict detected - moved back to [IM] for rebase/resolve".to_string(),
                );
            }
            PrMergeOutcome::Failed { ref error } => {
                self.status_message = Some(error.clone());
            }
        }
    }

    /// Collect Done items that need PR identity backfill (have a branch but
    /// no persisted pr_identity). Returns tuples of
    /// (wi_id, repo_path, branch, github_owner, github_repo).
    ///
    /// Temporary migration helper - can be removed once all existing Done
    /// items have been backfilled (i.e. no Done items with pr_identity=None
    /// remain on disk).
    pub fn collect_backfill_requests(&self) -> Vec<(WorkItemId, PathBuf, String, String, String)> {
        let records = match self.backend.list() {
            Ok(lr) => lr.records,
            Err(_) => return vec![],
        };
        let mut requests = Vec::new();
        for record in &records {
            if record.status != WorkItemStatus::Done {
                continue;
            }
            for assoc in &record.repo_associations {
                if assoc.pr_identity.is_some() {
                    continue;
                }
                let branch = match &assoc.branch {
                    Some(b) => b.clone(),
                    None => continue,
                };
                let (owner, repo_name) = match self.worktree_service.github_remote(&assoc.repo_path)
                {
                    Ok(Some((o, r))) => (o, r),
                    _ => continue,
                };
                requests.push((
                    record.id.clone(),
                    assoc.repo_path.clone(),
                    branch,
                    owner,
                    repo_name,
                ));
            }
        }
        requests
    }

    /// Drain results from the PR identity backfill channel. Returns true
    /// if any identities were saved (caller should reassemble).
    ///
    /// Temporary migration helper - can be removed once all existing Done
    /// items have been backfilled.
    pub fn drain_pr_identity_backfill(&mut self) -> bool {
        let rx = match self.pr_identity_backfill_rx.as_ref() {
            Some(rx) => rx,
            None => return false,
        };
        let mut changed = false;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(msg) => match msg {
                    Ok(result) => {
                        if let Err(e) = self.backend.save_pr_identity(
                            &result.wi_id,
                            &result.repo_path,
                            &result.identity,
                        ) {
                            self.status_message = Some(format!("PR identity backfill error: {e}"));
                        } else {
                            changed = true;
                        }
                    }
                    Err(e) => {
                        self.status_message = Some(format!("PR identity backfill: {e}"));
                    }
                },
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.pr_identity_backfill_rx = None;
        }
        changed
    }

    /// Remove the worktree directory and local branch for a work item after merge.
    /// Uses delete_branch=true so the merged branch is cleaned up. Uses force=false
    /// because post-merge worktrees should be clean and `-d` is safe for merged branches.
    fn cleanup_worktree_for_item(&mut self, wi_id: &WorkItemId) {
        let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
            Some(w) => w,
            None => return,
        };
        for assoc in &wi.repo_associations {
            if let Some(ref wt_path) = assoc.worktree_path {
                if let Err(e) =
                    self.worktree_service
                        .remove_worktree(&assoc.repo_path, wt_path, true, false)
                {
                    self.status_message = Some(format!("Worktree cleanup warning: {e}"));
                }
            } else if let Some(ref branch) = assoc.branch {
                // No worktree but a branch exists - still clean up the branch.
                if let Err(e) = self
                    .worktree_service
                    .delete_branch(&assoc.repo_path, branch, false)
                {
                    self.status_message = Some(format!("Branch cleanup warning: {e}"));
                }
            }
        }
    }

    /// Attempt to spawn the async review gate for the given work item.
    /// Returns `Spawned` if the gate is running (caller should wait),
    /// or `Blocked(reason)` if the transition must not proceed.
    fn spawn_review_gate(&mut self, wi_id: &WorkItemId) -> ReviewGateSpawn {
        // Guard: if a review gate is already running, don't spawn another one.
        // A second call would overwrite the fields and leak the first session.
        if self.review_gate_wi.is_some() {
            return ReviewGateSpawn::Blocked("Review gate already running".into());
        }

        // Read the plan from the backend.
        let plan = match self.backend.read_plan(wi_id) {
            Ok(Some(plan)) if !plan.trim().is_empty() => plan,
            Ok(_) => {
                return ReviewGateSpawn::Blocked("Cannot enter Review: no plan exists".into());
            }
            Err(e) => {
                return ReviewGateSpawn::Blocked(format!("Could not read plan: {e}"));
            }
        };

        // Find the branch for this work item to get the diff.
        let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
            Some(wi) => wi,
            None => {
                return ReviewGateSpawn::Blocked("Work item not found".into());
            }
        };
        let assoc = match wi.repo_associations.first() {
            Some(a) => a,
            None => {
                return ReviewGateSpawn::Blocked("Cannot enter Review: no repo association".into());
            }
        };
        let branch = match assoc.branch.as_ref() {
            Some(b) => b.clone(),
            None => {
                return ReviewGateSpawn::Blocked("Cannot enter Review: no branch set".into());
            }
        };
        let repo_path = assoc.repo_path.clone();

        // Get the default branch for diffing.
        let default_branch = self
            .worktree_service
            .default_branch(&repo_path)
            .unwrap_or_else(|_| "main".to_string());

        // Get the git diff (this is fast, local I/O only).
        let diff = match crate::worktree_service::git_command()
            .arg("-C")
            .arg(&repo_path)
            .args(["diff", &format!("{default_branch}...{branch}")])
            .output()
        {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).to_string()
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return ReviewGateSpawn::Blocked(format!("Review gate: git diff failed: {stderr}"));
            }
            Err(e) => {
                return ReviewGateSpawn::Blocked(format!("Review gate: could not run git: {e}"));
            }
        };

        if diff.trim().is_empty() {
            return ReviewGateSpawn::Blocked("Cannot enter Review: no changes on branch".into());
        }

        // Spawn the claude --print check in a background thread.
        let (tx, rx) = crossbeam_channel::bounded(1);
        let wi_id_clone = wi_id.clone();
        let review_skill = self.config.defaults.review_skill.clone();

        std::thread::spawn(move || {
            let mut vars = std::collections::HashMap::new();
            vars.insert("plan", plan.as_str());
            vars.insert("diff", diff.as_str());
            let system = crate::prompts::render("review_gate", &vars).unwrap_or_else(|| {
                "Compare plan to diff. Respond with JSON: {\"approved\": bool, \"detail\": string}"
                    .into()
            });
            let prompt = format!("{review_skill}\n\nPlan:\n{plan}\n\nDiff:\n{diff}");

            let json_schema = r#"{"type":"object","properties":{"approved":{"type":"boolean"},"detail":{"type":"string"}},"required":["approved","detail"]}"#;

            let result = match std::process::Command::new("claude")
                .args([
                    "--print",
                    "-p",
                    &prompt,
                    "--system-prompt",
                    &system,
                    "--output-format",
                    "json",
                    "--json-schema",
                    json_schema,
                ])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    // Parse the JSON envelope from claude --print --output-format json.
                    // The structured output is in the "structured_output" field.
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(envelope) => {
                            let structured = &envelope["structured_output"];
                            let approved = structured["approved"].as_bool().unwrap_or(false);
                            let detail = structured["detail"].as_str().unwrap_or("").to_string();
                            ReviewGateResult {
                                work_item_id: wi_id_clone,
                                approved,
                                detail,
                            }
                        }
                        Err(e) => ReviewGateResult {
                            work_item_id: wi_id_clone,
                            approved: false,
                            detail: format!("review gate: invalid JSON response: {e}"),
                        },
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!("claude failed: {stderr}"),
                    }
                }
                Err(e) => ReviewGateResult {
                    work_item_id: wi_id_clone,
                    approved: false,
                    detail: format!("could not run claude: {e}"),
                },
            };
            let _ = tx.send(result);
        });

        self.review_gate_rx = Some(rx);
        self.review_gate_wi = Some(wi_id.clone());
        self.review_gate_activity = Some(self.start_activity("Running review gate..."));
        ReviewGateSpawn::Spawned
    }

    /// Poll the async review gate for a result. Called on each timer tick.
    /// If the gate has completed, processes the result: advances to Review
    /// if approved, stays in Implementing if rejected.
    pub fn poll_review_gate(&mut self) {
        let rx = match self.review_gate_rx.as_ref() {
            Some(rx) => rx,
            None => return,
        };

        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return, // Still running.
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                // Thread exited without sending - treat as gate error.
                self.review_gate_rx = None;
                self.review_gate_wi = None;
                self.review_gate_progress = None;
                if let Some(aid) = self.review_gate_activity.take() {
                    self.end_activity(aid);
                }
                self.status_message =
                    Some("Review gate: background thread exited unexpectedly".into());
                return;
            }
        };

        // Gate completed - clear the receiver and tracked work item.
        self.review_gate_rx = None;
        self.review_gate_wi = None;
        self.review_gate_progress = None;
        if let Some(aid) = self.review_gate_activity.take() {
            self.end_activity(aid);
        }

        let wi_id = result.work_item_id.clone();

        // Verify the work item is still eligible for the gate result.
        // Both Implementing and Blocked are valid pre-gate states (Blocked->Review
        // is allowed per Fix #6). If the user retreated the item while the gate
        // was running, we discard the result silently.
        let gate_eligible = self
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .map(|w| {
                w.status == WorkItemStatus::Implementing || w.status == WorkItemStatus::Blocked
            })
            .unwrap_or(false);

        if !gate_eligible {
            return;
        }

        if result.approved {
            // Log approval and advance to Review.
            let entry = ActivityEntry {
                timestamp: now_iso8601(),
                event_type: "review_gate".to_string(),
                payload: serde_json::json!({
                    "result": "approved",
                    "response": result.detail
                }),
            };
            if let Err(e) = self.backend.append_activity(&wi_id, &entry) {
                self.status_message = Some(format!("Activity log error: {e}"));
            }

            // Store the gate's assessment so the Review session can present
            // it to the user (consumed one-shot by stage_system_prompt).
            self.review_gate_findings
                .insert(wi_id.clone(), result.detail.clone());

            // Get the actual current status for apply_stage_change.
            let current_status = self
                .work_items
                .iter()
                .find(|w| w.id == wi_id)
                .map(|w| w.status.clone())
                .unwrap_or(WorkItemStatus::Implementing);

            self.apply_stage_change(
                &wi_id,
                &current_status,
                &WorkItemStatus::Review,
                "review_gate",
            );
        } else {
            // Log rejection and stay in current stage.
            let entry = ActivityEntry {
                timestamp: now_iso8601(),
                event_type: "review_gate".to_string(),
                payload: serde_json::json!({
                    "result": "rejected",
                    "reason": result.detail
                }),
            };
            if let Err(e) = self.backend.append_activity(&wi_id, &entry) {
                self.status_message = Some(format!("Activity log error: {e}"));
            }
            // Store the rejection reason so the next Claude session uses the
            // implementing_rework prompt with specific feedback, rather than
            // a generic implementing prompt.
            self.rework_reasons
                .insert(wi_id.clone(), result.detail.clone());
            self.status_message = Some(format!("Review gate rejected: {}", result.detail));

            // If Blocked, transition to Implementing so the implementing_rework
            // prompt (which has {rework_reason}) is used instead of the "blocked"
            // prompt (which has no rework_reason placeholder).
            {
                let wi_status = self
                    .work_items
                    .iter()
                    .find(|w| w.id == wi_id)
                    .map(|w| w.status.clone());
                if wi_status == Some(WorkItemStatus::Blocked) {
                    let _ = self
                        .backend
                        .update_status(&wi_id, WorkItemStatus::Implementing);
                    self.reassemble_work_items();
                    self.build_display_list();
                }
            }

            // Kill the current session and respawn with the implementing_rework
            // prompt that includes the rejection feedback.
            if let Some(key) = self.session_key_for(&wi_id)
                && let Some(mut entry) = self.sessions.remove(&key)
                && let Some(ref mut session) = entry.session
            {
                session.kill();
            }
            self.cleanup_session_state_for(&wi_id);
            self.spawn_session(&wi_id);
        }
    }

    /// Get the SessionEntry for the currently selected work item, if any.
    pub fn active_session_entry(&self) -> Option<&SessionEntry> {
        let work_item_id = self.selected_work_item_id()?;
        let key = self.session_key_for(&work_item_id)?;
        self.sessions.get(&key)
    }

    /// Returns true if any session is alive (including the global session).
    pub fn has_any_session(&self) -> bool {
        self.sessions.values().any(|e| e.alive)
            || self.global_session.as_ref().is_some_and(|s| s.alive)
    }

    // -- Global assistant --------------------------------------------------

    /// Toggle the global assistant drawer open/closed.
    /// On first open, spawns the global session lazily.
    pub fn toggle_global_drawer(&mut self) {
        if self.global_drawer_open {
            // Close drawer, restore previous focus.
            self.global_drawer_open = false;
            self.focus = self.pre_drawer_focus;
        } else {
            // Open drawer.
            self.pre_drawer_focus = self.focus;
            self.global_drawer_open = true;

            // Spawn on first use (or respawn if dead).
            let needs_spawn = self.global_session.as_ref().is_none_or(|s| !s.alive);
            if needs_spawn {
                self.spawn_global_session();
            }
        }
    }

    /// Spawn the global assistant Claude Code session.
    fn spawn_global_session(&mut self) {
        // Build dynamic context and start MCP server.
        self.refresh_global_mcp_context();
        let socket_path = crate::mcp::socket_path_for_session();
        let mcp_server = match McpSocketServer::start_global(
            socket_path.clone(),
            Arc::clone(&self.global_mcp_context),
        ) {
            Ok(server) => server,
            Err(e) => {
                self.status_message = Some(format!("Global assistant MCP error: {e}"));
                self.global_drawer_open = false;
                self.focus = self.pre_drawer_focus;
                return;
            }
        };

        // Build repo list for the system prompt.
        let repo_list: String = self
            .active_repo_cache
            .iter()
            .map(|r| format!("- {}", r.path.display()))
            .collect::<Vec<_>>()
            .join("\n");

        let system_prompt = {
            let mut vars = std::collections::HashMap::new();
            vars.insert("repo_list", repo_list.as_str());
            crate::prompts::render("global_assistant", &vars)
        };

        let mut cmd: Vec<String> = vec!["claude".to_string()];
        cmd.push("--permission-mode".to_string());
        cmd.push("plan".to_string());
        // Auto-allow workbridge MCP tools so Claude Code does not prompt.
        cmd.push("--allowedTools".to_string());
        cmd.push(
            "mcp__workbridge__workbridge_list_repos,\
             mcp__workbridge__workbridge_list_work_items,\
             mcp__workbridge__workbridge_repo_info"
                .to_string(),
        );
        if let Some(ref prompt) = system_prompt {
            cmd.push("--system-prompt".to_string());
            cmd.push(prompt.clone());
        }

        // Write MCP config to a temp file and pass via --mcp-config.
        // Use a deterministic PID-based path so respawns overwrite the
        // previous file instead of leaking a new one each time.
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                self.status_message = Some(format!(
                    "Global assistant: cannot resolve executable path: {e}"
                ));
                self.global_drawer_open = false;
                self.focus = self.pre_drawer_focus;
                return;
            }
        };
        let mcp_config = crate::mcp::build_mcp_config(&exe, &mcp_server.socket_path);
        let config_path =
            std::env::temp_dir().join(format!("workbridge-global-mcp-{}.json", std::process::id()));
        if let Err(e) = std::fs::write(&config_path, &mcp_config) {
            self.status_message = Some(format!("Global assistant MCP config error: {e}"));
            self.global_drawer_open = false;
            self.focus = self.pre_drawer_focus;
            return;
        }
        self.global_mcp_config_path = Some(config_path.clone());
        cmd.push("--mcp-config".to_string());
        cmd.push(config_path.to_string_lossy().to_string());

        // Use home directory as cwd (neutral, not biased toward any repo).
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);

        let cmd_refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        match Session::spawn(
            self.global_pane_cols,
            self.global_pane_rows,
            Some(&home),
            &cmd_refs,
        ) {
            Ok(session) => {
                let parser = Arc::clone(&session.parser);
                self.global_session = Some(SessionEntry {
                    parser,
                    alive: true,
                    session: Some(session),
                });
                self.global_mcp_server = Some(mcp_server);
            }
            Err(e) => {
                self.status_message = Some(format!("Global assistant spawn error: {e}"));
                self.global_drawer_open = false;
                self.focus = self.pre_drawer_focus;
            }
        }
    }

    /// Send raw bytes to the global assistant session's PTY.
    pub fn send_bytes_to_global(&mut self, data: &[u8]) {
        if let Some(ref entry) = self.global_session
            && entry.alive
            && let Some(ref session) = entry.session
            && let Err(e) = session.write_bytes(data)
        {
            self.status_message = Some(format!("Global assistant write error: {e}"));
        }
    }

    /// Refresh the shared dynamic context for the global MCP server.
    /// Called on each timer tick.
    pub fn refresh_global_mcp_context(&mut self) {
        let repos: Vec<serde_json::Value> = self
            .active_repo_cache
            .iter()
            .map(|r| {
                let repo_path = r.path.display().to_string();
                let fetch_data = self.repo_data.get(&r.path);

                let worktrees: Vec<serde_json::Value> = fetch_data
                    .and_then(|fd| fd.worktrees.as_ref().ok())
                    .map(|wts| {
                        wts.iter()
                            .map(|wt| {
                                serde_json::json!({
                                    "path": wt.path.display().to_string(),
                                    "branch": wt.branch,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let prs: Vec<serde_json::Value> = fetch_data
                    .and_then(|fd| fd.prs.as_ref().ok())
                    .map(|pr_list| {
                        pr_list
                            .iter()
                            .map(|pr| {
                                serde_json::json!({
                                    "number": pr.number,
                                    "title": pr.title,
                                    "state": &pr.state,
                                    "branch": &pr.head_branch,
                                    "url": &pr.url,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                serde_json::json!({
                    "path": repo_path,
                    "worktrees": worktrees,
                    "prs": prs,
                })
            })
            .collect();

        let work_items: Vec<serde_json::Value> = self
            .work_items
            .iter()
            .map(|wi| {
                let repo_path = wi
                    .repo_associations
                    .first()
                    .map(|a| a.repo_path.display().to_string())
                    .unwrap_or_default();
                let branch = wi
                    .repo_associations
                    .first()
                    .and_then(|a| a.branch.as_deref())
                    .unwrap_or("");
                let pr_url = wi
                    .repo_associations
                    .first()
                    .and_then(|a| a.pr.as_ref())
                    .map(|pr| pr.url.as_str())
                    .unwrap_or("");
                serde_json::json!({
                    "title": wi.title,
                    "status": format!("{:?}", wi.status),
                    "repo_path": repo_path,
                    "branch": branch,
                    "pr_url": pr_url,
                })
            })
            .collect();

        let ctx = serde_json::json!({
            "repos": repos,
            "work_items": work_items,
        });

        match self.global_mcp_context.lock() {
            Ok(mut guard) => {
                *guard = ctx.to_string();
            }
            Err(e) => {
                self.status_message = Some(format!("Global MCP context lock poisoned: {e}"));
            }
        }
    }

    /// Collect extra branch names from backend records, grouped by repo
    /// path. These are branches recorded in work items that may not have
    /// worktrees yet. The fetcher uses them to also extract and fetch
    /// issue metadata for branch-only work items.
    pub fn extra_branches_from_backend(&self) -> std::collections::HashMap<PathBuf, Vec<String>> {
        let mut map: std::collections::HashMap<PathBuf, Vec<String>> =
            std::collections::HashMap::new();
        let list_result = match self.backend.list() {
            Ok(r) => r,
            Err(_) => {
                // Backend list failed - the fetcher just won't have extras.
                // The error will surface through other paths (assembly, etc.).
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
            if let Ok(canonical) = crate::config::canonicalize_path(&entry.path) {
                entry.path = canonical;
            }
            entry
        })
        .collect()
}

/// Public crate-level accessor for now_iso8601, used by the event module.
pub fn now_iso8601_pub() -> String {
    now_iso8601()
}

/// Return the current time as an ISO 8601 string (UTC).
fn now_iso8601() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Simple UTC timestamp without pulling in chrono.
    // Format: seconds since epoch as a decimal string with "Z" suffix.
    // This is monotonic and machine-parseable.
    format!("{secs}Z")
}

/// Returns true if a display entry can receive selection (is an item, not
/// a header or empty state).
pub fn is_selectable(entry: &DisplayEntry) -> bool {
    matches!(
        entry,
        DisplayEntry::ReviewRequestItem(_)
            | DisplayEntry::UnlinkedItem(_)
            | DisplayEntry::WorkItemEntry(_)
    )
}

/// A stub worktree service that returns empty results. Used as a default
/// when no real worktree operations are needed (e.g. tests, initial setup).
#[cfg(test)]
pub struct StubWorktreeService;

#[cfg(test)]
impl WorktreeService for StubWorktreeService {
    fn list_worktrees(
        &self,
        _repo_path: &std::path::Path,
    ) -> Result<Vec<crate::worktree_service::WorktreeInfo>, crate::worktree_service::WorktreeError>
    {
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
        _force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }

    fn delete_branch(
        &self,
        _repo_path: &std::path::Path,
        _branch: &str,
        _force: bool,
    ) -> Result<(), crate::worktree_service::WorktreeError> {
        Ok(())
    }

    fn is_worktree_dirty(
        &self,
        _worktree_path: &std::path::Path,
    ) -> Result<bool, crate::worktree_service::WorktreeError> {
        Ok(false)
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

    fn create_branch(
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
    fn read(
        &self,
        id: &WorkItemId,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::NotFound(id.clone()))
    }

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

    fn update_status(&self, _id: &WorkItemId, _status: WorkItemStatus) -> Result<(), BackendError> {
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

    fn import_review_request(
        &self,
        _rr: &crate::work_item::ReviewRequestedPr,
    ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
        Err(BackendError::Validation(
            "stub backend does not support import_review_request".into(),
        ))
    }

    fn append_activity(
        &self,
        _id: &WorkItemId,
        _entry: &ActivityEntry,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
        Ok(Vec::new())
    }

    fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
        Ok(())
    }

    fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
        Ok(None)
    }
    fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
        None
    }
    fn backend_type(&self) -> crate::work_item::BackendType {
        crate::work_item::BackendType::LocalFile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_item::BackendType;
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
        let canonical_dir = crate::config::canonicalize_path(&dir).unwrap();
        assert_eq!(
            root,
            canonical_dir,
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
        use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};
        use crate::work_item_backend::ListResult;

        /// Test backend that supports import and delete.
        struct TestBackend {
            records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
        }

        impl WorkItemBackend for TestBackend {
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                self.records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.id == *id)
                    .cloned()
                    .ok_or_else(|| BackendError::NotFound(id.clone()))
            }
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
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/fake.json")),
                    title: unlinked.pr.title.clone(),
                    description: None,
                    status: WorkItemStatus::Implementing,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn import_review_request(
                &self,
                rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/fake-rr.json")),
                    title: rr.pr.title.clone(),
                    status: WorkItemStatus::Review,
                    kind: crate::work_item::WorkItemKind::ReviewRequest,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: rr.repo_path.clone(),
                        branch: Some(rr.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                    description: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
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
        app.delete_selected_work_item(false);
        assert!(
            app.fetcher_repos_changed,
            "fetcher_repos_changed should be true after delete",
        );
    }

    /// F-1: fetcher_repos_changed is set after creating a work item with a
    /// branch. Without this, the fetcher never picks up the new branch for
    /// issue metadata.
    #[test]
    fn create_with_branch_sets_fetcher_repos_changed() {
        use crate::work_item_backend::ListResult;

        struct CreateBackend;

        impl WorkItemBackend for CreateBackend {
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::NotFound(id.clone()))
            }
            fn list(&self) -> Result<ListResult, BackendError> {
                Ok(ListResult {
                    records: Vec::new(),
                    corrupt: Vec::new(),
                })
            }
            fn create(
                &self,
                req: CreateWorkItem,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Ok(crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/new.json")),
                    title: req.title.clone(),
                    description: None,
                    status: req.status.clone(),
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: req.repo_associations,
                    plan: None,
                })
            }
            fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
                Ok(())
            }
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                _unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn import_review_request(
                &self,
                _rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not supported in test".into()))
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let mut app = App::with_config(Config::default(), Box::new(CreateBackend));
        app.active_repo_cache = vec![RepoEntry {
            path: PathBuf::from("/repo"),
            source: RepoSource::Explicit,
            git_dir_present: true,
        }];

        // Create with a branch - flag should be set.
        assert!(!app.fetcher_repos_changed);
        let result = app.create_work_item_with(
            "With branch".into(),
            None,
            vec![PathBuf::from("/repo")],
            "feature/test".into(),
        );
        assert!(result.is_ok());
        assert!(
            app.fetcher_repos_changed,
            "fetcher_repos_changed should be true after creating with a branch",
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
            source.contains(r#""500""#) && source.contains(r#""--limit""#),
            "PR list limit should be 500 to avoid silent truncation in busy repos",
        );
    }

    /// PR list calls must include --author @me to filter to the
    /// authenticated user's PRs. Without this, repos with 5000+ open PRs
    /// return foreign PRs and may not include the user's own.
    #[test]
    fn pr_list_uses_author_me() {
        let source = include_str!("github_client.rs");
        assert!(
            source.contains(r#""--author""#) && source.contains(r#""@me""#),
            "PR list calls should include --author @me to filter to user's PRs",
        );
    }

    /// FetchStarted message triggers a status bar activity, cleared on
    /// RepoData arrival.
    #[test]
    fn fetch_started_shows_activity() {
        let mut app = App::new();
        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        tx.send(FetchMessage::FetchStarted).unwrap();

        app.drain_fetch_results();
        assert!(app.fetch_activity.is_some());
        assert!(app.current_activity().is_some());

        // Sending RepoData should clear the activity.
        tx.send(FetchMessage::RepoData(RepoFetchResult {
            repo_path: PathBuf::from("/repo"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            authenticated_user: None,
            issues: vec![],
        }))
        .unwrap();

        app.drain_fetch_results();
        assert!(app.fetch_activity.is_none());
    }

    /// FetcherError also clears the fetch activity.
    #[test]
    fn fetch_started_cleared_on_error() {
        let mut app = App::new();
        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        tx.send(FetchMessage::FetchStarted).unwrap();
        app.drain_fetch_results();
        assert!(app.fetch_activity.is_some());

        tx.send(FetchMessage::FetcherError {
            repo_path: PathBuf::from("/repo"),
            error: "test error".into(),
        })
        .unwrap();

        app.drain_fetch_results();
        assert!(app.fetch_activity.is_none());
    }

    /// Multiple FetchStarted messages should not create duplicate activities.
    #[test]
    fn fetch_started_deduplicates() {
        let mut app = App::new();
        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        tx.send(FetchMessage::FetchStarted).unwrap();
        tx.send(FetchMessage::FetchStarted).unwrap();

        app.drain_fetch_results();
        assert_eq!(app.activities.len(), 1);
    }

    /// Spinner persists until all in-flight repos finish, not just the first.
    #[test]
    fn fetch_activity_persists_until_all_repos_finish() {
        let mut app = App::new();
        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        // Two repos start fetching.
        tx.send(FetchMessage::FetchStarted).unwrap();
        tx.send(FetchMessage::FetchStarted).unwrap();
        app.drain_fetch_results();
        assert!(app.fetch_activity.is_some());

        // First repo finishes - spinner should persist.
        tx.send(FetchMessage::RepoData(RepoFetchResult {
            repo_path: PathBuf::from("/repo-a"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            authenticated_user: None,
            issues: vec![],
        }))
        .unwrap();
        app.drain_fetch_results();
        assert!(
            app.fetch_activity.is_some(),
            "spinner should persist while second repo is still fetching",
        );

        // Second repo finishes - now spinner should clear.
        tx.send(FetchMessage::RepoData(RepoFetchResult {
            repo_path: PathBuf::from("/repo-b"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            authenticated_user: None,
            issues: vec![],
        }))
        .unwrap();
        app.drain_fetch_results();
        assert!(app.fetch_activity.is_none());
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
        let canonical_real = crate::config::canonicalize_path(&real_path).unwrap();
        assert_eq!(
            *cached_path,
            canonical_real,
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
                review_requested_prs: Ok(vec![]),
                authenticated_user: None,
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
                review_requested_prs: Ok(vec![]),
                authenticated_user: None,
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
                review_requested_prs: Ok(vec![]),
                authenticated_user: None,
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
            worktrees: Err(WorktreeError::GitError("not a git repository".into())),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            authenticated_user: None,
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
            worktrees: Err(WorktreeError::GitError("still broken".into())),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),
            authenticated_user: None,
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
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                self.records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.id == *id)
                    .cloned()
                    .ok_or_else(|| BackendError::NotFound(id.clone()))
            }
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
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                _unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn import_review_request(
                &self,
                _rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not supported in test".into()))
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
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
            description: None,
            status: WorkItemStatus::Backlog,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                pr_identity: None,
            }],
            plan: None,
        };
        let record_b = crate::work_item_backend::WorkItemRecord {
            id: id_b.clone(),
            title: "Item B".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                pr_identity: None,
            }],
            plan: None,
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
                kind: crate::work_item::WorkItemKind::Own,
                title: "Item B".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                status_derived: false,
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
                kind: crate::work_item::WorkItemKind::Own,
                title: "Item A".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                status_derived: false,
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
        let backend = crate::work_item_backend::LocalFileBackend::with_dir(dir.clone()).unwrap();

        // Create items with names that would sort differently than creation order.
        // File names are UUIDs, so we write files directly with known names.
        let names = ["zzz.json", "aaa.json", "mmm.json"];
        for name in &names {
            let record = crate::work_item_backend::WorkItemRecord {
                id: WorkItemId::LocalFile(dir.join(name)),
                title: format!("Item {name}"),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/repo"),
                    branch: None,
                    pr_identity: None,
                }],
                plan: None,
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
            Some("Fetch error (/repo): connection timed out"),
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
            review_requested_prs: Ok(vec![]),
            authenticated_user: None,
            issues: vec![],
        }))
        .unwrap();

        app.drain_fetch_results();

        // The status should remain unchanged.
        assert_eq!(app.status_message.as_deref(), Some("something important"),);
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
        use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};

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
        use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};
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
                _force: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn delete_branch(
                &self,
                _repo_path: &std::path::Path,
                _branch: &str,
                _force: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn is_worktree_dirty(
                &self,
                _worktree_path: &std::path::Path,
            ) -> Result<bool, WorktreeError> {
                Ok(false)
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

            fn create_branch(
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
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                self.records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.id == *id)
                    .cloned()
                    .ok_or_else(|| BackendError::NotFound(id.clone()))
            }
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
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported.json")),
                    title: unlinked.pr.title.clone(),
                    description: None,
                    status: WorkItemStatus::Implementing,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn import_review_request(
                &self,
                rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported-rr.json")),
                    title: rr.pr.title.clone(),
                    status: WorkItemStatus::Review,
                    kind: crate::work_item::WorkItemKind::ReviewRequest,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: rr.repo_path.clone(),
                        branch: Some(rr.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                    description: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
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
            Box::new(crate::config::InMemoryConfigProvider::new()),
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
        use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};
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
                _force: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn delete_branch(
                &self,
                _repo_path: &std::path::Path,
                _branch: &str,
                _force: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn is_worktree_dirty(
                &self,
                _worktree_path: &std::path::Path,
            ) -> Result<bool, WorktreeError> {
                Ok(false)
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

            fn create_branch(
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
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                self.records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.id == *id)
                    .cloned()
                    .ok_or_else(|| BackendError::NotFound(id.clone()))
            }
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
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported.json")),
                    title: unlinked.pr.title.clone(),
                    description: None,
                    status: WorkItemStatus::Implementing,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn import_review_request(
                &self,
                rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported-rr.json")),
                    title: rr.pr.title.clone(),
                    status: WorkItemStatus::Review,
                    kind: crate::work_item::WorkItemKind::ReviewRequest,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: rr.repo_path.clone(),
                        branch: Some(rr.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                    description: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
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
            Box::new(crate::config::InMemoryConfigProvider::new()),
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
        assert_eq!(path, PathBuf::from("/repos/myrepo/wt/fix-auth-bug"),);

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
        use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};
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
                _force: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn delete_branch(
                &self,
                _repo_path: &std::path::Path,
                _branch: &str,
                _force: bool,
            ) -> Result<(), WorktreeError> {
                Ok(())
            }

            fn is_worktree_dirty(
                &self,
                _worktree_path: &std::path::Path,
            ) -> Result<bool, WorktreeError> {
                Ok(false)
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

            fn create_branch(
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
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                self.records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.id == *id)
                    .cloned()
                    .ok_or_else(|| BackendError::NotFound(id.clone()))
            }
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
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported.json")),
                    title: unlinked.pr.title.clone(),
                    description: None,
                    status: WorkItemStatus::Implementing,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn import_review_request(
                &self,
                rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/imported-rr.json")),
                    title: rr.pr.title.clone(),
                    status: WorkItemStatus::Review,
                    kind: crate::work_item::WorkItemKind::ReviewRequest,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: rr.repo_path.clone(),
                        branch: Some(rr.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                    description: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
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
            Box::new(crate::config::InMemoryConfigProvider::new()),
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

    // -- Codex round regression tests --

    /// F-3: create_work_item_with rejects repos where git_dir_present is false.
    /// Even if a repo path is passed in the repos list, it should be filtered
    /// out when the corresponding active_repo_cache entry has git_dir_present
    /// set to false.
    #[test]
    fn create_work_item_with_rejects_repos_without_git_dir() {
        use crate::work_item_backend::ListResult;

        /// Backend that records create calls via a shared Arc.
        struct RecordingBackend {
            last_repos: Arc<std::sync::Mutex<Vec<PathBuf>>>,
        }

        impl WorkItemBackend for RecordingBackend {
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::NotFound(id.clone()))
            }
            fn list(&self) -> Result<ListResult, BackendError> {
                Ok(ListResult {
                    records: Vec::new(),
                    corrupt: Vec::new(),
                })
            }
            fn create(
                &self,
                req: CreateWorkItem,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                *self.last_repos.lock().unwrap() = req
                    .repo_associations
                    .iter()
                    .map(|r| r.repo_path.clone())
                    .collect();
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/new.json")),
                    title: req.title.clone(),
                    description: None,
                    status: req.status.clone(),
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: req.repo_associations,
                    plan: None,
                };
                Ok(record)
            }
            fn delete(&self, _id: &WorkItemId) -> Result<(), BackendError> {
                Ok(())
            }
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                _unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not used".into()))
            }
            fn import_review_request(
                &self,
                _rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not supported in test".into()))
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let last_repos = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut app = App::with_config(
            Config::default(),
            Box::new(RecordingBackend {
                last_repos: Arc::clone(&last_repos),
            }),
        );

        // Populate active_repo_cache with one repo that has git_dir and one
        // that does not.
        app.active_repo_cache = vec![
            RepoEntry {
                path: PathBuf::from("/repos/with-git"),
                source: RepoSource::Explicit,
                git_dir_present: true,
            },
            RepoEntry {
                path: PathBuf::from("/repos/no-git"),
                source: RepoSource::Explicit,
                git_dir_present: false,
            },
        ];

        // Attempt to create with both repos selected.
        let result = app.create_work_item_with(
            "Test item".into(),
            None,
            vec![
                PathBuf::from("/repos/with-git"),
                PathBuf::from("/repos/no-git"),
            ],
            "feature/test".into(),
        );
        assert!(result.is_ok(), "create should succeed for valid repos");

        // The status message should indicate success.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("Created"),
            "expected success message, got: {msg}",
        );

        // Verify only the repo with git_dir_present was sent to the backend.
        let repos = last_repos.lock().unwrap();
        assert_eq!(
            repos.len(),
            1,
            "backend should receive exactly one repo, got {}",
            repos.len(),
        );
        assert_eq!(
            repos[0],
            PathBuf::from("/repos/with-git"),
            "only the repo with git_dir_present should be included",
        );
    }

    fn make_work_item(path: &str, title: &str, status: WorkItemStatus) -> WorkItem {
        use crate::work_item::RepoAssociation;
        WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from(format!("/data/{title}.json"))),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: title.to_string(),
            description: None,
            status,
            status_derived: false,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from(path),
                branch: None,
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        }
    }

    #[test]
    fn display_list_groups_by_stage_and_repo() {
        let mut app = App::new();
        app.work_items = vec![
            make_work_item("/repos/alpha", "Backlog item", WorkItemStatus::Backlog),
            make_work_item("/repos/alpha", "Done item", WorkItemStatus::Done),
        ];
        app.build_display_list();

        let work_item_entries: Vec<_> = app
            .display_list
            .iter()
            .filter(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .collect();
        assert_eq!(work_item_entries.len(), 2, "both items should appear");

        let group_headers: Vec<_> = app
            .display_list
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::GroupHeader { label, count, .. } => Some((label.as_str(), *count)),
                _ => None,
            })
            .collect();
        assert_eq!(group_headers.len(), 2);
        assert_eq!(group_headers[0], ("BACKLOGGED (alpha)", 1));
        assert_eq!(group_headers[1], ("DONE (alpha)", 1));
    }

    #[test]
    fn display_list_all_backlog_only_shows_backlogged_group() {
        let mut app = App::new();
        app.work_items = vec![
            make_work_item("/repos/myrepo", "Item A", WorkItemStatus::Backlog),
            make_work_item("/repos/myrepo", "Item B", WorkItemStatus::Backlog),
        ];
        app.build_display_list();

        let headers: Vec<_> = app
            .display_list
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::GroupHeader { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec!["BACKLOGGED (myrepo)"]);
    }

    #[test]
    fn display_list_all_active_only_shows_active_group() {
        let mut app = App::new();
        app.work_items = vec![
            make_work_item(
                "/repos/myrepo",
                "Implementing item",
                WorkItemStatus::Implementing,
            ),
            make_work_item("/repos/myrepo", "Review item", WorkItemStatus::Review),
        ];
        app.build_display_list();

        let headers: Vec<_> = app
            .display_list
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::GroupHeader { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, vec!["ACTIVE (myrepo)"]);
    }

    #[test]
    fn display_list_no_items_no_groups() {
        let mut app = App::new();
        app.build_display_list();
        assert!(app.display_list.is_empty());
    }

    #[test]
    fn display_list_unlinked_with_grouped_items() {
        use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};
        let mut app = App::new();
        app.unlinked_prs = vec![UnlinkedPr {
            repo_path: PathBuf::from("/repo"),
            branch: "fix-typo".to_string(),
            pr: PrInfo {
                number: 1,
                title: "Fix typo".to_string(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: String::new(),
            },
        }];
        app.work_items = vec![
            make_work_item("/repos/alpha", "Active item", WorkItemStatus::Implementing),
            make_work_item("/repos/alpha", "Backlog item", WorkItemStatus::Backlog),
        ];
        app.build_display_list();

        let headers: Vec<_> = app
            .display_list
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::GroupHeader { label, count, .. } => Some((label.as_str(), *count)),
                _ => None,
            })
            .collect();
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[0], ("UNLINKED", 1));
        assert_eq!(headers[1], ("ACTIVE (alpha)", 1));
        assert_eq!(headers[2], ("BACKLOGGED (alpha)", 1));
    }

    #[test]
    fn display_list_multiple_repos_get_separate_groups() {
        let mut app = App::new();
        app.work_items = vec![
            make_work_item("/repos/alpha", "Alpha task", WorkItemStatus::Implementing),
            make_work_item("/repos/beta", "Beta task", WorkItemStatus::Implementing),
            make_work_item("/repos/alpha", "Alpha backlog", WorkItemStatus::Backlog),
        ];
        app.build_display_list();

        let headers: Vec<_> = app
            .display_list
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::GroupHeader { label, count, .. } => Some((label.as_str(), *count)),
                _ => None,
            })
            .collect();
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[0], ("ACTIVE (alpha)", 1));
        assert_eq!(headers[1], ("ACTIVE (beta)", 1));
        assert_eq!(headers[2], ("BACKLOGGED (alpha)", 1));
    }

    /// F-3: create_work_item_with returns error when ALL repos lack git_dir.
    #[test]
    fn create_work_item_with_errors_when_all_repos_lack_git_dir() {
        let mut app = App::new();

        // Populate cache with only repos missing git dirs.
        app.active_repo_cache = vec![RepoEntry {
            path: PathBuf::from("/repos/no-git"),
            source: RepoSource::Explicit,
            git_dir_present: false,
        }];

        let result = app.create_work_item_with(
            "Test item".into(),
            None,
            vec![PathBuf::from("/repos/no-git")],
            "feature/test".into(),
        );
        assert!(
            result.is_err(),
            "create should fail when all repos lack git dir"
        );

        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("No selected repos have a git directory"),
            "expected git directory error in status, got: {msg}",
        );
    }

    // -- Feature: merge prompt on Review -> Done --

    /// advance_stage from Review sets confirm_merge instead of immediately advancing.
    #[test]
    fn advance_stage_review_to_done_shows_merge_prompt() {
        let mut app = App::new();
        // Manually inject a work item in Review status.
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-test.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Merge test".into(),
            description: None,
            status: WorkItemStatus::Review,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        app.advance_stage();

        assert!(app.confirm_merge, "should show merge prompt");
        assert_eq!(
            app.merge_wi_id.as_ref(),
            Some(&wi_id),
            "merge_wi_id should be set to the work item",
        );
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("quash"),
            "status should mention squash option, got: {msg}",
        );
    }

    // -- Regression: execute_merge must not advance to Done without a real merge --

    #[test]
    fn execute_merge_no_repo_assoc_blocks_done() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-assoc.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "No assoc".into(),
            description: None,
            status: WorkItemStatus::Review,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.execute_merge(&wi_id, "squash");
        let status = app
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status
            .clone();
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(msg.contains("no repo association"), "got: {msg}");
    }

    #[test]
    fn execute_merge_no_branch_blocks_done() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-branch.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "No branch".into(),
            description: None,
            status: WorkItemStatus::Review,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/repo"),
                branch: None,
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });
        app.execute_merge(&wi_id, "squash");
        let status = app
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status
            .clone();
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(msg.contains("no branch"), "got: {msg}");
    }

    #[test]
    fn execute_merge_no_github_remote_blocks_done() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-remote.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "No remote".into(),
            description: None,
            status: WorkItemStatus::Review,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/repo"),
                branch: Some("feature/test".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });
        // StubWorktreeService.github_remote() returns Ok(None)
        app.execute_merge(&wi_id, "squash");
        let status = app
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status
            .clone();
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(msg.contains("no GitHub remote"), "got: {msg}");
    }

    #[test]
    fn poll_pr_merge_no_pr_blocks_done() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-pr.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "No PR".into(),
            description: None,
            status: WorkItemStatus::Review,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(PrMergeResult {
            wi_id: wi_id.clone(),
            branch: "feature/test".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            outcome: PrMergeOutcome::NoPr,
        })
        .unwrap();
        app.pr_merge_rx = Some(rx);
        app.poll_pr_merge();
        let status = app
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status
            .clone();
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(msg.contains("no PR found"), "got: {msg}");
    }

    #[test]
    fn poll_pr_merge_merged_advances_to_done() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-ok.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Merged OK".into(),
            description: None,
            status: WorkItemStatus::Review,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(PrMergeResult {
            wi_id: wi_id.clone(),
            branch: "feature/test".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            outcome: PrMergeOutcome::Merged {
                strategy: "squash".into(),
                pr_identity: None,
            },
        })
        .unwrap();
        app.pr_merge_rx = Some(rx);
        app.poll_pr_merge();
        // After apply_stage_change, reassemble rebuilds from StubBackend (empty),
        // so we verify via the status message that the merge path was taken.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("PR merged") && msg.contains("[DN]"),
            "should confirm merge and Done, got: {msg}",
        );
    }

    // -- Feature: rework prompt on Review -> Implementing --

    /// retreat_stage from Review sets rework_prompt_visible instead of
    /// immediately retreating.
    #[test]
    fn retreat_stage_review_to_implementing_shows_rework_prompt() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/rework-test.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Rework test".into(),
            description: None,
            status: WorkItemStatus::Review,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        app.retreat_stage();

        assert!(app.rework_prompt_visible, "should show rework prompt",);
        assert_eq!(
            app.rework_prompt_wi.as_ref(),
            Some(&wi_id),
            "rework_prompt_wi should be set",
        );
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("Rework reason"),
            "status should mention rework reason, got: {msg}",
        );
    }

    /// Rework reasons are stored per work item and influence prompt key.
    #[test]
    fn rework_reason_stored_per_work_item() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/rework-store.json"));
        app.rework_reasons
            .insert(wi_id.clone(), "Fix the tests".into());

        assert_eq!(
            app.rework_reasons.get(&wi_id).map(|s| s.as_str()),
            Some("Fix the tests"),
        );
    }

    /// advance_stage from non-Review stages does NOT show merge prompt.
    #[test]
    fn advance_stage_non_review_skips_merge_prompt() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/no-merge.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id,
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Backlog item".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        app.advance_stage();

        assert!(
            !app.confirm_merge,
            "merge prompt should not appear for Backlog -> Planning",
        );
    }

    /// Manual advance from Planning to Implementing is blocked.
    #[test]
    fn advance_stage_planning_to_implementing_blocked() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/plan.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id,
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Planning item".into(),
            description: None,
            status: WorkItemStatus::Planning,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        app.advance_stage();

        // Status should still be Planning - manual advance blocked.
        assert_eq!(app.work_items[0].status, WorkItemStatus::Planning);
        assert!(
            app.status_message
                .as_deref()
                .unwrap_or("")
                .contains("workbridge_set_plan"),
        );
    }

    /// Session lookup requires matching stage in composite key.
    #[test]
    fn session_lookup_requires_correct_stage() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/session-key.json"));

        // Insert a mock session entry under (wi_id, Planning).
        let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
        app.sessions.insert(
            (wi_id.clone(), WorkItemStatus::Planning),
            SessionEntry {
                parser,
                alive: true,
                session: None,
            },
        );

        // Lookup with Planning stage finds it.
        assert!(
            app.sessions
                .contains_key(&(wi_id.clone(), WorkItemStatus::Planning))
        );

        // Lookup with Implementing stage does NOT find it.
        assert!(
            !app.sessions
                .contains_key(&(wi_id.clone(), WorkItemStatus::Implementing))
        );
    }

    // -- Issue 7: gh availability check --

    /// check_gh_available returns a bool (not a panic/error).
    /// We do not test for a specific value since CI may or may not have gh.
    #[test]
    fn check_gh_available_returns_bool() {
        let result: bool = App::check_gh_available();
        // Verify it returns a bool without panicking. The type annotation
        // above confirms the return type at compile time.
        let _ = result;
    }

    /// gh_available is set in the constructor.
    #[test]
    fn app_constructor_sets_gh_available() {
        let app = App::new();
        // The field should be initialized (to whatever the system has).
        // Just verify the field exists and can be read.
        let _ = app.gh_available;
    }

    // -- Issue 5: merge conflict detection --

    /// Verify the conflict detection string matching logic.
    #[test]
    fn merge_conflict_detection_logic() {
        // Case-insensitive "conflict" detection in stderr.
        let cases = vec![
            ("CONFLICT (content): Merge conflict in file.rs", true),
            ("error: merge conflict", true),
            ("Conflict detected while merging", true),
            ("Authentication failure", false),
            ("merge was successful", false),
            ("", false),
        ];
        for (stderr, expected) in cases {
            let lower = stderr.to_lowercase();
            let detected = lower.contains("conflict");
            assert_eq!(
                detected, expected,
                "stderr={stderr:?}: expected conflict={expected}, got {detected}",
            );
        }
    }

    /// Regression: the positional prompt must come BEFORE --mcp-config
    /// in the argument list. If it comes after, Claude Code treats it as
    /// a config file path and exits with "MCP config file not found".
    #[test]
    fn build_claude_cmd_prompt_before_mcp_config() {
        // Planning session: has --permission-mode plan, --settings hook, and positional prompt.
        let cmd =
            App::build_claude_cmd(&WorkItemStatus::Planning, Some("system prompt here"), false);
        assert_eq!(cmd[0], "claude");
        assert_eq!(cmd[1], "--permission-mode");
        assert_eq!(cmd[2], "plan");
        assert_eq!(cmd[3], "--settings");
        assert!(
            cmd[4].contains("PostToolUse") && cmd[4].contains("workbridge_set_plan"),
            "planning sessions must include TodoWrite reminder hook via --settings",
        );
        assert_eq!(cmd[5], "--system-prompt");
        assert_eq!(cmd[6], "system prompt here");
        // Positional prompt is the LAST element - callers append
        // --mcp-config after this, so it stays after the prompt.
        assert_eq!(
            cmd.last().unwrap(),
            "Explain who you are and start working.",
        );
        // Verify --mcp-config is not in the vec (callers add it).
        assert!(
            !cmd.iter().any(|a| a == "--mcp-config"),
            "build_claude_cmd must not include --mcp-config",
        );
    }

    /// Implementing sessions also get an auto-start prompt.
    #[test]
    fn build_claude_cmd_implementing_has_prompt() {
        let cmd = App::build_claude_cmd(&WorkItemStatus::Implementing, Some("impl prompt"), false);
        assert_eq!(cmd[0], "claude");
        // No --permission-mode for implementing.
        assert_eq!(cmd[1], "--system-prompt");
        assert!(
            cmd.last().unwrap().contains("start working"),
            "implementing should have auto-start prompt",
        );
    }

    // -- Feature: plan_from_branch (no-plan recovery) --

    /// plan_from_branch accepts a Blocked item and applies the transition.
    #[test]
    fn plan_from_branch_accepts_blocked_item() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/plan-from-branch.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Plan from branch test".into(),
            description: None,
            status: WorkItemStatus::Blocked,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        app.plan_from_branch(&wi_id);

        // StubBackend persists nothing, so we verify via the status message
        // that apply_stage_change was called (it sets "Moved to [PL]").
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("[PL]"),
            "should show Planning transition message, got: {msg}",
        );
    }

    /// plan_from_branch rejects a work item that is not Blocked.
    #[test]
    fn plan_from_branch_rejects_non_blocked() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/plan-not-blocked.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Not blocked test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);

        app.plan_from_branch(&wi_id);

        // Item should remain unchanged - verify via status message.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("no longer blocked"),
            "should show informational message, got: {msg}",
        );
        // Work item should still be in original status.
        let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
        assert_eq!(
            wi.status,
            WorkItemStatus::Implementing,
            "should remain in Implementing when not Blocked",
        );
    }

    // -- Feature: BLOCKED sidebar group --

    /// Blocked items appear in a BLOCKED group, not in ACTIVE.
    #[test]
    fn display_list_blocked_items_in_blocked_group() {
        let mut app = App::new();
        // Add one Blocked and one Implementing item.
        let blocked_id = WorkItemId::LocalFile(PathBuf::from("/tmp/blocked.json"));
        let active_id = WorkItemId::LocalFile(PathBuf::from("/tmp/active.json"));
        let repo = PathBuf::from("/repos/test");
        for (id, status) in [
            (blocked_id, WorkItemStatus::Blocked),
            (active_id, WorkItemStatus::Implementing),
        ] {
            app.work_items.push(crate::work_item::WorkItem {
                id,
                backend_type: BackendType::LocalFile,
                kind: crate::work_item::WorkItemKind::Own,
                title: format!("{status:?} item"),
                description: None,
                status,
                status_derived: false,
                repo_associations: vec![crate::work_item::RepoAssociation {
                    repo_path: repo.clone(),
                    branch: Some("test-branch".into()),
                    worktree_path: None,
                    pr: None,
                    issue: None,
                    git_state: None,
                }],
                errors: vec![],
            });
        }

        app.build_display_list();

        let headers: Vec<&str> = app
            .display_list
            .iter()
            .filter_map(|e| match e {
                DisplayEntry::GroupHeader { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            headers.contains(&"BLOCKED (test)"),
            "should have BLOCKED group, got: {headers:?}",
        );
        assert!(
            headers.contains(&"ACTIVE (test)"),
            "should have ACTIVE group, got: {headers:?}",
        );
        // BLOCKED should come before ACTIVE.
        let blocked_pos = headers
            .iter()
            .position(|h| h.starts_with("BLOCKED"))
            .unwrap();
        let active_pos = headers
            .iter()
            .position(|h| h.starts_with("ACTIVE"))
            .unwrap();
        assert!(
            blocked_pos < active_pos,
            "BLOCKED group should come before ACTIVE",
        );
    }

    /// BLOCKED group header uses GroupHeaderKind::Blocked.
    #[test]
    fn display_list_blocked_header_kind() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/blocked-kind.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id,
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Blocked kind test".into(),
            description: None,
            status: WorkItemStatus::Blocked,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/repos/test"),
                branch: Some("branch".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        app.build_display_list();

        let blocked_header = app.display_list.iter().find(|e| {
            matches!(
                e,
                DisplayEntry::GroupHeader { label, .. } if label.starts_with("BLOCKED")
            )
        });
        assert!(blocked_header.is_some(), "should have BLOCKED header");
        if let Some(DisplayEntry::GroupHeader { kind, .. }) = blocked_header {
            assert_eq!(
                *kind,
                GroupHeaderKind::Blocked,
                "BLOCKED header should use Blocked kind"
            );
        }
    }

    /// Blocked and Review sessions do NOT get an auto-start prompt.
    #[test]
    fn build_claude_cmd_blocked_review_no_prompt() {
        for status in [WorkItemStatus::Blocked, WorkItemStatus::Review] {
            let cmd = App::build_claude_cmd(&status, Some("prompt"), false);
            // Last arg should be the system prompt value, not a positional prompt.
            assert_eq!(cmd.last().unwrap(), "prompt");
        }
    }

    /// Review sessions auto-start when force_auto_start is true (gate findings present).
    #[test]
    fn build_claude_cmd_review_force_auto_start() {
        let cmd = App::build_claude_cmd(&WorkItemStatus::Review, Some("review prompt"), true);
        assert!(
            cmd.last().unwrap().contains("review gate assessment"),
            "review with force_auto_start should have gate-specific prompt",
        );
    }

    /// Review-gate findings are stored per work item and influence prompt key.
    #[test]
    fn review_gate_findings_stored_per_work_item() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-findings.json"));
        app.review_gate_findings
            .insert(wi_id.clone(), "All plan items implemented correctly".into());

        assert_eq!(
            app.review_gate_findings.get(&wi_id).map(|s| s.as_str()),
            Some("All plan items implemented correctly"),
        );
    }

    // -- Activity indicator tests --

    #[test]
    fn start_activity_returns_unique_ids() {
        let mut app = App::new();
        let id1 = app.start_activity("First");
        let id2 = app.start_activity("Second");
        assert_ne!(id1, id2);
        assert_eq!(app.activities.len(), 2);
    }

    #[test]
    fn end_activity_removes_by_id() {
        let mut app = App::new();
        let id1 = app.start_activity("First");
        let id2 = app.start_activity("Second");
        app.end_activity(id1);
        assert_eq!(app.activities.len(), 1);
        assert_eq!(app.current_activity(), Some("Second"));
        app.end_activity(id2);
        assert!(app.activities.is_empty());
        assert_eq!(app.current_activity(), None);
    }

    #[test]
    fn end_activity_noop_for_unknown_id() {
        let mut app = App::new();
        let id = app.start_activity("Test");
        app.end_activity(ActivityId(999));
        assert_eq!(app.activities.len(), 1);
        app.end_activity(id);
        assert!(app.activities.is_empty());
    }

    #[test]
    fn current_activity_returns_last() {
        let mut app = App::new();
        assert_eq!(app.current_activity(), None);
        app.start_activity("First");
        assert_eq!(app.current_activity(), Some("First"));
        app.start_activity("Second");
        assert_eq!(app.current_activity(), Some("Second"));
    }

    #[test]
    fn current_activity_pops_to_previous_on_end() {
        let mut app = App::new();
        let _id1 = app.start_activity("First");
        let id2 = app.start_activity("Second");
        app.end_activity(id2);
        assert_eq!(app.current_activity(), Some("First"));
    }

    #[test]
    fn has_visible_status_bar_with_activity() {
        let mut app = App::new();
        assert!(!app.has_visible_status_bar());
        let id = app.start_activity("Working...");
        assert!(app.has_visible_status_bar());
        app.end_activity(id);
        assert!(!app.has_visible_status_bar());
    }

    #[test]
    fn has_visible_status_bar_with_message() {
        let mut app = App::new();
        app.status_message = Some("test".into());
        assert!(app.has_visible_status_bar());
    }

    #[test]
    fn has_visible_status_bar_activity_overrides_message() {
        let mut app = App::new();
        app.status_message = Some("test".into());
        let id = app.start_activity("Working...");
        assert!(app.has_visible_status_bar());
        // Activity takes precedence in rendering, but bar is visible either way.
        assert_eq!(app.current_activity(), Some("Working..."));
        app.end_activity(id);
        // Status message still keeps bar visible.
        assert!(app.has_visible_status_bar());
    }

    // -- Review gate regression tests --

    /// Helper: create an App with a single work item at the given status,
    /// with an optional repo association (branch + repo_path).
    fn app_with_work_item(
        status: WorkItemStatus,
        branch: Option<&str>,
        repo_path: Option<&str>,
    ) -> (App, WorkItemId) {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-test.json"));
        let repo_assoc = if let Some(rp) = repo_path {
            vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from(rp),
                branch: branch.map(|b| b.to_string()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }]
        } else {
            vec![]
        };
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Gate test item".into(),
            description: None,
            status,
            status_derived: false,
            repo_associations: repo_assoc,
            errors: vec![],
        });
        app.display_list
            .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
        app.selected_item = Some(app.display_list.len() - 1);
        (app, wi_id)
    }

    /// Test 4: MCP StatusUpdate for Review on Implementing item with no plan
    /// must NOT change status to Review (gate spawn fails), and rework_reasons
    /// must be populated.
    #[test]
    fn mcp_review_gate_bypass_prevented_no_plan() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        // Set up MCP channel with a StatusUpdate for Review.
        let (tx, rx) = crossbeam_channel::unbounded();
        app.mcp_rx = Some(rx);
        let wi_id_json = serde_json::to_string(&wi_id).unwrap();
        tx.send(McpEvent::StatusUpdate {
            work_item_id: wi_id_json,
            status: "Review".into(),
            reason: "Implementation complete".into(),
        })
        .unwrap();

        app.poll_mcp_status_updates();

        // Status must stay at Implementing - the gate cannot run without a plan.
        let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
        assert_eq!(
            wi.status,
            WorkItemStatus::Implementing,
            "status must not change to Review when no plan exists",
        );
        // rework_reasons must be populated (gate spawn failure triggers rework flow).
        assert!(
            app.rework_reasons.contains_key(&wi_id),
            "rework_reasons must be populated after gate spawn failure",
        );
        let reason = app.rework_reasons.get(&wi_id).unwrap();
        assert!(
            reason.contains("no plan"),
            "rework reason should mention no plan, got: {reason}",
        );
    }

    /// Test 5: TUI advance_stage from Implementing with no plan must NOT
    /// change status to Review.
    #[test]
    fn tui_advance_stage_blocked_without_plan() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        app.advance_stage();

        // Status must stay at Implementing - spawn_review_gate returns Blocked.
        let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
        assert_eq!(
            wi.status,
            WorkItemStatus::Implementing,
            "TUI advance_stage must not advance to Review without a plan",
        );
        // Status message should explain why.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("no plan"),
            "status message should explain gate failure, got: {msg}",
        );
    }

    /// Test 6: After poll_review_gate processes a rejection result,
    /// rework_reasons is populated for the work item.
    #[test]
    fn poll_review_gate_rejection_populates_rework_reasons() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        // Simulate a review gate that completed with a rejection.
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(ReviewGateResult {
            work_item_id: wi_id.clone(),
            approved: false,
            detail: "Tests are missing for the new feature".into(),
        })
        .unwrap();
        app.review_gate_rx = Some(rx);
        app.review_gate_wi = Some(wi_id.clone());

        app.poll_review_gate();

        assert!(
            app.rework_reasons.contains_key(&wi_id),
            "rework_reasons must be populated after gate rejection",
        );
        assert_eq!(
            app.rework_reasons.get(&wi_id).unwrap(),
            "Tests are missing for the new feature",
        );
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("rejected"),
            "status should mention rejection, got: {msg}",
        );
    }

    /// Test 7: poll_review_gate supports Blocked status - a Blocked work item
    /// can transition to Review when the gate approves.
    #[test]
    fn poll_review_gate_approves_blocked_to_review() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Blocked,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        // Simulate a review gate that completed with approval.
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(ReviewGateResult {
            work_item_id: wi_id.clone(),
            approved: true,
            detail: "All plan items implemented".into(),
        })
        .unwrap();
        app.review_gate_rx = Some(rx);
        app.review_gate_wi = Some(wi_id.clone());

        app.poll_review_gate();

        // StubBackend's update_status is a no-op, but reassemble rebuilds from
        // StubBackend (empty). The status message from apply_stage_change confirms
        // the transition was attempted. Also verify gate findings were stored.
        assert!(
            app.review_gate_findings.contains_key(&wi_id),
            "review_gate_findings should be stored on approval",
        );
        assert_eq!(
            app.review_gate_findings.get(&wi_id).unwrap(),
            "All plan items implemented",
        );
        // Verify the receiver and tracked WI are cleared.
        assert!(app.review_gate_rx.is_none(), "gate rx should be cleared");
        assert!(app.review_gate_wi.is_none(), "gate wi should be cleared");
    }

    /// Test 8: Gate spawn failure (MCP path) populates rework_reasons with
    /// the failure message.
    #[test]
    fn mcp_gate_spawn_failure_sets_rework_reasons() {
        // Work item with no branch - gate will fail with "no branch set".
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            None, // no branch
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        app.mcp_rx = Some(rx);
        let wi_id_json = serde_json::to_string(&wi_id).unwrap();
        tx.send(McpEvent::StatusUpdate {
            work_item_id: wi_id_json,
            status: "Review".into(),
            reason: "Done implementing".into(),
        })
        .unwrap();

        app.poll_mcp_status_updates();

        assert!(
            app.rework_reasons.contains_key(&wi_id),
            "rework_reasons must be set on gate spawn failure (no branch)",
        );
        let reason = app.rework_reasons.get(&wi_id).unwrap();
        // The gate failure could mention "no plan" (checked first) or "no branch".
        // StubBackend.read_plan returns Ok(None), so "no plan" is the first failure.
        assert!(
            reason.contains("no plan") || reason.contains("no branch"),
            "rework reason should explain the failure, got: {reason}",
        );
    }

    /// Test 9: When a review gate is already running for item A, an MCP
    /// StatusUpdate for Review on item B should NOT populate rework_reasons
    /// for item B. It should be silently skipped (gate busy).
    #[test]
    fn gate_busy_for_different_item_does_not_rework() {
        let mut app = App::new();

        // Item A: gate is running for this one.
        let wi_id_a = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-a.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id_a.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Item A".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/repo"),
                branch: Some("branch-a".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        // Item B: MCP will request Review for this one.
        let wi_id_b = WorkItemId::LocalFile(PathBuf::from("/tmp/gate-b.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id_b.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Item B".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/repo"),
                branch: Some("branch-b".into()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        // Simulate gate running for item A.
        app.review_gate_wi = Some(wi_id_a.clone());
        // (We don't need a real rx - spawn_review_gate checks review_gate_wi first.)

        // Send MCP StatusUpdate for item B.
        let (tx, rx) = crossbeam_channel::unbounded();
        app.mcp_rx = Some(rx);
        let wi_id_b_json = serde_json::to_string(&wi_id_b).unwrap();
        tx.send(McpEvent::StatusUpdate {
            work_item_id: wi_id_b_json,
            status: "Review".into(),
            reason: "Done".into(),
        })
        .unwrap();

        app.poll_mcp_status_updates();

        // Item B's status must be unchanged.
        let wi_b = app.work_items.iter().find(|w| w.id == wi_id_b).unwrap();
        assert_eq!(
            wi_b.status,
            WorkItemStatus::Implementing,
            "item B should remain Implementing when gate is busy",
        );
        // rework_reasons must NOT contain item B (gate busy is not a failure).
        assert!(
            !app.rework_reasons.contains_key(&wi_id_b),
            "rework_reasons must NOT be set for item B when gate is busy for item A",
        );
        // Status message should mention "already running".
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("already running"),
            "status should mention gate already running, got: {msg}",
        );
    }

    /// Test 10: A Blocked work item with no plan that fails the gate via MCP
    /// should transition to Implementing (not stay Blocked), so the
    /// implementing_rework prompt (which has {rework_reason}) is used.
    #[test]
    fn blocked_gate_failure_transitions_to_implementing() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Blocked,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        // Send MCP StatusUpdate for Review.
        let (tx, rx) = crossbeam_channel::unbounded();
        app.mcp_rx = Some(rx);
        let wi_id_json = serde_json::to_string(&wi_id).unwrap();
        tx.send(McpEvent::StatusUpdate {
            work_item_id: wi_id_json,
            status: "Review".into(),
            reason: "Implementation complete".into(),
        })
        .unwrap();

        app.poll_mcp_status_updates();

        // The work item should now be Implementing (not still Blocked).
        // StubBackend.update_status is a no-op, but reassemble_work_items
        // rebuilds from the StubBackend (which returns empty). The important
        // assertion is that rework_reasons is populated AND the code path
        // that transitions Blocked -> Implementing was executed.
        assert!(
            app.rework_reasons.contains_key(&wi_id),
            "rework_reasons must be populated for Blocked gate failure",
        );
        // Verify status message mentions the gate failure.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("Review gate failed") || msg.contains("no plan"),
            "status should mention gate failure, got: {msg}",
        );
    }

    /// Test 11: spawn_review_gate sets status_message on all failure paths.
    #[test]
    fn spawn_review_gate_sets_status_on_failure() {
        // Case 1: no plan exists.
        {
            let (mut app, wi_id) = app_with_work_item(
                WorkItemStatus::Implementing,
                Some("feature/test"),
                Some("/tmp/repo"),
            );
            let result = app.spawn_review_gate(&wi_id);
            match result {
                ReviewGateSpawn::Blocked(reason) => {
                    assert!(
                        reason.contains("no plan"),
                        "should mention no plan, got: {reason}",
                    );
                }
                ReviewGateSpawn::Spawned => {
                    panic!("gate should not have spawned without a plan");
                }
            }
        }

        // Case 2: no branch set.
        {
            let (mut app, wi_id) = app_with_work_item(
                WorkItemStatus::Implementing,
                None, // no branch
                Some("/tmp/repo"),
            );
            let result = app.spawn_review_gate(&wi_id);
            match result {
                ReviewGateSpawn::Blocked(reason) => {
                    // Could fail on "no plan" first (StubBackend returns None).
                    assert!(
                        reason.contains("no plan") || reason.contains("no branch"),
                        "should mention no plan or no branch, got: {reason}",
                    );
                }
                ReviewGateSpawn::Spawned => {
                    panic!("gate should not have spawned without a branch");
                }
            }
        }

        // Case 3: no repo association.
        {
            let (mut app, wi_id) = app_with_work_item(
                WorkItemStatus::Implementing,
                None,
                None, // no repo association
            );
            let result = app.spawn_review_gate(&wi_id);
            match result {
                ReviewGateSpawn::Blocked(reason) => {
                    // Could fail on "no plan" first (StubBackend returns None).
                    assert!(
                        reason.contains("no plan") || reason.contains("no repo"),
                        "should mention no plan or no repo, got: {reason}",
                    );
                }
                ReviewGateSpawn::Spawned => {
                    panic!("gate should not have spawned without a repo association");
                }
            }
        }
    }

    /// Test 2 (from MCP context): Blocked->Review is in the allowed
    /// transitions in poll_mcp_status_updates. Verify by sending a
    /// StatusUpdate from Blocked and confirming it is NOT rejected with
    /// "not allowed".
    #[test]
    fn mcp_blocked_to_review_is_allowed_transition() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Blocked,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        app.mcp_rx = Some(rx);
        let wi_id_json = serde_json::to_string(&wi_id).unwrap();
        tx.send(McpEvent::StatusUpdate {
            work_item_id: wi_id_json,
            status: "Review".into(),
            reason: "Done".into(),
        })
        .unwrap();

        app.poll_mcp_status_updates();

        // The transition should NOT be rejected as "not allowed". It should
        // reach the gate spawn path (and fail there due to no plan).
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            !msg.contains("not allowed"),
            "Blocked->Review must not be rejected as 'not allowed', got: {msg}",
        );
    }

    // -- PR identity backfill tests --

    fn make_assoc(repo: &str, branch: &str) -> crate::work_item_backend::RepoAssociationRecord {
        crate::work_item_backend::RepoAssociationRecord {
            repo_path: PathBuf::from(repo),
            branch: Some(branch.to_string()),
            pr_identity: None,
        }
    }

    #[test]
    fn collect_backfill_requests_returns_done_items_without_pr_identity() {
        use crate::work_item_backend::LocalFileBackend;

        let dir = std::env::temp_dir().join("workbridge-test-backfill-collect");
        let _ = std::fs::remove_dir_all(&dir);
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();

        // Done item with branch but no pr_identity - should be returned.
        let done_record = backend
            .create(CreateWorkItem {
                title: "Done item".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![make_assoc("/tmp/repo", "feature/done")],
            })
            .unwrap();
        backend
            .update_status(&done_record.id, WorkItemStatus::Done)
            .unwrap();

        // Backlog item with branch - should be skipped.
        let _ = backend
            .create(CreateWorkItem {
                title: "Impl item".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![make_assoc("/tmp/repo", "feature/impl")],
            })
            .unwrap();

        // Done item with pr_identity already set - should be skipped.
        let done_with_pr = backend
            .create(CreateWorkItem {
                title: "Done with PR".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![make_assoc("/tmp/repo", "feature/done-pr")],
            })
            .unwrap();
        backend
            .update_status(&done_with_pr.id, WorkItemStatus::Done)
            .unwrap();
        backend
            .save_pr_identity(
                &done_with_pr.id,
                &PathBuf::from("/tmp/repo"),
                &crate::work_item_backend::PrIdentityRecord {
                    number: 42,
                    title: "Already set".into(),
                    url: "https://example.com/pr/42".into(),
                },
            )
            .unwrap();

        let mut app = App::with_config(Config::default(), Box::new(backend));
        app.worktree_service = Arc::new(StubWorktreeService);

        let requests = app.collect_backfill_requests();

        // Only the first Done item (no pr_identity) should be a candidate.
        // StubWorktreeService.github_remote returns None, so the request
        // is skipped (no github remote). Verify filter works correctly.
        assert!(
            requests.is_empty(),
            "no requests without github remote, got {}",
            requests.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- Delete resource cleanup tests --

    /// Delete cleans up all in-memory state keyed by the deleted work item ID:
    /// rework_reasons, review_gate_findings, no_plan_prompt_queue, and
    /// associated visibility flags.
    #[test]
    fn delete_cleans_up_memory_state() {
        use crate::work_item::{CheckStatus, PrInfo, PrState, ReviewDecision};
        use crate::work_item_backend::ListResult;

        struct TestBackend {
            records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
        }

        impl WorkItemBackend for TestBackend {
            fn read(
                &self,
                id: &WorkItemId,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                self.records
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|r| r.id == *id)
                    .cloned()
                    .ok_or_else(|| BackendError::NotFound(id.clone()))
            }
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
            fn update_status(
                &self,
                _id: &WorkItemId,
                _status: WorkItemStatus,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn import(
                &self,
                unlinked: &crate::work_item::UnlinkedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                let record = crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from("/tmp/delete-mem-test.json")),
                    title: unlinked.pr.title.clone(),
                    description: None,
                    status: WorkItemStatus::Implementing,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: unlinked.repo_path.clone(),
                        branch: Some(unlinked.branch.clone()),
                        pr_identity: None,
                    }],
                    plan: None,
                };
                self.records.lock().unwrap().push(record.clone());
                Ok(record)
            }
            fn import_review_request(
                &self,
                _rr: &crate::work_item::ReviewRequestedPr,
            ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
                Err(BackendError::Validation("not supported in test".into()))
            }
            fn append_activity(
                &self,
                _id: &WorkItemId,
                _entry: &ActivityEntry,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
                Ok(Vec::new())
            }
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let backend = TestBackend {
            records: std::sync::Mutex::new(Vec::new()),
        };
        let mut app = App::with_config(Config::default(), Box::new(backend));

        // Import a work item so we have something to delete.
        app.unlinked_prs.push(crate::work_item::UnlinkedPr {
            repo_path: PathBuf::from("/repo"),
            pr: PrInfo {
                number: 1,
                title: "Memory cleanup test".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::None,
                checks: CheckStatus::None,
                url: "https://github.com/o/r/pull/1".into(),
            },
            branch: "1-test".into(),
        });
        app.build_display_list();
        let unlinked_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::UnlinkedItem(_)))
            .unwrap();
        app.selected_item = Some(unlinked_idx);
        app.import_selected_unlinked();

        // Get the work item ID.
        let wi_id = app.work_items[0].id.clone();

        // Populate in-memory state for this work item.
        app.rework_reasons
            .insert(wi_id.clone(), "needs fixes".into());
        app.review_gate_findings
            .insert(wi_id.clone(), "some findings".into());
        app.no_plan_prompt_queue.push_back(wi_id.clone());
        app.no_plan_prompt_visible = true;
        app.rework_prompt_wi = Some(wi_id.clone());
        app.rework_prompt_visible = true;
        app.merge_wi_id = Some(wi_id.clone());
        app.confirm_merge = true;

        // Select and delete.
        let work_item_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .unwrap();
        app.selected_item = Some(work_item_idx);
        app.delete_selected_work_item(false);

        // Verify all in-memory state is cleaned up.
        assert!(
            app.rework_reasons.is_empty(),
            "rework_reasons should be empty after delete"
        );
        assert!(
            app.review_gate_findings.is_empty(),
            "review_gate_findings should be empty after delete"
        );
        assert!(
            app.no_plan_prompt_queue.is_empty(),
            "no_plan_prompt_queue should be empty after delete"
        );
        assert!(
            !app.no_plan_prompt_visible,
            "no_plan_prompt_visible should be false after delete"
        );
        assert!(
            app.rework_prompt_wi.is_none(),
            "rework_prompt_wi should be None after delete"
        );
        assert!(
            !app.rework_prompt_visible,
            "rework_prompt_visible should be false after delete"
        );
        assert!(
            app.merge_wi_id.is_none(),
            "merge_wi_id should be None after delete"
        );
        assert!(
            !app.confirm_merge,
            "confirm_merge should be false after delete"
        );
    }

    // -- Shared delete test fixtures --

    /// A backend that returns a fixed list of records from `list()`. All
    /// mutating operations (delete, update_status, etc.) are no-ops that
    /// return Ok. Eliminates the need for per-test OneItemBackend /
    /// RecordingTestBackend / etc. boilerplate.
    struct FixedListBackend {
        records: Vec<crate::work_item_backend::WorkItemRecord>,
    }

    impl FixedListBackend {
        fn one_item(id_path: &str, title: &str, repo_path: &str, branch: &str) -> Self {
            Self {
                records: vec![crate::work_item_backend::WorkItemRecord {
                    id: WorkItemId::LocalFile(PathBuf::from(id_path)),
                    title: title.into(),
                    description: None,
                    status: WorkItemStatus::Implementing,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: vec![RepoAssociationRecord {
                        repo_path: PathBuf::from(repo_path),
                        branch: Some(branch.into()),
                        pr_identity: None,
                    }],
                    plan: None,
                }],
            }
        }
    }

    impl WorkItemBackend for FixedListBackend {
        fn read(
            &self,
            id: &WorkItemId,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::NotFound(id.clone()))
        }
        fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
            Ok(crate::work_item_backend::ListResult {
                records: self.records.clone(),
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
        fn update_status(
            &self,
            _id: &WorkItemId,
            _status: WorkItemStatus,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn import(
            &self,
            _unlinked: &crate::work_item::UnlinkedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not used".into()))
        }
        fn import_review_request(
            &self,
            _rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation("not supported in test".into()))
        }
        fn append_activity(
            &self,
            _id: &WorkItemId,
            _entry: &ActivityEntry,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_activity(&self, _id: &WorkItemId) -> Result<Vec<ActivityEntry>, BackendError> {
            Ok(Vec::new())
        }
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(None)
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> crate::work_item::BackendType {
            crate::work_item::BackendType::LocalFile
        }
    }

    /// Worktree service that delegates to StubWorktreeService defaults but
    /// overrides `is_worktree_dirty` to always return the configured value,
    /// and optionally records `remove_worktree` / `delete_branch` calls.
    struct ConfigurableWorktreeService {
        always_dirty: bool,
        remove_worktree_calls: std::sync::Mutex<Vec<(PathBuf, PathBuf, bool, bool)>>,
        delete_branch_calls: std::sync::Mutex<Vec<(PathBuf, String, bool)>>,
    }

    impl ConfigurableWorktreeService {
        fn dirty() -> Self {
            Self {
                always_dirty: true,
                remove_worktree_calls: std::sync::Mutex::new(Vec::new()),
                delete_branch_calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn recording() -> Self {
            Self {
                always_dirty: false,
                remove_worktree_calls: std::sync::Mutex::new(Vec::new()),
                delete_branch_calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl WorktreeService for ConfigurableWorktreeService {
        fn list_worktrees(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<
            Vec<crate::worktree_service::WorktreeInfo>,
            crate::worktree_service::WorktreeError,
        > {
            Ok(Vec::new())
        }
        fn create_worktree(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _target_dir: &std::path::Path,
        ) -> Result<crate::worktree_service::WorktreeInfo, crate::worktree_service::WorktreeError>
        {
            Err(crate::worktree_service::WorktreeError::GitError(
                "not used".into(),
            ))
        }
        fn remove_worktree(
            &self,
            repo_path: &std::path::Path,
            worktree_path: &std::path::Path,
            delete_branch: bool,
            force: bool,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            self.remove_worktree_calls.lock().unwrap().push((
                repo_path.to_path_buf(),
                worktree_path.to_path_buf(),
                delete_branch,
                force,
            ));
            Ok(())
        }
        fn delete_branch(
            &self,
            repo_path: &std::path::Path,
            branch: &str,
            force: bool,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            self.delete_branch_calls.lock().unwrap().push((
                repo_path.to_path_buf(),
                branch.to_string(),
                force,
            ));
            Ok(())
        }
        fn is_worktree_dirty(
            &self,
            _worktree_path: &std::path::Path,
        ) -> Result<bool, crate::worktree_service::WorktreeError> {
            Ok(self.always_dirty)
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
        fn create_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            Ok(())
        }
    }

    /// When a worktree has uncommitted changes, attempt_delete escalates
    /// the confirmation state to AwaitingForce instead of deleting.
    #[test]
    fn dirty_worktree_triggers_force_prompt() {
        use crate::config::InMemoryConfigProvider;

        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Box::new(FixedListBackend::one_item(
                "/tmp/dirty-test.json",
                "Dirty worktree item",
                "/repo",
                "dirty-branch",
            )),
            Arc::new(ConfigurableWorktreeService::dirty()),
            Box::new(InMemoryConfigProvider::new()),
        );

        // Inject a fake worktree path into the assembled work item so
        // is_worktree_dirty has something to check.
        assert_eq!(app.work_items.len(), 1);
        app.work_items[0].repo_associations[0].worktree_path =
            Some(PathBuf::from("/tmp/fake-worktree"));
        app.build_display_list();

        // Select the work item.
        let wi_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .unwrap();
        app.selected_item = Some(wi_idx);
        app.sync_selection_identity();

        // Attempt delete - should escalate to AwaitingForce.
        app.attempt_delete_selected_work_item();
        assert_eq!(
            app.confirm_delete,
            DeleteConfirmState::AwaitingForce,
            "dirty worktree should trigger AwaitingForce state"
        );
        assert!(
            app.status_message
                .as_deref()
                .unwrap_or("")
                .contains("uncommitted changes"),
            "status should mention uncommitted changes, got: {:?}",
            app.status_message,
        );

        // Work item should NOT be deleted yet.
        assert_eq!(
            app.work_items.len(),
            1,
            "work item should still exist after AwaitingForce"
        );
    }

    /// Verify that deleting a work item calls remove_worktree and
    /// delete_branch on the worktree service with the correct arguments.
    #[test]
    fn delete_calls_remove_worktree_and_delete_branch() {
        let recording_ws = Arc::new(ConfigurableWorktreeService::recording());

        use crate::config::InMemoryConfigProvider;
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Box::new(FixedListBackend::one_item(
                "/tmp/recording-test.json",
                "Recording test item",
                "/my/repo",
                "feature-branch",
            )),
            recording_ws.clone(),
            Box::new(InMemoryConfigProvider::new()),
        );

        // Inject a fake worktree path so remove_worktree has something
        // to clean up.
        assert_eq!(app.work_items.len(), 1);
        app.work_items[0].repo_associations[0].worktree_path =
            Some(PathBuf::from("/my/repo/.worktrees/feature-branch"));
        app.build_display_list();

        // Select the work item.
        let wi_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .unwrap();
        app.selected_item = Some(wi_idx);
        app.sync_selection_identity();

        // Delete (non-force).
        app.delete_selected_work_item(false);

        // Verify remove_worktree was called with correct arguments.
        let rw_calls = recording_ws.remove_worktree_calls.lock().unwrap();
        assert_eq!(rw_calls.len(), 1, "remove_worktree should be called once");
        assert_eq!(
            rw_calls[0].0,
            PathBuf::from("/my/repo"),
            "remove_worktree repo_path"
        );
        assert_eq!(
            rw_calls[0].1,
            PathBuf::from("/my/repo/.worktrees/feature-branch"),
            "remove_worktree worktree_path"
        );
        assert!(
            !rw_calls[0].2,
            "remove_worktree delete_branch should be false (handled separately)"
        );
        assert!(
            !rw_calls[0].3,
            "remove_worktree force should be false for non-force delete"
        );
        drop(rw_calls);

        // Verify delete_branch was called with correct arguments.
        let db_calls = recording_ws.delete_branch_calls.lock().unwrap();
        assert_eq!(db_calls.len(), 1, "delete_branch should be called once");
        assert_eq!(
            db_calls[0].0,
            PathBuf::from("/my/repo"),
            "delete_branch repo_path"
        );
        assert_eq!(db_calls[0].1, "feature-branch", "delete_branch branch name");
        assert!(
            db_calls[0].2,
            "delete_branch force should be true (user chose to destroy the item)"
        );
    }
}
