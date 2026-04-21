//! The `App` struct definition, extracted from `src/app/mod.rs`.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;

use super::*;
use crate::agent_backend::AgentBackendKind;
use crate::config::RepoEntry;
use crate::create_dialog::CreateDialog;
use crate::mcp::{McpEvent, McpSocketServer};
use crate::work_item::{
    FetchMessage, FetcherHandle, RepoFetchResult, ReviewRequestedPr, SessionEntry, UnlinkedPr,
    WorkItem, WorkItemId, WorkItemStatus,
};

/// App holds the entire application state.
pub struct App {
    /// Top-level shell chrome: quit flag, focus panel, status bar
    /// message, double-Q confirm, PTY pane dimensions, shutdown
    /// lifecycle. Owned by the `Shell` subsystem so the state
    /// machine lives in one place. See `app::Shell`.
    pub shell: Shell,
    /// True when the delete confirmation modal is visible.
    pub delete_prompt_visible: bool,
    /// Identity of the work item targeted by the open delete modal. Stored
    /// by identity (not display index) so it survives list reassembly.
    pub delete_target_wi_id: Option<WorkItemId>,
    /// Title of the targeted work item, shown in the dialog body.
    pub delete_target_title: Option<String>,
    /// True while the async delete cleanup thread is running on behalf of
    /// the user-initiated (modal) delete path. The dialog stays visible
    /// with a spinner and the event loop swallows all keys except Q/Ctrl+Q.
    pub delete_in_progress: bool,
    /// Warnings collected synchronously during the modal delete's
    /// `delete_work_item_by_id` call (Phase 2 backend pre-delete hook,
    /// Phase 5 inline orphan-worktree cleanup). Stashed here so
    /// `poll_delete_cleanup` can fold them into the final status/alert
    /// message alongside the background thread's warnings - otherwise
    /// they would be silently dropped when `cleanup_infos` is non-empty.
    pub delete_sync_warnings: Vec<String>,
    /// When `Some`, the "Set branch name" recovery modal is visible.
    /// Shown when a Planning/Implementing work item has no branch on any
    /// repo association and would otherwise be stuck (e.g. Enter pressed
    /// on a branchless item, or `advance_stage` called on a branchless
    /// Backlog item). See `docs/UI.md` "Set branch recovery dialog".
    pub set_branch_dialog: Option<crate::create_dialog::SetBranchDialog>,
    /// True when the merge strategy prompt is visible (Review -> Done).
    pub confirm_merge: bool,
    /// The work item ID that the merge prompt applies to.
    pub merge_wi_id: Option<WorkItemId>,
    /// True while the merge background thread is running.
    /// The dialog stays open with a spinner in this state.
    ///
    /// Set the moment `execute_merge` admits the `UserActionKey::PrMerge`
    /// slot, so the modal renders the "Refreshing remote state..."
    /// spinner during the precheck phase as well as the actual
    /// `gh pr merge` phase. Cleared in `poll_pr_merge` /
    /// `poll_merge_precheck` on every terminal arm.
    pub merge_in_progress: bool,
    /// True when the rework reason text input is visible (Review -> Implementing).
    pub rework_prompt_visible: bool,
    /// Text input for the rework reason.
    pub rework_prompt_input: rat_widget::text_input::TextInputState,
    /// The work item ID that the rework prompt applies to.
    pub rework_prompt_wi: Option<WorkItemId>,
    /// Rework reasons keyed by work item ID. Used by `stage_system_prompt`
    /// to select the "`implementing_rework`" prompt template.
    pub rework_reasons: HashMap<WorkItemId, String>,
    /// Review-gate findings keyed by work item ID. Populated when the gate
    /// approves, consumed one-shot by `stage_system_prompt` to select the
    /// "`review_with_findings`" prompt template and inject the assessment.
    pub review_gate_findings: HashMap<WorkItemId, String>,
    /// True when the unlinked-item cleanup confirmation prompt is visible.
    pub cleanup_prompt_visible: bool,
    /// True when the cleanup reason text input is active (user pressed Enter
    /// from the confirmation prompt to type an optional close reason).
    pub cleanup_reason_input_active: bool,
    /// Text input for the optional close reason.
    pub cleanup_reason_input: rat_widget::text_input::TextInputState,
    /// Identity of the unlinked PR being cleaned up: (`repo_path`, branch, `pr_number`).
    /// Stored by identity rather than index so it survives reassembly.
    pub cleanup_unlinked_target: Option<(PathBuf, String, u64)>,
    /// PR number shown in the in-progress dialog body.
    pub cleanup_progress_pr_number: Option<u64>,
    /// Repo path of the in-progress cleanup target (for cache eviction on completion).
    pub cleanup_progress_repo_path: Option<PathBuf>,
    /// Branch name of the in-progress cleanup target (for cache eviction on completion).
    pub cleanup_progress_branch: Option<String>,
    /// Branches whose PRs were recently closed via cleanup. Used to suppress
    /// stale fetch results from re-adding the PR as unlinked. Applied and
    /// cleared when a fresh fetch arrives (`drain_fetch_results` returns true).
    pub cleanup_evicted_branches: Vec<(PathBuf, String)>,
    /// General-purpose alert dialog. When Some, a red-bordered modal is shown.
    /// Dismissed with Enter or Esc.
    pub alert_message: Option<String>,
    /// Branch-gone dialog. Shown when worktree creation fails because the
    /// work item's branch no longer exists. Holds (`work_item_id`, `error_message`).
    /// The user can choose to delete the work item or dismiss.
    pub branch_gone_prompt: Option<(WorkItemId, String)>,
    /// Stale-worktree recovery dialog. Shown when worktree creation fails
    /// because the branch is locked to a stale/corrupt worktree.
    pub stale_worktree_prompt: Option<StaleWorktreePrompt>,
    /// True while the background recovery thread is running (force-remove,
    /// prune, recreate). The dialog switches to a spinner with no key
    /// options so the user cannot interact until recovery completes.
    pub stale_recovery_in_progress: bool,
    /// True when the no-plan prompt is visible (offered when Claude blocks
    /// because no implementation plan exists).
    pub no_plan_prompt_visible: bool,
    /// Queue of work item IDs awaiting no-plan prompt resolution.
    /// When multiple items block with "No implementation plan" concurrently,
    /// all are queued. The front item is shown to the user; resolving it
    /// pops it and shows the next (if any).
    pub no_plan_prompt_queue: VecDeque<WorkItemId>,
    /// App-wide service aggregate: `backend`, `worktree_service`,
    /// `github_client`, `pr_closer`, `agent_backend`, `config`,
    /// `config_provider`. Replaces seven previously sibling fields on
    /// `App` with a single owning struct so every subsystem method
    /// takes `&mut SharedServices` instead of `&mut App` when only
    /// services are needed. See `app::SharedServices`.
    pub services: SharedServices,
    /// Settings overlay subsystem (the `?` key view). Owns the
    /// visible flag, both cursors, the active tab, the Repos-tab
    /// focus column, the keybindings scroll, and the Review-Gate tab
    /// review-skill text input + editing flag. Replaces eight
    /// previously sibling fields on `App`. See
    /// `app::SettingsOverlay`.
    pub settings: SettingsOverlay,
    /// Cached active repo entries (explicit + included). Rebuilt when
    /// inclusions change, not on every frame or keypress.
    ///
    /// Lives on `App`, NOT inside `SettingsOverlay`, because every
    /// reassembly / spawn site / display read consults it. The
    /// settings overlay only renders it.
    pub active_repo_cache: Vec<RepoEntry>,
    /// State for the work item creation modal dialog.
    pub create_dialog: CreateDialog,

    // -- Work item state --
    /// Assembled work items (from backend records + repo data).
    pub work_items: Vec<WorkItem>,
    /// PRs not linked to any work item (only the user's own).
    pub unlinked_prs: Vec<UnlinkedPr>,
    /// PRs where the user has been requested as a reviewer.
    pub review_requested_prs: Vec<ReviewRequestedPr>,
    /// Cached GitHub login of the authenticated user, as reported by
    /// the fetcher thread (which in turn calls `gh api user`). None
    /// until the first successful fetch reports it; subsequent fetch
    /// results never clobber a `Some` value with `None` so transient
    /// `gh api user` failures do not erase a previously known login.
    /// Consumed by review-request row rendering and sorting to decide
    /// which rows are direct-to-you vs. team-only.
    pub current_user_login: Option<String>,
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
    /// Scroll offset for the left-panel work item list. Persisted between
    /// render frames so the viewport stays stable during navigation.
    /// Uses `Cell` for interior mutability since rendering takes `&App`.
    ///
    /// Authoritative for the viewport position: mutated by (a) mouse
    /// wheel events, (b) the keyboard-triggered recenter pass when
    /// `recenter_viewport_on_selection` is set, and (c) a clamp at
    /// render time when the list shrinks beneath the current offset.
    /// See `docs/UI.md` "List/viewport/scrollbar".
    pub list_scroll_offset: Cell<usize>,
    /// When `true`, the next render of the work item list centers the
    /// viewport on the current selection (clamped against the list's
    /// top / bottom). Keyboard navigation (`select_next_item` /
    /// `select_prev_item`) sets this so that selection-moving keys
    /// always snap the viewport back; mouse wheel and click-to-select
    /// deliberately do NOT set it, leaving the viewport where the user
    /// parked it. The flag is consumed (`take()`) by the renderer each
    /// frame, so a single selection change schedules exactly one
    /// recenter.
    pub recenter_viewport_on_selection: Cell<bool>,
    /// Inner body rect (absolute frame coordinates) of the work item
    /// list. Written each render, read by `handle_mouse` so wheel
    /// events and row clicks can be classified without re-doing the
    /// layout math. `None` before the first render.
    pub work_item_list_body: Cell<Option<ratatui_core::layout::Rect>>,
    /// Maximum item-level offset the renderer will accept next frame
    /// (i.e. `display_list.len().saturating_sub(visible_items)`).
    /// Written each render, read by the wheel-scroll handler to clamp
    /// the new offset without having to recompute layout. `0` before
    /// the first render.
    pub list_max_item_offset: Cell<usize>,
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
    /// Rolling time window currently selected in the Dashboard view.
    /// Not persisted to disk; resets to Month on each launch.
    pub dashboard_window: DashboardWindow,
    /// Metrics subsystem: owns the latest aggregated metrics snapshot
    /// plus the receiver that feeds it from the background aggregator
    /// thread. Replaces the two previously sibling fields
    /// (`metrics_snapshot`, `metrics_rx`) with a single owning struct
    /// so the poll / disconnect / snapshot-cache dance lives in one
    /// place. See `app::Metrics`.
    pub metrics: Metrics,
    /// Set when manage/unmanage changes active repos. The main loop checks
    /// this flag and restarts the background fetcher with the updated repo
    /// list so newly managed repos get fetched and removed repos stop.
    pub fetcher_repos_changed: bool,
    /// Tracks the `WorkItemId` of the currently selected work item so that
    /// selection survives reassembly even when display indices change.
    /// After `build_display_list`, the matching entry is found and
    /// `selected_item` is restored.
    pub selected_work_item: Option<WorkItemId>,
    /// Tracks the (`repo_path`, branch) of the currently selected unlinked PR
    /// so that selection survives reassembly even when display indices change.
    /// Keyed by both `repo_path` and branch to disambiguate same-named branches
    /// across different repos.
    pub selected_unlinked_branch: Option<(PathBuf, String)>,
    /// Tracks the selected review-requested PR for selection restoration.
    pub selected_review_request_branch: Option<(PathBuf, String)>,
    /// Fetch errors that could not be shown because the status bar was
    /// occupied. Drained on the next tick when `status_message` is None.
    pub pending_fetch_errors: Vec<String>,
    /// True when the fetcher channel has disconnected unexpectedly (all
    /// sender threads exited). Surfaced in the status bar so the user
    /// knows background updates have stopped.
    pub fetcher_disconnected: bool,
    /// Handle to the background fetcher threads. Used to stop the fetcher
    /// when repos change or when the app shuts down. Managed by the
    /// rat-salsa event callback in salsa.rs.
    pub fetcher_handle: Option<FetcherHandle>,
    /// Per-work-item harness choice. Populated when the user presses
    /// `c` / `x` / `o` on a work-item row (see
    /// `App::open_session_with_harness`); read back by every spawn site
    /// (work-item interactive, review gate, rebase gate). In-memory only:
    /// the choice is deliberately not persisted across TUI restarts - the
    /// acceptance criteria for the multi-harness selection explicitly
    /// call this out. Absence of an entry means "no choice made yet";
    /// spawn sites surface that as a toast rather than silently defaulting.
    pub harness_choice: HashMap<WorkItemId, AgentBackendKind>,
    /// Timestamped single-key state for the `kk` double-press that ends
    /// a live session. `Some((id, when))` means the user pressed `k` on
    /// work item `id` at `when`; a second `k` press on the same item
    /// within ~1.5s kills the session. Any other key or a timeout
    /// clears this (see `App::handle_k_press` and `App::clear_k_press`).
    pub last_k_press: Option<(WorkItemId, std::time::Instant)>,
    /// First-run Ctrl+G modal state. `Some(..)` means the modal is
    /// visible; it lists the harnesses currently on PATH and asks the
    /// user to pick one. The pick persists to
    /// `config.defaults.global_assistant_harness` and then opens the
    /// drawer immediately. `None` is the steady state.
    pub first_run_global_harness_modal: Option<FirstRunGlobalHarnessModal>,
    /// MCP socket servers keyed by work item ID. Each server is created when
    /// an agent session is spawned and handles MCP communication via a Unix
    /// socket. "Agent" is the harness-neutral name; the reference
    /// implementation today is `ClaudeCodeBackend` (see `docs/harness-
    /// contract.md` and `crate::agent_backend`).
    pub mcp_servers: HashMap<WorkItemId, McpSocketServer>,
    /// Work item IDs where the agent has signaled it is actively working
    /// (via `workbridge_set_activity`). Cleared when the session dies.
    pub agent_working: std::collections::HashSet<WorkItemId>,
    /// Side-car file paths written by the agent backend, tracked for cleanup.
    /// Receiver for MCP events from all socket servers.
    pub mcp_rx: Option<crossbeam_channel::Receiver<McpEvent>>,
    /// Sender for MCP events (cloned for each socket server).
    pub mcp_tx: crossbeam_channel::Sender<McpEvent>,
    /// Per-work-item review gate state. Multiple gates can run concurrently.
    pub review_gates: HashMap<WorkItemId, ReviewGateState>,
    /// Per-work-item rebase-onto-main gate state. Owns the streaming
    /// receiver, the status-bar activity, and the base branch name for
    /// each rebase in flight, so `drop_rebase_gate` is the single drop
    /// site for all three. Single-flight at the user-action layer
    /// (`UserActionKey::RebaseOnMain`) means at most one entry exists
    /// at a time today, but the map shape leaves room for a future
    /// per-item key without rewriting the ownership story.
    pub rebase_gates: HashMap<WorkItemId, RebaseGateState>,

    // -- Activity indicator --
    /// Owns the status-bar spinner state, the activity queue, and
    /// the structural-fetch bookkeeping. Replaces the five previous
    /// sibling fields (`activity_counter`, `activities`,
    /// `spinner_tick`, `structural_fetch_activity`,
    /// `pending_fetch_count`) so the "exactly one spinner at a
    /// time" invariant lives inside one owning type.
    pub activities: Activities,

    // -- User action guard (single-flight admission for remote I/O) --
    /// Owns the in-flight slot + debounce timestamps for every action
    /// routed through `App::try_begin_user_action`. See `docs/UI.md`
    /// "User action guard" for the contract. Replaces seven separate
    /// `Option<Receiver>` + sibling `Option<ActivityId>` triplets.
    pub user_actions: UserActionGuard,

    // -- PR creation queue (bespoke, outside the user-action guard) --
    /// Queued work item IDs waiting for PR creation when a creation is
    /// already in-flight. Drained one at a time as each creation completes.
    /// Kept separate from `user_actions` because queueing semantics are
    /// PR-create-specific; the guard itself only models single-flight
    /// admission.
    pub pr_create_pending: VecDeque<WorkItemId>,

    /// Work item IDs whose reviews were just submitted. These are excluded
    /// from the re-open logic in `reassemble_work_items()` because `repo_data`
    /// may still contain stale review-requested entries until the next
    /// GitHub fetch cycle. Cleared when fresh `repo_data` arrives.
    pub review_reopen_suppress: std::collections::HashSet<WorkItemId>,

    // -- Mergequeue polling --
    /// Active mergequeue watches - items waiting for their PR to be merged.
    /// Each watch carries its own cooldown timestamp so polls run
    /// concurrently rather than serially round-robin.
    pub mergequeue_watches: Vec<PrMergeWatch>,
    /// In-flight polls keyed by work item ID. At most one entry per
    /// watched item; the entry owns the receiver and the activity ID so
    /// retreat / delete can drop it cleanly without touching unrelated
    /// items.
    pub mergequeue_polls: HashMap<WorkItemId, PrMergePollState>,
    /// Last poll error per watched work item. Cleared when the next poll
    /// succeeds or when the item retreats from Mergequeue. Shown in the
    /// detail pane so users notice `gh pr view` failures instead of
    /// losing them to a transient `status_message`.
    pub mergequeue_poll_errors: HashMap<WorkItemId, String>,

    // -- ReviewRequest merge polling --
    /// Active watches for `ReviewRequest` work items in Review, waiting for
    /// their PR to be merged externally. Same shape as
    /// `mergequeue_watches`: each entry owns its own cooldown timestamp so
    /// polls run concurrently rather than serially round-robin, and
    /// reassembly rebuilds them idempotently so app restarts never lose a
    /// watch.
    pub review_request_merge_watches: Vec<PrMergeWatch>,
    /// In-flight `ReviewRequest` polls keyed by work item ID. At most one
    /// entry per watched item; the entry owns the receiver and the
    /// activity ID so retreat / delete can drop it cleanly without
    /// touching unrelated items (structural ownership, per CLAUDE.md).
    pub review_request_merge_polls: HashMap<WorkItemId, PrMergePollState>,
    /// Last poll error per watched `ReviewRequest` work item. Cleared when
    /// the next poll succeeds or when the item leaves Review. Surfaced in
    /// the detail pane so `gh pr view` failures persist across ticks
    /// instead of vanishing with the transient `status_message`.
    pub review_request_merge_poll_errors: HashMap<WorkItemId, String>,

    // -- PR identity backfill --
    /// `PrIdentityBackfill` subsystem: owns the startup-only
    /// migration's receiver + status-bar activity as a single pair
    /// (so the two `Option`s can no longer drift out of lockstep).
    /// Replaces `pr_identity_backfill_rx` / `pr_identity_backfill_activity`.
    /// See `app::PrIdentityBackfill`.
    pub pr_identity_backfill: PrIdentityBackfill,

    // -- Async session-open plan read --
    /// Pending background plan reads, keyed by work item ID. Each entry
    /// holds a receiver for a single `SessionOpenPlanResult` plus the
    /// `ActivityId` of the "Opening session..." spinner started in
    /// `begin_session_open` and ended in `poll_session_opens`. Once the
    /// result arrives, `poll_session_opens` ends the activity and
    /// finishes the session open on the main thread. Keyed by work item
    /// (rather than a single Option<Receiver>) so that opens for
    /// different items do not collide: pressing Enter on several items
    /// in quick succession must not cancel each other. `docs/UI.md`
    /// "Blocking I/O Prohibition" requires the backend read to live on
    /// a background thread; this map is how the result flows back.
    pub session_open_rx: HashMap<WorkItemId, SessionOpenPending>,
    /// Phase 2 PTY spawn results. `finish_session_open` hands the
    /// `Session::spawn` call off to a background thread; the result
    /// flows back here and is drained by `poll_session_spawns` on the
    /// next timer tick. Keyed by work item so concurrent spawns for
    /// different items do not collide.
    pub session_spawn_rx: HashMap<WorkItemId, SessionSpawnPending>,

    /// `OrphanCleanup` subsystem: owns the completion-message channel
    /// pair used by `spawn_orphan_worktree_cleanup` background
    /// threads. Replaces the previously sibling
    /// `orphan_cleanup_finished_tx` / `_rx` fields so the channel
    /// pair has one owner and a narrow `drain_pending` interface.
    /// See `app::OrphanCleanup`.
    pub orphan_cleanup: OrphanCleanup,

    /// Global assistant drawer state: open flag, PTY session, MCP
    /// server, config tempfile path, pane geometry, pre-drawer focus,
    /// spawn lifecycle, context dirty flag, and the PTY write buffer.
    /// Replaces eleven previously sibling fields on App with a single
    /// owning struct so drawer open/close/spawn/teardown can be
    /// reasoned about in one place. See `app::GlobalDrawer`.
    pub global_drawer: GlobalDrawer,
    /// Buffered bytes destined for the active (work-item) PTY
    /// session. Key events that forward to the PTY push here instead
    /// of writing immediately. Flushed as a single write on the next
    /// timer tick so the child process receives all characters in one
    /// `read()` - matching how a native terminal delivers drag-and-
    /// drop or fast paste.
    pub pending_active_pty_bytes: Vec<u8>,

    /// Which tab is active in the right panel (Claude Code or Terminal).
    pub right_panel_tab: RightPanelTab,
    /// Terminal shell sessions keyed by work item ID. One terminal per
    /// work item, spawned lazily on first tab switch.
    pub terminal_sessions: HashMap<WorkItemId, SessionEntry>,
    /// Buffered bytes destined for the active terminal PTY session.
    pub pending_terminal_pty_bytes: Vec<u8>,

    /// Per-frame click registry + pending click-to-copy gesture.
    /// Owned by the `ClickTracking` subsystem so the two previously
    /// sibling fields (`click_registry`, `pending_chrome_click`) and
    /// the `fire_chrome_copy` cross-subsystem call now live behind
    /// a single narrow interface. Field-borrow splitting at the
    /// mouse event dispatcher lets `ClickTracking::fire_copy` take
    /// `&mut self` + `&mut Toasts` disjointly.
    pub click_tracking: ClickTracking,

    /// Transient top-right toast notifications. Owned by the `Toasts`
    /// subsystem so the rest of `App` cannot reach the vector directly;
    /// every mutation goes through `self.toasts.push(...)` /
    /// `self.toasts.prune()`.
    pub toasts: Toasts,
}
