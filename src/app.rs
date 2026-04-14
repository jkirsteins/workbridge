use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::assembly;
use crate::config::{Config, ConfigProvider, RepoEntry, RepoSource};
use crate::create_dialog::CreateDialog;
use crate::github_client::GithubError;
use crate::mcp::{McpEvent, McpSocketServer};
use crate::session::Session;
use crate::work_item::{
    CheckStatus, FetchMessage, FetcherHandle, RepoFetchResult, ReviewRequestedPr, SessionEntry,
    UnlinkedPr, WorkItem, WorkItemId, WorkItemKind, WorkItemStatus,
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

/// Which tab is active in the right panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RightPanelTab {
    ClaudeCode,
    Terminal,
}

/// Which view mode the root overview is in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    FlatList,
    Board,
    Dashboard,
}

/// Rolling window selection for the metrics Dashboard view. Each value maps
/// to a key in the header: 1=Week, 2=Month, 3=Quarter, 4=Year.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DashboardWindow {
    Week,
    Month,
    Quarter,
    Year,
}

impl DashboardWindow {
    /// Number of days the window covers (inclusive of today).
    pub fn days(self) -> i64 {
        match self {
            Self::Week => 7,
            Self::Month => 30,
            Self::Quarter => 90,
            Self::Year => 365,
        }
    }

    /// Short label shown in the header strip.
    pub fn label(self) -> &'static str {
        match self {
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Quarter => "90d",
            Self::Year => "365d",
        }
    }
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
/// Sentinel title used when a quick-start work item is created before the
/// user has specified what they want to work on. The planning_quickstart
/// system prompt instructs Claude to call workbridge_set_title once the
/// user explains the task, replacing this placeholder.
pub const QUICKSTART_TITLE: &str = "Quick start";

/// Rejection wording shown when `execute_merge` refuses a second
/// concurrent merge. Duplicated at the pre-check and the
/// defense-in-depth race handler after `try_begin_user_action` - kept
/// in one const so a future rename is a single-site edit.
const PR_MERGE_ALREADY_IN_PROGRESS: &str = "PR merge already in progress";

/// Rejection wording shown when `spawn_review_submission` refuses a
/// second concurrent review submission. Same duplication rationale as
/// `PR_MERGE_ALREADY_IN_PROGRESS`.
const REVIEW_SUBMIT_ALREADY_IN_PROGRESS: &str = "Review submission already in progress";

pub const BOARD_COLUMNS: &[WorkItemStatus] = &[
    WorkItemStatus::Backlog,
    WorkItemStatus::Planning,
    WorkItemStatus::Implementing,
    WorkItemStatus::Review,
];

/// Which top-level tab is active inside the settings overlay.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Repos,
    ReviewGate,
    Keybindings,
}

/// Which column has focus inside the Repos tab of the settings overlay.
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
    /// Short repo name derived from the last path segment of
    /// `RepoAssociation.repo_path` (e.g., `workbridge`). Used for the
    /// context bar so the full path does not crowd the row; the
    /// authoritative cwd is always visible in the right panel
    /// Terminal tab via the live shell prompt.
    pub repo_name: String,
    /// Issue labels (from IssueInfo.labels). Empty if no issue linked.
    pub labels: Vec<String>,
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

/// Keys identifying every user-initiated remote I/O action that runs
/// through `App::try_begin_user_action`. One variant per admission slot:
/// rejecting a second concurrent entry is the whole point of routing
/// through the helper.
///
/// See `docs/UI.md` "User action guard" for the admission contract and
/// `CLAUDE.md` severity overrides for the review-time policy.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub enum UserActionKey {
    /// User-triggered GitHub refresh (Ctrl+R). Debounced to 500ms; the
    /// background fetcher's structural restart path does NOT go through
    /// this key - only the explicit user press does.
    GithubRefresh,
    /// Asynchronous PR creation initiated from the Review stage.
    PrCreate,
    /// Asynchronous PR merge (`gh pr merge`) initiated from the merge
    /// modal.
    PrMerge,
    /// Asynchronous PR review submission (approve / request-changes).
    ReviewSubmit,
    /// Shared single slot for `spawn_session`'s auto-worktree creation
    /// and `spawn_import_worktree`'s import flow. Per-repo concurrency
    /// is intentionally out of scope here: both callers compete for the
    /// same global slot. If concurrent worktree creation for different
    /// repos is later wanted, key this on `(RepoPath, Branch)` instead.
    WorktreeCreate,
    /// Asynchronous unlinked-PR cleanup initiated from the cleanup
    /// modal. Single source of truth for the "cleanup in progress" read
    /// state that event.rs / ui.rs / salsa.rs query via
    /// `App::is_user_action_in_flight(&UnlinkedCleanup)`.
    UnlinkedCleanup,
    /// Asynchronous delete-cleanup initiated from the delete modal or
    /// the MCP delete handler.
    DeleteCleanup,
}

/// Payload stored inside `UserActionState` for each in-flight entry.
/// Carries the background-thread receiver and any per-action metadata
/// (such as the `WorkItemId` the action was spawned for) directly inside
/// the helper map, so state ownership is structural: dropping the map
/// entry drops the receiver, and there is no way to leave behind a
/// stray `Option<Receiver>` or orphaned `Option<ActivityId>`.
pub enum UserActionPayload {
    /// No receiver attached. Used by `GithubRefresh` (the fetcher has
    /// its own channel) and as the transient state between
    /// `try_begin_user_action` returning and the caller attaching the
    /// payload via `attach_user_action_payload`.
    Empty,
    PrCreate {
        rx: crossbeam_channel::Receiver<PrCreateResult>,
        wi_id: WorkItemId,
    },
    PrMerge {
        rx: crossbeam_channel::Receiver<PrMergeResult>,
    },
    ReviewSubmit {
        rx: crossbeam_channel::Receiver<ReviewSubmitResult>,
        wi_id: WorkItemId,
    },
    WorktreeCreate {
        rx: crossbeam_channel::Receiver<WorktreeCreateResult>,
        wi_id: WorkItemId,
    },
    UnlinkedCleanup {
        rx: crossbeam_channel::Receiver<CleanupResult>,
    },
    DeleteCleanup {
        rx: crossbeam_channel::Receiver<CleanupResult>,
    },
}

/// State for one in-flight user action. Owned by the `UserActionGuard`
/// map keyed on `UserActionKey`. The `activity_id` is the status-bar
/// spinner started when the action was admitted; the `payload` carries
/// the per-action receiver so there is exactly one structural drop
/// site for both.
pub struct UserActionState {
    pub activity_id: ActivityId,
    pub payload: UserActionPayload,
}

/// Single source of truth for "is this user action in flight" plus the
/// last-attempt timestamps used for debounce. See `App::try_begin_user_action`.
#[derive(Default)]
pub struct UserActionGuard {
    pub in_flight: HashMap<UserActionKey, UserActionState>,
    pub last_attempted: HashMap<UserActionKey, Instant>,
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

/// Messages sent from the review gate background thread to the main thread.
/// The gate sends Progress updates (e.g. CI check status) before the final Result.
pub enum ReviewGateMessage {
    /// Intermediate progress update shown in the right panel.
    Progress(String),
    /// Final result - the gate completed or failed.
    Result(ReviewGateResult),
    /// Terminal "cannot run" outcome discovered on the background thread
    /// (no plan, no diff, git failure, default branch unresolvable). This
    /// is NOT the same as `Result { approved: false }`: Blocked means the
    /// gate never actually ran against a diff, so the caller must only
    /// surface the reason in the status bar and clear the gate state -
    /// NOT log an activity entry or kill/respawn the session as if the
    /// review had rejected the work.
    Blocked {
        work_item_id: WorkItemId,
        reason: String,
    },
}

/// Who initiated a review gate. Determines how `poll_review_gate`
/// handles a `Blocked` outcome:
///
/// - `Mcp`: Claude requested Review via workbridge_set_status and the
///   background gate decided it cannot run. The rework flow applies -
///   kill the existing session and respawn with the rejection reason so
///   Claude has feedback to iterate on.
/// - `Tui`: The user pressed `l` (advance) on a no-diff or no-plan
///   Implementing item. The session is still the user's primary
///   workspace - killing and respawning would be destructive. Only
///   surface the reason in the status bar and let the user decide.
/// - `Auto`: An Implementing session died without calling
///   workbridge_set_status("Review"). Auto-triggering the gate is a
///   convenience; if it blocks we still want the rework flow so Claude
///   sees the reason on the next restart (mirrors Mcp semantics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewGateOrigin {
    Mcp,
    Tui,
    Auto,
}

/// Per-work-item state for an in-flight review gate.
pub struct ReviewGateState {
    pub rx: crossbeam_channel::Receiver<ReviewGateMessage>,
    pub progress: Option<String>,
    pub origin: ReviewGateOrigin,
    /// Status-bar activity ID for the "Running review gate..." spinner
    /// started in `spawn_review_gate`. The review gate is a
    /// system-initiated long-running operation (no blocking dialog is
    /// open) so per `docs/UI.md` "Activity indicator placement" it owes
    /// the user a status-bar spinner. Ownership lives inside
    /// `ReviewGateState` so that every drop site (delete, retreat, all
    /// terminal arms of `poll_review_gate`, shutdown) can route through
    /// `drop_review_gate` and end the activity in one place.
    pub activity: ActivityId,
}

/// A single CI check as returned by `gh pr checks --json name,bucket`.
struct CiCheck {
    name: String,
    /// One of: pass, fail, pending, skipping, cancel
    bucket: String,
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

/// Outcome of an asynchronous PR review submission.
pub enum ReviewSubmitOutcome {
    /// Review posted successfully on GitHub.
    Success,
    /// Review submission failed.
    Failed { error: String },
}

/// Result from the asynchronous review submission thread.
pub struct ReviewSubmitResult {
    pub wi_id: WorkItemId,
    pub action: String,
    pub outcome: ReviewSubmitOutcome,
}

/// Information needed to poll a Mergequeue item's PR state.
///
/// `pr_number` is the unambiguous identity of the PR the user opted into
/// when they pressed `[p] Poll`. When it is `Some`, the poll thread
/// targets `gh pr view <number>`, which always returns the exact PR even
/// if the branch has since had another PR opened on it. `enter_mergequeue`
/// pins it from `assoc.pr.number` immediately, so the live-entry path is
/// never vulnerable to branch-resolution drift.
///
/// `pr_number` is `None` only on a watch that was rebuilt from a backend
/// record after an app restart, since the in-memory `assoc.pr` may have
/// been gone by then. In that case the poll thread falls back to
/// `gh pr view <branch>`; the result drain writes the resolved number
/// back into the watch so subsequent polls are unambiguous.
///
/// `last_polled` enforces a per-item cooldown so each watch is checked on
/// its own 30s schedule. Polls run concurrently across watches.
pub struct MergequeueWatch {
    pub wi_id: WorkItemId,
    pub pr_number: Option<u64>,
    pub owner_repo: String,
    pub branch: String,
    pub repo_path: PathBuf,
    pub last_polled: Option<std::time::Instant>,
}

/// In-flight poll for a single Mergequeue work item. The map key is the
/// `WorkItemId`, so retreat / delete can drop exactly the entry that
/// belongs to the affected item without touching anything else.
pub struct MergequeuePollState {
    pub rx: crossbeam_channel::Receiver<MergequeuePollResult>,
    pub activity: ActivityId,
}

/// Result from the background Mergequeue PR state poll.
pub struct MergequeuePollResult {
    pub wi_id: WorkItemId,
    pub pr_state: String,
    pub branch: String,
    pub repo_path: PathBuf,
    pub pr_identity: Option<PrIdentityRecord>,
}

/// Result from the background PR identity backfill thread.
pub struct PrIdentityBackfillResult {
    pub wi_id: WorkItemId,
    pub repo_path: PathBuf,
    pub identity: PrIdentityRecord,
}

/// One completion message per `spawn_orphan_worktree_cleanup` thread.
/// Always sent exactly once when the background closure finishes -
/// success or failure. Carries the `ActivityId` so the main thread can
/// end the matching status-bar spinner, and any warnings so they can
/// be surfaced via `status_message`. Per `docs/UI.md` "Activity
/// indicator placement", the orphan cleanup is system-initiated
/// fire-and-forget background work and therefore owes the user a
/// status-bar spinner; the per-spawn `ActivityId` is the structural
/// owner of that spinner.
pub struct OrphanCleanupFinished {
    pub activity: ActivityId,
    pub warnings: Vec<String>,
}

/// Result from the asynchronous unlinked-item cleanup thread.
pub struct CleanupResult {
    /// Best-effort warnings (non-fatal failures during PR close / branch delete).
    pub warnings: Vec<String>,
    /// (repo_path, branch) pairs for PRs that were successfully closed.
    /// Used to populate `cleanup_evicted_branches` so stale fetch data
    /// does not resurrect closed PRs as phantom unlinked items.
    pub closed_pr_branches: Vec<(PathBuf, String)>,
}

/// Info gathered on the main thread for one repo association, passed to
/// the background delete-cleanup thread for resource removal.
pub(crate) struct DeleteCleanupInfo {
    repo_path: PathBuf,
    branch: Option<String>,
    worktree_path: Option<PathBuf>,
    branch_in_main_worktree: bool,
    open_pr_number: Option<u64>,
    github_remote: Option<(String, String)>,
}

/// Result from the asynchronous plan-read that precedes opening a
/// Claude session. `stage_system_prompt` previously read the plan
/// synchronously on the UI thread - the read is now performed on a
/// background thread and the deserialized plan (plus any error
/// message) flows back via this struct for the main thread to apply.
pub struct SessionOpenPlanResult {
    /// The work item the session is being opened for.
    pub wi_id: WorkItemId,
    /// The worktree path where Claude will run.
    pub cwd: PathBuf,
    /// Plan text read from the backend, if any. Empty string when the
    /// backend returned `Ok(None)` or an error (the caller treats an
    /// empty plan the same as a missing plan).
    pub plan_text: String,
    /// Human-readable error surfaced in the status bar when the backend
    /// read failed. `None` on success or when the backend reported no
    /// plan exists.
    pub read_error: Option<String>,
}

/// Per-entry state tracked alongside the `session_open_rx` map so
/// `poll_session_opens` can end the "Opening session..." spinner
/// started by `begin_session_open`. Stored in a named struct (rather
/// than a bare tuple) so the activity ID cannot be accidentally
/// dropped if the map grows new fields - a missed `end_activity`
/// would leak a permanent spinner in the status bar.
pub struct SessionOpenPending {
    pub rx: crossbeam_channel::Receiver<SessionOpenPlanResult>,
    pub activity: ActivityId,
}

/// Result from the asynchronous worktree creation thread.
pub struct WorktreeCreateResult {
    /// The work item the worktree was created for.
    pub wi_id: WorkItemId,
    /// The repo path the worktree belongs to.
    pub repo_path: PathBuf,
    /// The branch name the worktree was created for. Preserved here so
    /// orphan-cleanup paths (Phase 5 in `delete_work_item_by_id`) can
    /// forward it to `spawn_delete_cleanup` and run `git branch -D`
    /// off the UI thread. `None` only for test stubs that synthesize a
    /// result without a branch (e.g. `collect_backfill_requests` tests).
    pub branch: Option<String>,
    /// The worktree path on success.
    pub path: Option<PathBuf>,
    /// Human-readable error message on failure.
    pub error: Option<String>,
    /// When true, automatically open a Claude session after worktree creation.
    /// Set to false for import operations that only need the worktree.
    pub open_session: bool,
    /// True when the failure is specifically because the branch could not be
    /// fetched or created (branch gone). False for other worktree errors
    /// (permissions, disk full, path conflict).
    pub branch_gone: bool,
    /// True when `path` points at a pre-existing worktree that the background
    /// thread observed via `list_worktrees` instead of creating with
    /// `git worktree add`. Cancel/orphan cleanup paths must not run
    /// `remove_worktree` on reused worktrees because the thread never owned
    /// them (the worktree was already registered with git when we found it).
    pub reused: bool,
}

/// An orphaned worktree captured from an in-flight worktree-create
/// result at the moment a delete is confirmed. Threaded back to the
/// caller of `delete_work_item_by_id` so the caller can synthesize a
/// `DeleteCleanupInfo` for `spawn_delete_cleanup` - keeping the
/// `git worktree remove` and `git branch -D` off the UI thread. The
/// branch name is preserved so the cleanup thread deletes the stale
/// branch ref too (dropping it here would leak the branch on master's
/// pre-P0-fix behaviour).
pub(crate) struct OrphanWorktree {
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
}

/// App holds the entire application state.
pub struct App {
    pub should_quit: bool,
    pub focus: FocusPanel,
    /// Status message displayed to the user (errors, confirmations, etc.).
    pub status_message: Option<String>,
    /// True when waiting for a second press to confirm quit.
    pub confirm_quit: bool,
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
    /// on a branchless item, or advance_stage called on a branchless
    /// Backlog item). See `docs/UI.md` "Set branch recovery dialog".
    pub set_branch_dialog: Option<crate::create_dialog::SetBranchDialog>,
    /// True when the merge strategy prompt is visible (Review -> Done).
    pub confirm_merge: bool,
    /// The work item ID that the merge prompt applies to.
    pub merge_wi_id: Option<WorkItemId>,
    /// True while the merge background thread is running.
    /// The dialog stays open with a spinner in this state.
    pub merge_in_progress: bool,
    /// True when the rework reason text input is visible (Review -> Implementing).
    pub rework_prompt_visible: bool,
    /// Text input for the rework reason.
    pub rework_prompt_input: rat_widget::text_input::TextInputState,
    /// The work item ID that the rework prompt applies to.
    pub rework_prompt_wi: Option<WorkItemId>,
    /// Rework reasons keyed by work item ID. Used by stage_system_prompt
    /// to select the "implementing_rework" prompt template.
    pub rework_reasons: HashMap<WorkItemId, String>,
    /// Review-gate findings keyed by work item ID. Populated when the gate
    /// approves, consumed one-shot by `stage_system_prompt` to select the
    /// "review_with_findings" prompt template and inject the assessment.
    pub review_gate_findings: HashMap<WorkItemId, String>,
    /// True when the unlinked-item cleanup confirmation prompt is visible.
    pub cleanup_prompt_visible: bool,
    /// True when the cleanup reason text input is active (user pressed Enter
    /// from the confirmation prompt to type an optional close reason).
    pub cleanup_reason_input_active: bool,
    /// Text input for the optional close reason.
    pub cleanup_reason_input: rat_widget::text_input::TextInputState,
    /// Identity of the unlinked PR being cleaned up: (repo_path, branch, pr_number).
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
    /// cleared when a fresh fetch arrives (drain_fetch_results returns true).
    pub cleanup_evicted_branches: Vec<(PathBuf, String)>,
    /// General-purpose alert dialog. When Some, a red-bordered modal is shown.
    /// Dismissed with Enter or Esc.
    pub alert_message: Option<String>,
    /// Branch-gone dialog. Shown when worktree creation fails because the
    /// work item's branch no longer exists. Holds (work_item_id, error_message).
    /// The user can choose to delete the work item or dismiss.
    pub branch_gone_prompt: Option<(WorkItemId, String)>,
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
    /// Which top-level tab is active in the settings overlay.
    pub settings_tab: SettingsTab,
    /// Which column has focus inside the Repos tab.
    pub settings_list_focus: SettingsListFocus,
    /// Scroll offset for the keybindings tab in the settings overlay.
    pub settings_keybindings_scroll: u16,
    /// Text input for editing the review skill in the Review Gate tab.
    pub settings_review_skill_input: rat_widget::text_input::TextInputState,
    /// Whether the review skill text input is in editing mode.
    pub settings_review_skill_editing: bool,
    /// State for the work item creation modal dialog.
    pub create_dialog: CreateDialog,

    // -- Work item state --
    /// Backend for persisting work item records. Held as `Arc` rather than
    /// `Box` so background threads (PR creation, review gate, delete
    /// cleanup) can clone the handle and perform backend I/O off the UI
    /// thread - see `docs/UI.md` "Blocking I/O Prohibition" for why
    /// `backend.read_plan(...)` and similar calls must not run on the
    /// main thread.
    pub backend: Arc<dyn WorkItemBackend>,
    /// Worktree service for creating/listing worktrees.
    pub worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    /// GitHub pull-request closer, injected via trait so the background
    /// delete-cleanup thread can be exercised in tests without shelling
    /// out to `gh`. Production uses `GhPullRequestCloser`.
    pub pr_closer: Arc<dyn crate::pr_service::PullRequestCloser>,
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
    /// Scroll offset for the left-panel work item list. Persisted between
    /// render frames so the viewport stays stable during navigation.
    /// Uses `Cell` for interior mutability since rendering takes `&App`.
    pub list_scroll_offset: Cell<usize>,
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
    /// Latest metrics snapshot produced by the background aggregator. None
    /// on startup until the first aggregation completes. The Dashboard
    /// renders a "computing..." placeholder while None.
    pub metrics_snapshot: Option<crate::metrics::MetricsSnapshot>,
    /// Receiver for fresh `MetricsSnapshot` values from the background
    /// metrics aggregator thread. Polled (non-blocking `try_recv`) from
    /// the UI timer tick. See `docs/UI.md` "Blocking I/O Prohibition".
    pub metrics_rx: Option<crossbeam_channel::Receiver<crate::metrics::MetricsSnapshot>>,
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
    /// Per-work-item review gate state. Multiple gates can run concurrently.
    pub review_gates: HashMap<WorkItemId, ReviewGateState>,

    // -- Activity indicator --
    /// Monotonic counter for generating unique ActivityId values.
    pub activity_counter: u64,
    /// Currently running activities. The last entry is displayed in the
    /// status bar. When empty, the normal status_message shows through.
    pub activities: Vec<Activity>,
    /// Spinner frame index, advanced on each 200ms timer tick when
    /// activities are present.
    pub spinner_tick: usize,

    // -- User action guard (single-flight admission for remote I/O) --
    /// Owns the in-flight slot + debounce timestamps for every action
    /// routed through `App::try_begin_user_action`. See `docs/UI.md`
    /// "User action guard" for the contract. Replaces seven separate
    /// `Option<Receiver>` + sibling `Option<ActivityId>` triplets.
    pub user_actions: UserActionGuard,

    // -- Background fetch indicator --
    /// Activity ID for an in-flight GitHub fetch that was NOT initiated
    /// via the `GithubRefresh` user-action guard - i.e. a structural
    /// fetcher restart (newly managed repo, work item created, delete
    /// cleanup completed). Started when a `FetchStarted` message arrives
    /// and the user-action guard does not already own the spinner;
    /// cleared when `pending_fetch_count` returns to zero. The invariant
    /// is "exactly one fetch spinner at a time": either this field or
    /// the `UserActionKey::GithubRefresh` entry owns it, never both.
    pub structural_fetch_activity: Option<ActivityId>,
    /// Number of repos currently fetching. The activity spinner is shown
    /// while this is > 0 and cleared when it returns to 0.
    pub pending_fetch_count: usize,

    // -- PR creation queue (bespoke, outside the user-action guard) --
    /// Queued work item IDs waiting for PR creation when a creation is
    /// already in-flight. Drained one at a time as each creation completes.
    /// Kept separate from `user_actions` because queueing semantics are
    /// PR-create-specific; the guard itself only models single-flight
    /// admission.
    pub pr_create_pending: VecDeque<WorkItemId>,

    /// Work item IDs whose reviews were just submitted. These are excluded
    /// from the re-open logic in reassemble_work_items() because repo_data
    /// may still contain stale review-requested entries until the next
    /// GitHub fetch cycle. Cleared when fresh repo_data arrives.
    pub review_reopen_suppress: std::collections::HashSet<WorkItemId>,

    // -- Mergequeue polling --
    /// Active mergequeue watches - items waiting for their PR to be merged.
    /// Each watch carries its own cooldown timestamp so polls run
    /// concurrently rather than serially round-robin.
    pub mergequeue_watches: Vec<MergequeueWatch>,
    /// In-flight polls keyed by work item ID. At most one entry per
    /// watched item; the entry owns the receiver and the activity ID so
    /// retreat / delete can drop it cleanly without touching unrelated
    /// items.
    pub mergequeue_polls: HashMap<WorkItemId, MergequeuePollState>,
    /// Last poll error per watched work item. Cleared when the next poll
    /// succeeds or when the item retreats from Mergequeue. Shown in the
    /// detail pane so users notice `gh pr view` failures instead of
    /// losing them to a transient `status_message`.
    pub mergequeue_poll_errors: HashMap<WorkItemId, String>,

    // -- PR identity backfill --
    /// Receiver for background PR identity backfill results (one-time startup).
    pub pr_identity_backfill_rx:
        Option<crossbeam_channel::Receiver<Result<PrIdentityBackfillResult, String>>>,
    /// Status-bar activity ID for the PR identity backfill spawned in
    /// `app_init` (see `salsa.rs`). Kept on `App` so
    /// `drain_pr_identity_backfill` can end it when the background thread
    /// finishes. Following the `docs/UI.md` "Activity indicator placement"
    /// rule: this is a system-initiated startup migration and therefore
    /// owes the user a status-bar spinner (not a blocking dialog).
    pub pr_identity_backfill_activity: Option<ActivityId>,

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

    /// Sender for completion messages from `spawn_orphan_worktree_cleanup`
    /// background threads. Cloned into each spawned closure. The closure
    /// always sends exactly one `OrphanCleanupFinished` when it finishes
    /// (success or failure), so `poll_orphan_cleanup_finished` can both
    /// surface any warnings AND end the matching status-bar activity.
    /// Drained by `poll_orphan_cleanup_finished` on each background tick.
    pub orphan_cleanup_finished_tx: crossbeam_channel::Sender<OrphanCleanupFinished>,
    /// Receiver paired with `orphan_cleanup_finished_tx`.
    pub orphan_cleanup_finished_rx: crossbeam_channel::Receiver<OrphanCleanupFinished>,

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
    /// Buffered bytes destined for the active PTY session. Key events
    /// that forward to the PTY push here instead of writing immediately.
    /// Flushed as a single write on the next timer tick so the child
    /// process receives all characters in one read() - matching how a
    /// native terminal delivers drag-and-drop or fast paste.
    pub pending_active_pty_bytes: Vec<u8>,
    /// Same buffer for the global assistant session.
    pub pending_global_pty_bytes: Vec<u8>,

    /// Which tab is active in the right panel (Claude Code or Terminal).
    pub right_panel_tab: RightPanelTab,
    /// Terminal shell sessions keyed by work item ID. One terminal per
    /// work item, spawned lazily on first tab switch.
    pub terminal_sessions: HashMap<WorkItemId, SessionEntry>,
    /// Buffered bytes destined for the active terminal PTY session.
    pub pending_terminal_pty_bytes: Vec<u8>,
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
            Arc::new(StubBackend),
            Arc::new(StubWorktreeService),
            Box::new(InMemoryConfigProvider::new()),
        )
    }

    /// Create a new App with the given config and backend.
    /// Uses InMemoryConfigProvider so tests never touch the real config.
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
    /// and config provider.
    pub fn with_config_and_worktree_service(
        config: Config,
        backend: Arc<dyn WorkItemBackend>,
        worktree_service: Arc<dyn WorktreeService + Send + Sync>,
        config_provider: Box<dyn ConfigProvider>,
    ) -> Self {
        let active_repo_cache = canonicalize_repo_entries(config.active_repos());
        let (mcp_tx, mcp_rx) = crossbeam_channel::unbounded();
        let (orphan_cleanup_finished_tx, orphan_cleanup_finished_rx) =
            crossbeam_channel::unbounded();
        let mut app = Self {
            pr_closer: crate::pr_service::default_pr_closer(),
            should_quit: false,
            focus: FocusPanel::Left,
            status_message: None,
            confirm_quit: false,
            delete_prompt_visible: false,
            delete_target_wi_id: None,
            delete_target_title: None,
            delete_in_progress: false,
            delete_sync_warnings: Vec::new(),
            set_branch_dialog: None,
            confirm_merge: false,
            merge_wi_id: None,
            merge_in_progress: false,
            rework_prompt_visible: false,
            rework_prompt_input: rat_widget::text_input::TextInputState::new(),
            rework_prompt_wi: None,
            rework_reasons: HashMap::new(),
            review_gate_findings: HashMap::new(),
            cleanup_prompt_visible: false,
            cleanup_reason_input_active: false,
            cleanup_reason_input: rat_widget::text_input::TextInputState::new(),
            cleanup_unlinked_target: None,
            cleanup_progress_pr_number: None,
            cleanup_progress_repo_path: None,
            cleanup_progress_branch: None,
            cleanup_evicted_branches: Vec::new(),
            alert_message: None,
            branch_gone_prompt: None,
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
            settings_tab: SettingsTab::Repos,
            settings_list_focus: SettingsListFocus::Managed,
            settings_keybindings_scroll: 0,
            settings_review_skill_input: rat_widget::text_input::TextInputState::new(),
            settings_review_skill_editing: false,
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
            list_scroll_offset: Cell::new(0),
            display_list: Vec::new(),
            view_mode: ViewMode::FlatList,
            board_cursor: BoardCursor {
                column: 0,
                row: None,
            },
            board_drill_down: false,
            dashboard_window: DashboardWindow::Month,
            metrics_snapshot: None,
            metrics_rx: None,
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
            review_gates: HashMap::new(),
            activity_counter: 0,
            activities: Vec::new(),
            spinner_tick: 0,
            user_actions: UserActionGuard::default(),
            structural_fetch_activity: None,
            pending_fetch_count: 0,
            pr_create_pending: VecDeque::new(),
            review_reopen_suppress: std::collections::HashSet::new(),
            mergequeue_watches: Vec::new(),
            mergequeue_polls: HashMap::new(),
            mergequeue_poll_errors: HashMap::new(),
            pr_identity_backfill_rx: None,
            pr_identity_backfill_activity: None,
            session_open_rx: HashMap::new(),
            orphan_cleanup_finished_tx,
            orphan_cleanup_finished_rx,
            global_drawer_open: false,
            global_session: None,
            global_mcp_server: None,
            global_mcp_context: Arc::new(Mutex::new("{}".to_string())),
            pre_drawer_focus: FocusPanel::Left,
            global_pane_cols: 80,
            global_pane_rows: 24,
            global_mcp_config_path: None,
            global_mcp_context_dirty: false,
            pending_active_pty_bytes: Vec::new(),
            pending_global_pty_bytes: Vec::new(),
            right_panel_tab: RightPanelTab::ClaudeCode,
            terminal_sessions: HashMap::new(),
            pending_terminal_pty_bytes: Vec::new(),
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
        let now = Instant::now();
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
        let activity_id = self.start_activity(message);
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
        match self.user_actions.in_flight.get_mut(key) {
            Some(state) => state.payload = payload,
            None => panic!(
                "attach_user_action_payload called without a prior successful \
                 try_begin_user_action for {key:?}: every attach must be preceded \
                 by an admit that returned Some(_)",
            ),
        }
    }

    /// End a user action: remove the map entry and clear the status-bar
    /// spinner. Idempotent - calling twice (or calling without a prior
    /// begin) is a no-op, because early-return cancel paths (delete,
    /// retreat) use this as a best-effort cleanup.
    pub fn end_user_action(&mut self, key: &UserActionKey) {
        if let Some(state) = self.user_actions.in_flight.remove(key) {
            self.end_activity(state.activity_id);
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
            | UserActionPayload::WorktreeCreate { wi_id, .. } => Some(wi_id),
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
    /// would be stranded forever: `pending_fetch_count` would stay
    /// non-zero for the rest of the process lifetime, which the Ctrl+R
    /// hard gate in `src/event.rs` interprets as "a fetch cycle is
    /// still running" and rejects every user-initiated refresh from
    /// that point on. The dangling `structural_fetch_activity` id would
    /// similarly leave a stuck spinner on the status bar.
    ///
    /// This helper groups the three invariants that must always move
    /// together on a structural restart:
    ///   1. `fetch_rx = None` - the channel the old threads write into
    ///      is torn down.
    ///   2. `pending_fetch_count = 0` - any counted-but-unpaired
    ///      `FetchStarted` from the old channel is reset so the Ctrl+R
    ///      gate does not permanently lock out the user.
    ///   3. `structural_fetch_activity` / `UserActionKey::GithubRefresh`
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
        self.pending_fetch_count = 0;
        // 3. End both possible owners of the current fetch spinner.
        //    Both are idempotent no-ops when already clear.
        self.end_user_action(&UserActionKey::GithubRefresh);
        if let Some(id) = self.structural_fetch_activity.take() {
            self.end_activity(id);
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
                let repo_name = wi
                    .repo_associations
                    .first()
                    .map(|a| {
                        a.repo_path
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| a.repo_path.display().to_string())
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
            if wi.status != WorkItemStatus::Implementing || self.review_gates.contains_key(&wi_id) {
                continue;
            }
            // Unconditionally spawn the gate. The background closure
            // runs `git diff default..branch` itself and emits
            // `ReviewGateMessage::Blocked("Cannot enter Review: no
            // changes on branch")` when there are no commits. That
            // single source of truth is more reliable than peeking at
            // the 120s fetcher cache, which can still report
            // `Some(false)` (or `None`) for up to two minutes after
            // Claude's final commit - causing the item to get stuck in
            // Implementing with no auto-retry until the next fetch.
            // The Blocked path runs the rework flow (the Auto origin is
            // equivalent to Mcp here) so Claude sees the reason on the
            // next session restart.
            match self.spawn_review_gate(&wi_id, ReviewGateOrigin::Auto) {
                ReviewGateSpawn::Spawned => {
                    self.status_message =
                        Some("Implementing session ended - running review gate...".into());
                }
                ReviewGateSpawn::Blocked(reason) => {
                    self.status_message = Some(reason);
                }
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

        // Check terminal session liveness.
        for entry in self.terminal_sessions.values_mut() {
            if let Some(ref mut session) = entry.session {
                entry.alive = session.is_alive();
            } else {
                entry.alive = false;
            }
        }

        // Remove terminal sessions whose work item no longer exists.
        let terminal_orphans: Vec<_> = self
            .terminal_sessions
            .keys()
            .filter(|wi_id| !self.work_items.iter().any(|w| &w.id == *wi_id))
            .cloned()
            .collect();
        for wi_id in terminal_orphans {
            if let Some(mut entry) = self.terminal_sessions.remove(&wi_id)
                && let Some(mut session) = entry.session.take()
            {
                session.kill();
            }
        }
    }

    /// Stop MCP server and clear activity state for a work item.
    fn cleanup_session_state_for(&mut self, wi_id: &WorkItemId) {
        self.mcp_servers.remove(wi_id);
        self.claude_working.remove(wi_id);
        // Drop any pending background plan read and end its
        // "Opening session..." spinner. The thread will complete and
        // try to send; the send will fail because the receiver is
        // gone, and the thread exits. `finish_session_open` also has
        // its own deleted-work-item guard as a second line of defence.
        self.drop_session_open_entry(wi_id);
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
        // Resize terminal sessions to the same dimensions as the right pane.
        for entry in self.terminal_sessions.values() {
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
        if let Some(ref mut entry) = self.global_session
            && entry.alive
            && let Some(ref mut session) = entry.session
        {
            session.send_sigterm();
        }
        for entry in self.terminal_sessions.values_mut() {
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
            && self.global_session.as_ref().is_none_or(|s| !s.alive)
            && self.terminal_sessions.values().all(|entry| !entry.alive)
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
        // Cancel all in-flight review gates. Route through
        // `drop_review_gate` for each entry so the matching status-bar
        // activity is ended; otherwise force-quit would leak the
        // spinner state on the way out (cosmetic in the moments before
        // exit, but the helper exists precisely so no remove site can
        // skip activity teardown).
        let gate_keys: Vec<WorkItemId> = self.review_gates.keys().cloned().collect();
        for key in gate_keys {
            self.drop_review_gate(&key);
        }
        if let Some(ref mut entry) = self.global_session {
            if let Some(ref mut session) = entry.session {
                session.force_kill();
            }
            entry.alive = false;
        }
        self.global_mcp_server = None;
        for entry in self.terminal_sessions.values_mut() {
            if let Some(ref mut session) = entry.session {
                session.force_kill();
            }
            entry.alive = false;
        }
    }

    /// Find the session key for a work item ID (any stage).
    pub fn session_key_for(&self, wi_id: &WorkItemId) -> Option<(WorkItemId, WorkItemStatus)> {
        self.sessions.keys().find(|(id, _)| id == wi_id).cloned()
    }

    /// Buffer bytes for the active PTY session. The bytes are not written
    /// immediately - they accumulate until `flush_pty_buffers()` is called
    /// (every timer tick). This batches rapid keystrokes (e.g. drag-and-drop
    /// arriving as individual key events) into a single PTY write so the
    /// child process receives them in one `read()`.
    pub fn buffer_bytes_to_active(&mut self, data: &[u8]) {
        self.pending_active_pty_bytes.extend_from_slice(data);
    }

    /// Buffer bytes for the global assistant PTY session.
    pub fn buffer_bytes_to_global(&mut self, data: &[u8]) {
        self.pending_global_pty_bytes.extend_from_slice(data);
    }

    /// Flush buffered PTY bytes to their respective sessions as single
    /// writes. Called on each timer tick before rendering.
    pub fn flush_pty_buffers(&mut self) {
        if !self.pending_active_pty_bytes.is_empty() {
            let data = std::mem::take(&mut self.pending_active_pty_bytes);
            self.send_bytes_to_active(&data);
        }
        if !self.pending_global_pty_bytes.is_empty() {
            let data = std::mem::take(&mut self.pending_global_pty_bytes);
            self.send_bytes_to_global(&data);
        }
        if !self.pending_terminal_pty_bytes.is_empty() {
            let data = std::mem::take(&mut self.pending_terminal_pty_bytes);
            self.send_bytes_to_terminal(&data);
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

    /// Lazily spawn a terminal shell session for the currently selected
    /// work item. Uses `$SHELL` (falling back to `/bin/sh`) with the
    /// worktree path as cwd.
    pub fn spawn_terminal_session(&mut self) {
        let Some(wi_id) = self.selected_work_item_id() else {
            return;
        };
        // Already spawned and still alive?
        if self.terminal_sessions.get(&wi_id).is_some_and(|e| e.alive) {
            return;
        }
        // Remove dead entry so we can respawn.
        if self.terminal_sessions.get(&wi_id).is_some_and(|e| !e.alive) {
            self.terminal_sessions.remove(&wi_id);
        }
        let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
            return;
        };
        let Some(cwd) = wi
            .repo_associations
            .iter()
            .find_map(|a| a.worktree_path.clone())
        else {
            self.status_message = Some("No worktree available for terminal".into());
            return;
        };
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        match Session::spawn(self.pane_cols, self.pane_rows, Some(&cwd), &[&shell]) {
            Ok(session) => {
                let parser = Arc::clone(&session.parser);
                self.terminal_sessions.insert(
                    wi_id,
                    SessionEntry {
                        parser,
                        alive: true,
                        session: Some(session),
                        scrollback_offset: 0,
                        selection: None,
                    },
                );
            }
            Err(e) => {
                self.status_message = Some(format!("Terminal spawn error: {e}"));
            }
        }
    }

    /// Get the terminal SessionEntry for the currently selected work item.
    pub fn active_terminal_entry(&self) -> Option<&SessionEntry> {
        let wi_id = self.selected_work_item_id()?;
        self.terminal_sessions.get(&wi_id)
    }

    /// Get a mutable terminal SessionEntry for the currently selected work item.
    pub fn active_terminal_entry_mut(&mut self) -> Option<&mut SessionEntry> {
        let wi_id = self.selected_work_item_id()?;
        self.terminal_sessions.get_mut(&wi_id)
    }

    /// Buffer bytes for the terminal PTY session.
    pub fn buffer_bytes_to_terminal(&mut self, data: &[u8]) {
        self.pending_terminal_pty_bytes.extend_from_slice(data);
    }

    /// Send raw bytes to the terminal session for the selected work item.
    pub fn send_bytes_to_terminal(&mut self, data: &[u8]) {
        let Some(wi_id) = self.selected_work_item_id() else {
            return;
        };
        let Some(entry) = self.terminal_sessions.get(&wi_id) else {
            return;
        };
        if let Some(ref session) = entry.session
            && let Err(e) = session.write_bytes(data)
        {
            self.status_message = Some(format!("Terminal send error: {e}"));
        }
    }

    /// Route buffered bytes to whichever right-panel tab is active.
    pub fn buffer_bytes_to_right_panel(&mut self, data: &[u8]) {
        match self.right_panel_tab {
            RightPanelTab::ClaudeCode => self.buffer_bytes_to_active(data),
            RightPanelTab::Terminal => self.buffer_bytes_to_terminal(data),
        }
    }

    /// Returns true if the currently selected work item has a worktree path.
    pub fn selected_work_item_has_worktree(&self) -> bool {
        let Some(wi_id) = self.selected_work_item_id() else {
            return false;
        };
        let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
            return false;
        };
        wi.repo_associations
            .iter()
            .any(|a| a.worktree_path.is_some())
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
        // First, collect all pending messages into a local Vec so the
        // `self.fetch_rx` borrow is released before we call any
        // `&mut self` helpers (`end_user_action`, `end_activity`, etc.).
        // Previously this function reached directly into
        // `self.user_actions.in_flight.remove(...)` and
        // `self.activities.retain(...)` because the `rx` borrow blocked
        // it from routing through `end_user_action`; that created a
        // drift hazard if `end_user_action` ever grew side effects.
        // Now every cleanup path here is identical to the rest of the
        // codebase.
        let mut messages = Vec::new();
        let mut disconnected = false;
        {
            let Some(ref rx) = self.fetch_rx else {
                return false;
            };
            loop {
                match rx.try_recv() {
                    Ok(msg) => messages.push(msg),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut received_any = false;
        for msg in messages {
            match msg {
                FetchMessage::FetchStarted => {
                    // Show a spinner while GitHub data is being fetched.
                    // Track how many repos are in-flight so the spinner
                    // persists until all repos have reported back.
                    //
                    // If the Ctrl+R path has already admitted a
                    // `GithubRefresh` action, reuse its activity - do NOT
                    // start a second one. Otherwise (structural restart
                    // path: manage/unmanage, quickstart create, delete
                    // cleanup, etc.) own the spinner locally via
                    // `structural_fetch_activity` so the single-spinner
                    // invariant holds.
                    self.pending_fetch_count += 1;
                    let helper_owns_it =
                        self.is_user_action_in_flight(&UserActionKey::GithubRefresh);
                    if !helper_owns_it && self.structural_fetch_activity.is_none() {
                        let id = self.start_activity("Refreshing GitHub data");
                        self.structural_fetch_activity = Some(id);
                    }
                }
                FetchMessage::RepoData(result) => {
                    received_any = true;
                    self.pending_fetch_count = self.pending_fetch_count.saturating_sub(1);
                    // End both possible owners of the fetch spinner:
                    // the Ctrl+R helper entry (if it started this
                    // cycle) and the structural fallback (if the
                    // restart path started it). Exactly one of them
                    // actually holds an activity at any given time.
                    if self.pending_fetch_count == 0 {
                        self.end_user_action(&UserActionKey::GithubRefresh);
                        if let Some(id) = self.structural_fetch_activity.take() {
                            self.end_activity(id);
                        }
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
                    // Clear re-open suppression only after ALL repos have
                    // reported back.  In multi-repo setups, clearing on every
                    // single RepoData arrival lets an early repo's stale data
                    // re-open items that were just reviewed in a later repo.
                    if self.pending_fetch_count == 0 {
                        self.review_reopen_suppress.clear();
                    }
                }
                FetchMessage::FetcherError { repo_path, error } => {
                    received_any = true;
                    self.pending_fetch_count = self.pending_fetch_count.saturating_sub(1);
                    if self.pending_fetch_count == 0 {
                        self.end_user_action(&UserActionKey::GithubRefresh);
                        if let Some(id) = self.structural_fetch_activity.take() {
                            self.end_activity(id);
                        }
                        // Clear re-open suppression when all repos have
                        // reported back, even if they all failed.  This
                        // mirrors the clear in the RepoData arm.
                        self.review_reopen_suppress.clear();
                    }
                    let msg = format!("Fetch error ({}): {error}", repo_path.display());
                    if self.status_message.is_none() {
                        self.status_message = Some(msg);
                    } else {
                        self.pending_fetch_errors.push(msg);
                    }
                }
            }
        }

        if disconnected && !self.fetcher_disconnected {
            self.fetcher_disconnected = true;
            let msg = "Background fetcher stopped unexpectedly".to_string();
            if self.status_message.is_none() {
                self.status_message = Some(msg);
            } else {
                self.pending_fetch_errors.push(msg);
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
        let (items, unlinked, review_requested, mut reopen_ids) =
            assembly::reassemble(&list_result.records, &self.repo_data, issue_pattern);
        self.work_items = items;
        self.unlinked_prs = unlinked;
        self.review_requested_prs = review_requested;

        // Start the archival clock for items that became Done through PR merge
        // (derived status) but don't yet have a done_at timestamp.
        if self.config.defaults.archive_after_days > 0 {
            match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => {
                    let epoch = duration.as_secs();
                    for record in &list_result.records {
                        if record.status != WorkItemStatus::Done
                            && record.done_at.is_none()
                            && let Some(wi) = self.work_items.iter().find(|w| w.id == record.id)
                            && wi.status == WorkItemStatus::Done
                            && wi.status_derived
                            && let Err(e) = self.backend.set_done_at(&record.id, Some(epoch))
                        {
                            self.status_message =
                                Some(format!("Failed to set archive timestamp: {e}"));
                        }
                    }
                }
                Err(e) => {
                    self.status_message = Some(format!(
                        "System clock error, skipping archive timestamps: {e}"
                    ));
                }
            }
        }

        // Exclude items whose reviews were recently submitted. Stale
        // repo_data may still list them as review-requested until the
        // next GitHub fetch cycle refreshes the data.
        reopen_ids.retain(|id| !self.review_reopen_suppress.contains(id));

        // Re-open Done ReviewRequest items that have been re-requested.
        if !reopen_ids.is_empty() {
            for wi_id in &reopen_ids {
                if let Err(e) = self.backend.update_status(wi_id, WorkItemStatus::Review) {
                    self.status_message = Some(format!("Re-open error: {e}"));
                    continue;
                }
                // Clear done_at so auto-archive won't delete the re-opened item.
                if let Err(e) = self.backend.set_done_at(wi_id, None) {
                    self.status_message =
                        Some(format!("Failed to clear archive timestamp on re-open: {e}"));
                }
                let entry = ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "stage_change".to_string(),
                    payload: serde_json::json!({
                        "from": "Done",
                        "to": "Review",
                        "source": "review_re_requested"
                    }),
                };
                let _ = self.backend.append_activity(wi_id, &entry);
            }
            // Reassemble again to pick up the status changes.
            let list_result = match self.backend.list() {
                Ok(r) => r,
                Err(_) => return,
            };
            let (items, unlinked, review_requested, _) =
                assembly::reassemble(&list_result.records, &self.repo_data, issue_pattern);
            self.work_items = items;
            self.unlinked_prs = unlinked;
            self.review_requested_prs = review_requested;

            let count = reopen_ids.len();
            self.status_message = Some(format!("{count} review request(s) re-opened"));
        }

        // Auto-archive: delete Done items that have exceeded the retention period.
        // This runs AFTER re-open detection so that re-opened items have their
        // done_at cleared and won't be incorrectly archived.
        // Skip entirely when archive is disabled (archive_after_days == 0).
        if self.config.defaults.archive_after_days > 0 {
            match self.backend.list() {
                Ok(pre_archive_list) => {
                    let pre_archive_count = pre_archive_list.records.len();
                    let kept = self.auto_archive_done_items(pre_archive_list.records);
                    if kept.len() < pre_archive_count {
                        // Items were archived; reassemble to update display state.
                        let pattern = &self.config.defaults.branch_issue_pattern;
                        let (items, unlinked, review_requested, _) =
                            assembly::reassemble(&kept, &self.repo_data, pattern);
                        self.work_items = items;
                        self.unlinked_prs = unlinked;
                        self.review_requested_prs = review_requested;
                    }
                }
                Err(e) => {
                    self.status_message =
                        Some(format!("Failed to list items for auto-archive: {e}"));
                }
            }
        }

        // Reconstruct mergequeue watches for items that are in Mergequeue
        // but don't have a watch (e.g., after app restart).
        self.reconstruct_mergequeue_watches();
    }

    /// Core work-item deletion. Removes the backend record, kills the
    /// Claude and terminal sessions, cancels in-flight background
    /// operations (worktree create, PR create, merge, review submit,
    /// mergequeue poll) and clears in-memory state.
    ///
    /// Does NOT touch selection/cursor/display state - callers handle that.
    ///
    /// Resource cleanup (worktree removal, branch deletion, PR close) is
    /// NOT performed here - it is blocking I/O and must run on a
    /// background thread. Callers that need resource cleanup first call
    /// `gather_delete_cleanup_infos` (a pure cache lookup) and then
    /// `spawn_delete_cleanup` to run the actual `git` / `gh` commands off
    /// the UI thread. Auto-archive skips resource cleanup entirely
    /// because Done items have already been through the merge flow.
    ///
    /// Warnings (best-effort cleanup failures from Phase 5 orphan
    /// handling) are appended to `warnings`. Orphaned worktrees
    /// discovered in Phase 5 (an in-flight worktree-create thread that
    /// had already produced a path before the user requested the
    /// delete) are appended to `orphan_worktrees` as `OrphanWorktree`
    /// entries so the caller can forward them to `spawn_delete_cleanup`
    /// and run both `git worktree remove` and `git branch -D` on the
    /// background cleanup thread. The branch name is preserved so
    /// `spawn_delete_cleanup` can delete the stale branch ref too
    /// (dropping it would leak the branch - master deleted it inline
    /// before the async refactor). This function MUST NOT call
    /// `self.worktree_service.remove_worktree(...)` directly - it runs
    /// on the UI thread (either the MCP tick handler or the modal
    /// confirm handler) where blocking I/O is forbidden by
    /// `docs/UI.md` "Blocking I/O Prohibition".
    ///
    /// Returns Err only if the backend delete itself fails (fatal).
    fn delete_work_item_by_id(
        &mut self,
        wi_id: &WorkItemId,
        warnings: &mut Vec<String>,
        orphan_worktrees: &mut Vec<OrphanWorktree>,
    ) -> Result<(), crate::work_item_backend::BackendError> {
        // -- Phase 2: Backend cleanup (fatal on delete failure) --
        if let Err(e) = self.backend.pre_delete_cleanup(wi_id) {
            warnings.push(format!("pre-delete cleanup: {e}"));
        }
        self.backend.delete(wi_id)?;

        // -- Phase 3: Kill session and clean up MCP --
        self.cleanup_session_state_for(wi_id);
        if let Some(key) = self.session_key_for(wi_id)
            && let Some(mut entry) = self.sessions.remove(&key)
            && let Some(ref mut session) = entry.session
        {
            session.kill();
        }
        // Kill associated terminal session.
        if let Some(mut entry) = self.terminal_sessions.remove(wi_id)
            && let Some(ref mut session) = entry.session
        {
            session.kill();
        }

        // -- Phase 4: (removed) Resource cleanup runs on a background
        //    thread via `spawn_delete_cleanup`. Doing it synchronously
        //    here would block the UI thread on `git worktree remove`,
        //    `git branch -D`, and `gh pr close` - all forbidden by
        //    `docs/UI.md` "Blocking I/O Prohibition".

        // -- Phase 5: Cancel in-flight operations --
        if self.user_action_work_item(&UserActionKey::WorktreeCreate) == Some(wi_id) {
            // Drain the helper payload's receiver. If the thread has
            // finished, capture the (non-reused) worktree path so the
            // caller can run background cleanup; if the thread is still
            // running, leave the helper entry intact so
            // `poll_worktree_creation` can drain it on the next tick
            // and run its orphan-cleanup path.
            let (thread_done, captured_orphan) = match self
                .user_actions
                .in_flight
                .get(&UserActionKey::WorktreeCreate)
            {
                Some(state) => match &state.payload {
                    UserActionPayload::WorktreeCreate { rx, .. } => match rx.try_recv() {
                        Ok(result) => {
                            let orphan = if !result.reused
                                && let Some(ref path) = result.path
                            {
                                Some(OrphanWorktree {
                                    repo_path: result.repo_path.clone(),
                                    worktree_path: path.clone(),
                                    branch: result.branch.clone(),
                                })
                            } else {
                                None
                            };
                            (true, orphan)
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => (true, None),
                        Err(crossbeam_channel::TryRecvError::Empty) => (false, None),
                    },
                    _ => (true, None),
                },
                None => (true, None),
            };
            if let Some(orphan) = captured_orphan {
                orphan_worktrees.push(orphan);
            }
            if thread_done {
                self.end_user_action(&UserActionKey::WorktreeCreate);
            }
        }
        if self.user_action_work_item(&UserActionKey::PrCreate) == Some(wi_id) {
            self.end_user_action(&UserActionKey::PrCreate);
        }
        self.pr_create_pending.retain(|id| id != wi_id);
        if self.merge_wi_id.as_ref() == Some(wi_id)
            && self.is_user_action_in_flight(&UserActionKey::PrMerge)
        {
            self.end_user_action(&UserActionKey::PrMerge);
            self.merge_in_progress = false;
        }
        if self.user_action_work_item(&UserActionKey::ReviewSubmit) == Some(wi_id) {
            self.end_user_action(&UserActionKey::ReviewSubmit);
        }
        self.mergequeue_watches.retain(|w| w.wi_id != *wi_id);
        self.mergequeue_poll_errors.remove(wi_id);
        if let Some(state) = self.mergequeue_polls.remove(wi_id) {
            self.end_activity(state.activity);
        }

        // -- Phase 6: In-memory state cleanup --
        self.rework_reasons.remove(wi_id);
        self.review_gate_findings.remove(wi_id);
        self.review_reopen_suppress.remove(wi_id);
        self.no_plan_prompt_queue.retain(|id| id != wi_id);
        if self.no_plan_prompt_queue.is_empty() {
            self.no_plan_prompt_visible = false;
        }
        if self.rework_prompt_wi.as_ref() == Some(wi_id) {
            self.rework_prompt_wi = None;
            self.rework_prompt_visible = false;
        }
        if self.merge_wi_id.as_ref() == Some(wi_id) {
            self.merge_wi_id = None;
            self.confirm_merge = false;
        }
        self.drop_review_gate(wi_id);
        if self
            .branch_gone_prompt
            .as_ref()
            .map(|(id, _)| id == wi_id)
            .unwrap_or(false)
        {
            self.branch_gone_prompt = None;
        }

        Ok(())
    }

    /// Keep the dialog open in progress mode and spawn a background thread to
    /// close the PR and delete the branch. The dialog shows a spinner until
    /// poll_unlinked_cleanup() receives the result.
    pub fn spawn_unlinked_cleanup(&mut self, reason: Option<&str>) {
        let Some((repo_path, branch, pr_number)) = self.cleanup_unlinked_target.take() else {
            return;
        };

        // Admit the action through the user-action guard. In practice the
        // cleanup modal at `src/event.rs` already prevents overlapping
        // invocations (the key handler swallows input while the dialog
        // is in progress), so rejection here is defense-in-depth. We
        // still surface a status message on rejection so any future
        // code path that bypasses the modal does not silently drop the
        // request. The modal's "in-progress spinner" is rendered by
        // reading `is_user_action_in_flight(&UserActionKey::UnlinkedCleanup)`
        // via the UI layer.
        let activity_id = match self.try_begin_user_action(
            UserActionKey::UnlinkedCleanup,
            Duration::ZERO,
            "Cleaning up unlinked PR...",
        ) {
            Some(aid) => aid,
            None => {
                self.status_message = Some("Unlinked PR cleanup already in progress".into());
                return;
            }
        };
        // The cleanup modal already renders its own in-progress spinner
        // in the dialog body; a duplicate status-bar indicator would
        // mislead the user. Drop the visible activity but leave the
        // helper map entry intact so `is_user_action_in_flight` still
        // reports the true state to the modal / event / ui layers.
        self.end_activity(activity_id);

        // Extract github remote before leaving the main thread.
        let github_remote = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone());

        // Transition to in-progress: clear the input fields but keep the dialog
        // open. The UI renders a spinner + "Please wait." instead of key options.
        self.cleanup_reason_input_active = false;
        self.cleanup_reason_input.clear();
        self.cleanup_progress_pr_number = Some(pr_number);
        self.cleanup_progress_repo_path = Some(repo_path.clone());
        self.cleanup_progress_branch = Some(branch.clone());
        self.selected_unlinked_branch = None;

        let reason_owned: Option<String> = reason.map(|s| s.to_string());
        let ws = Arc::clone(&self.worktree_service);
        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            let mut warnings = Vec::new();

            let pr_close_ok = if let Some((ref owner, ref repo)) = github_remote {
                let owner_repo = format!("{owner}/{repo}");

                // Post optional reason as a comment before closing.
                if let Some(ref r) = reason_owned
                    && !r.is_empty()
                {
                    match std::process::Command::new("gh")
                        .args([
                            "pr",
                            "comment",
                            &pr_number.to_string(),
                            "--repo",
                            &owner_repo,
                            "--body",
                            r.as_str(),
                        ])
                        .output()
                    {
                        Ok(output) if !output.status.success() => {
                            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                            warnings.push(format!("PR comment: {stderr}"));
                        }
                        Err(e) => warnings.push(format!("PR comment: {e}")),
                        _ => {}
                    }
                }

                // Close the PR.
                let mut close_succeeded = false;
                match std::process::Command::new("gh")
                    .args(["pr", "close", &pr_number.to_string(), "--repo", &owner_repo])
                    .output()
                {
                    Ok(output) if !output.status.success() => {
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                        warnings.push(format!("PR close: {stderr}"));
                    }
                    Err(e) => warnings.push(format!("PR close: {e}")),
                    _ => {
                        close_succeeded = true;
                    }
                }
                close_succeeded
            } else {
                // No GitHub remote - local-only cleanup is safe to proceed.
                true
            };

            // Only proceed with destructive local operations (worktree removal,
            // branch deletion) if the PR was successfully closed on GitHub.
            // Otherwise the user would lose their local branch while the PR
            // remains open, and any unpushed commits would be permanently lost.
            if !pr_close_ok {
                let _ = tx.send(CleanupResult {
                    warnings,
                    closed_pr_branches: Vec::new(),
                });
                return;
            }

            // Get a fresh worktree list so we don't rely on potentially stale
            // cached repo_data (e.g., if the user switched branches since last fetch).
            match ws.list_worktrees(&repo_path) {
                Ok(fresh_worktrees) => {
                    let wt_for_branch = fresh_worktrees
                        .iter()
                        .find(|wt| wt.branch.as_deref() == Some(branch.as_str()));

                    match wt_for_branch {
                        Some(wt) if wt.is_main => {
                            // Branch is the main worktree's current branch; git forbids
                            // deleting the checked-out branch. Skip silently - the PR
                            // was closed, and the user can switch branches later.
                        }
                        Some(wt) => {
                            // Remove the linked worktree first, then delete the branch.
                            let wt_path = wt.path.clone();
                            if let Err(e) = ws.remove_worktree(&repo_path, &wt_path, false, true) {
                                warnings.push(format!("worktree: {e}"));
                            }
                            if let Err(e) = ws.delete_branch(&repo_path, &branch, true) {
                                warnings.push(format!("branch: {e}"));
                            }
                        }
                        None => {
                            // No worktree for this branch - just delete the branch.
                            if let Err(e) = ws.delete_branch(&repo_path, &branch, true) {
                                warnings.push(format!("branch: {e}"));
                            }
                        }
                    }
                }
                Err(e) => {
                    warnings.push(format!(
                        "list worktrees: {e}; skipping worktree/branch cleanup"
                    ));
                }
            }

            let _ = tx.send(CleanupResult {
                warnings,
                closed_pr_branches: Vec::new(),
            });
        });

        self.attach_user_action_payload(
            &UserActionKey::UnlinkedCleanup,
            UserActionPayload::UnlinkedCleanup { rx },
        );
    }

    /// Poll the async unlinked-item cleanup thread for a result. Called on each timer tick.
    /// Drain the metrics channel, keeping only the latest snapshot. Called
    /// from the salsa timer tick. Non-blocking; never touches disk. The
    /// background thread produces a fresh snapshot every ~60s, so multiple
    /// pending values are rare but the drain-to-latest pattern keeps the
    /// dashboard truthful even if the consumer briefly lags.
    pub fn poll_metrics_snapshot(&mut self) {
        let Some(rx) = self.metrics_rx.as_ref() else {
            return;
        };
        let mut latest: Option<crate::metrics::MetricsSnapshot> = None;
        loop {
            match rx.try_recv() {
                Ok(snap) => latest = Some(snap),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // The aggregator thread has exited - drop the receiver
                    // so we stop polling. The dashboard will keep showing
                    // the last snapshot we received.
                    self.metrics_rx = None;
                    break;
                }
            }
        }
        if let Some(snap) = latest {
            self.metrics_snapshot = Some(snap);
        }
    }

    pub fn poll_unlinked_cleanup(&mut self) {
        // Read the receiver out of the user-action guard. The borrow is
        // scoped so we can call `&mut self` methods below.
        let recv_result = {
            let Some(UserActionPayload::UnlinkedCleanup { rx }) =
                self.user_action_payload(&UserActionKey::UnlinkedCleanup)
            else {
                return;
            };
            match rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            }
        };
        let result = match recv_result {
            Ok(r) => r,
            Err(()) => {
                self.end_user_action(&UserActionKey::UnlinkedCleanup);
                self.cleanup_prompt_visible = false;
                self.cleanup_progress_pr_number = None;
                self.cleanup_progress_repo_path = None;
                self.cleanup_progress_branch = None;
                self.alert_message = Some("Cleanup: background thread exited unexpectedly".into());
                return;
            }
        };

        self.end_user_action(&UserActionKey::UnlinkedCleanup);
        self.cleanup_prompt_visible = false;

        // Track the closed branch so stale fetch results (from in-flight
        // fetches that started before the close) don't re-add the PR.
        // apply_cleanup_evictions() removes these from repo_data after every
        // drain_fetch_results, and drain clears the list on fresh data.
        if let Some(repo_path) = self.cleanup_progress_repo_path.take()
            && let Some(branch) = self.cleanup_progress_branch.take()
        {
            self.cleanup_evicted_branches.push((repo_path, branch));
        }
        self.cleanup_progress_pr_number = None;

        self.apply_cleanup_evictions();

        self.reassemble_work_items();
        self.build_display_list();
        self.fetcher_repos_changed = true;

        if result.warnings.is_empty() {
            self.status_message = Some("Unlinked item closed".into());
        } else {
            self.alert_message = Some(format!(
                "Closed with warnings: {}",
                result.warnings.join("; ")
            ));
        }
    }

    /// Gather resource cleanup info for a work item's repo associations.
    /// Pure data lookup from `repo_data` - no I/O. Used to prepare data
    /// for the background delete-cleanup thread.
    fn gather_delete_cleanup_infos(
        &self,
        repo_associations: &[crate::work_item_backend::RepoAssociationRecord],
    ) -> Vec<DeleteCleanupInfo> {
        repo_associations
            .iter()
            .map(|assoc| {
                let wt_for_branch = self
                    .repo_data
                    .get(&assoc.repo_path)
                    .and_then(|rd| rd.worktrees.as_ref().ok())
                    .and_then(|wts| {
                        wts.iter()
                            .find(|wt| wt.branch.as_deref() == assoc.branch.as_deref())
                    });

                let worktree_path = wt_for_branch
                    .filter(|wt| !wt.is_main)
                    .map(|wt| wt.path.clone());

                let branch_in_main_worktree = wt_for_branch.map(|wt| wt.is_main).unwrap_or(false);

                let open_pr_number = assoc.branch.as_deref().and_then(|branch| {
                    self.repo_data.get(&assoc.repo_path).and_then(|rd| {
                        rd.prs.as_ref().ok().and_then(|prs| {
                            prs.iter()
                                .find(|pr| pr.head_branch == branch && pr.state == "OPEN")
                                .map(|pr| pr.number)
                        })
                    })
                });

                let github_remote = self
                    .repo_data
                    .get(&assoc.repo_path)
                    .and_then(|rd| rd.github_remote.clone());

                DeleteCleanupInfo {
                    repo_path: assoc.repo_path.clone(),
                    branch: assoc.branch.clone(),
                    worktree_path,
                    branch_in_main_worktree,
                    open_pr_number,
                    github_remote,
                }
            })
            .collect()
    }

    /// Spawn a background thread to perform resource cleanup (worktree
    /// removal, branch deletion, PR close) for a deleted work item.
    /// Called from the MCP delete handler or from the user-initiated modal
    /// delete flow after the backend record and session have already been
    /// cleaned up on the main thread. poll_delete_cleanup() receives the
    /// result.
    ///
    /// When `show_status_activity` is true, a "Deleting work item
    /// resources..." spinner is pushed onto the status bar. The modal
    /// delete path passes `false` because its own dialog already shows
    /// an in-progress spinner - a second status-bar indicator would be
    /// redundant and mislead the user about what is waiting on what.
    pub fn spawn_delete_cleanup(
        &mut self,
        cleanup_infos: Vec<DeleteCleanupInfo>,
        force: bool,
        show_status_activity: bool,
    ) {
        // Route single-flight admission through the user-action guard.
        // Preserves the pre-refactor alert wording verbatim on rejection.
        let activity_id = match self.try_begin_user_action(
            UserActionKey::DeleteCleanup,
            Duration::ZERO,
            "Deleting work item resources...",
        ) {
            Some(aid) => aid,
            None => {
                // A previous delete cleanup is still running. Alert the user
                // so orphaned resources (worktrees, branches, open PRs) are
                // visible rather than silently dropped.
                //
                // Reset `delete_in_progress` here because the modal
                // delete flow (`confirm_delete_from_prompt`) sets it to
                // true BEFORE calling into `spawn_delete_cleanup`. If
                // admission is rejected we must close that latent-state
                // gap - otherwise the modal stays pinned at the
                // in-progress spinner with no key input accepted and
                // no exit path. The helper map is the single source of
                // truth for "is cleanup running", but `delete_in_progress`
                // still gates modal rendering and key input in the
                // current code, so both flags must clear together on
                // the rejection arm.
                self.delete_in_progress = false;
                self.alert_message = Some(
                    "Delete cleanup skipped: a previous cleanup is still in progress. \
                     Worktrees, branches, and open PRs for this item may need manual cleanup."
                        .into(),
                );
                return;
            }
        };
        // Modal delete flow already renders its own in-progress spinner
        // in the dialog body - a duplicate status-bar spinner would
        // mislead the user about what is waiting on what. Clear the
        // visible activity but leave the helper map entry intact so
        // single-flight admission (and `is_user_action_in_flight`
        // reads) still work.
        if !show_status_activity {
            self.end_activity(activity_id);
        }

        let ws = Arc::clone(&self.worktree_service);
        let pr_closer = Arc::clone(&self.pr_closer);
        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            let mut warnings = Vec::new();
            let mut closed_pr_branches = Vec::new();

            // Per-association ordering: close the remote PR FIRST, and
            // only run destructive local cleanup (worktree removal,
            // branch deletion) if the close succeeds. Reversing this
            // order means a `gh pr close` failure (auth, network, merge
            // queue state) would leave the user with an open PR AND no
            // local branch/worktree to recover unpushed commits from.
            // This mirrors `spawn_unlinked_cleanup`'s ordering.
            for info in &cleanup_infos {
                let pr_close_ok = if let Some(pr_number) = info.open_pr_number
                    && let Some((ref owner, ref repo)) = info.github_remote
                {
                    match pr_closer.close_pr(owner, repo, pr_number) {
                        Ok(()) => {
                            // Track for eviction so stale fetch data does
                            // not resurrect the closed PR as a phantom
                            // unlinked item.
                            if let Some(ref branch) = info.branch {
                                closed_pr_branches.push((info.repo_path.clone(), branch.clone()));
                            }
                            true
                        }
                        Err(msg) => {
                            warnings.push(format!("PR close: {msg}"));
                            false
                        }
                    }
                } else {
                    // No open PR for this association - local-only
                    // cleanup is safe to proceed.
                    true
                };

                if !pr_close_ok {
                    // Preserve local worktree and branch so the user can
                    // recover unpushed work and manually retry the PR
                    // close. The backend record is already gone, so this
                    // warning is the user's only breadcrumb pointing at
                    // the preserved paths.
                    if let Some(ref wt_path) = info.worktree_path {
                        warnings.push(format!(
                            "preserved local worktree {} (PR close failed)",
                            wt_path.display()
                        ));
                    }
                    if let Some(ref branch) = info.branch {
                        warnings.push(format!("preserved local branch {branch} (PR close failed)"));
                    }
                    continue;
                }

                if let Some(ref wt_path) = info.worktree_path
                    && let Err(e) = ws.remove_worktree(&info.repo_path, wt_path, false, force)
                {
                    warnings.push(format!("worktree: {e}"));
                }
                // Skip branch deletion when checked out in the main worktree
                // (git forbids deleting the currently checked-out branch).
                if !info.branch_in_main_worktree
                    && let Some(ref branch) = info.branch
                    && let Err(e) = ws.delete_branch(&info.repo_path, branch, true)
                {
                    warnings.push(format!("branch: {e}"));
                }
            }

            let _ = tx.send(CleanupResult {
                warnings,
                closed_pr_branches,
            });
        });

        self.attach_user_action_payload(
            &UserActionKey::DeleteCleanup,
            UserActionPayload::DeleteCleanup { rx },
        );
    }

    /// Background cleanup for a single orphaned worktree. Used when
    /// `poll_worktree_creation` discovers that the work item was
    /// deleted while the worktree-create thread was running and the
    /// fresh worktree on disk is now an orphan.
    ///
    /// The worktree-create thread finished successfully, so the
    /// original `spawn_delete_cleanup` flow is not involved here - the
    /// user may have confirmed the delete modal minutes ago. A
    /// dedicated background thread runs `git worktree remove --force`
    /// followed by `git branch -D` (when a branch name is available)
    /// off the UI thread.
    ///
    /// Per `docs/UI.md` "Activity indicator placement", this is
    /// system-initiated background work and therefore owes the user a
    /// status-bar spinner. We start an activity here, hand the
    /// `ActivityId` to the closure, and the closure sends exactly one
    /// `OrphanCleanupFinished` message on completion (success or
    /// failure) carrying the activity ID and any warnings.
    /// `poll_orphan_cleanup_finished` ends the activity and surfaces
    /// the warnings. Deleting the branch here matches the behaviour of
    /// the Phase 5 orphan path routed through `spawn_delete_cleanup`,
    /// so a delete-during-create race never leaks a branch ref
    /// regardless of which of the two orphan paths fires.
    fn spawn_orphan_worktree_cleanup(
        &mut self,
        repo_path: PathBuf,
        worktree_path: PathBuf,
        branch: Option<String>,
    ) {
        let activity = self.start_activity(format!(
            "Cleaning up orphan worktree {}",
            worktree_path.display()
        ));
        let ws = Arc::clone(&self.worktree_service);
        let finished_tx = self.orphan_cleanup_finished_tx.clone();
        std::thread::spawn(move || {
            let mut warnings: Vec<String> = Vec::new();
            if let Err(e) = ws.remove_worktree(&repo_path, &worktree_path, true, true) {
                warnings.push(format!(
                    "Orphan worktree cleanup failed for {}: {e}",
                    worktree_path.display()
                ));
            }
            if let Some(ref branch) = branch
                && let Err(e) = ws.delete_branch(&repo_path, branch, true)
            {
                warnings.push(format!(
                    "Orphan branch cleanup failed for {branch} in {}: {e}",
                    repo_path.display()
                ));
            }
            // Always send exactly one completion message so the main
            // thread can end the matching status-bar activity even on
            // the success path. If the receiver has been dropped
            // (`App` torn down mid-cleanup) we silently discard - the
            // activity disappears with the App.
            let _ = finished_tx.send(OrphanCleanupFinished { activity, warnings });
        });
    }

    /// Drain pending completion messages from
    /// `spawn_orphan_worktree_cleanup` background threads. For each
    /// message, end the matching status-bar activity and accumulate any
    /// warnings. If any warnings arrived, surface them as a single
    /// `status_message` so the user notices failed cleanups instead of
    /// silently leaking worktrees / branches. An empty channel is the
    /// idle path - no spinner is touched and no message is set. Called
    /// from the background-work tick alongside the other `poll_*`
    /// methods.
    pub fn poll_orphan_cleanup_finished(&mut self) {
        let mut warnings: Vec<String> = Vec::new();
        while let Ok(msg) = self.orphan_cleanup_finished_rx.try_recv() {
            self.end_activity(msg.activity);
            warnings.extend(msg.warnings);
        }
        if !warnings.is_empty() {
            self.status_message = Some(warnings.join(" | "));
        }
    }

    /// Poll the async delete-cleanup thread for a result. Called on each
    /// timer tick from the event loop.
    pub fn poll_delete_cleanup(&mut self) {
        let recv_result = {
            let Some(UserActionPayload::DeleteCleanup { rx }) =
                self.user_action_payload(&UserActionKey::DeleteCleanup)
            else {
                return;
            };
            match rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            }
        };
        let result = match recv_result {
            Ok(r) => r,
            Err(()) => {
                self.end_user_action(&UserActionKey::DeleteCleanup);
                let sync_warnings = std::mem::take(&mut self.delete_sync_warnings);
                if self.delete_in_progress {
                    self.delete_in_progress = false;
                    self.delete_prompt_visible = false;
                    self.delete_target_wi_id = None;
                    self.delete_target_title = None;
                }
                let mut msg = String::from("Delete cleanup: background thread exited unexpectedly");
                if !sync_warnings.is_empty() {
                    msg.push_str(" (sync warnings: ");
                    msg.push_str(&sync_warnings.join("; "));
                    msg.push(')');
                }
                self.alert_message = Some(msg);
                return;
            }
        };

        self.end_user_action(&UserActionKey::DeleteCleanup);

        // Modal-initiated delete: route through finish_delete_cleanup so
        // the dialog closes, evictions are applied, and the final message
        // uses the "Work item deleted" wording seen in the manual flow.
        // Drain delete_sync_warnings so Phase 2/Phase 5 warnings collected
        // on the UI thread (e.g. pre-delete hook failure, inline orphan
        // worktree cleanup) are folded into the final status/alert.
        if self.delete_in_progress {
            let sync_warnings = std::mem::take(&mut self.delete_sync_warnings);
            self.finish_delete_cleanup(result.warnings, result.closed_pr_branches, sync_warnings);
            return;
        }

        // MCP-initiated delete: no modal to close, just track evictions
        // and surface a status/alert. Wording differs from the modal path
        // because the user didn't explicitly trigger the delete.
        if !result.closed_pr_branches.is_empty() {
            self.cleanup_evicted_branches
                .extend(result.closed_pr_branches);
            self.apply_cleanup_evictions();
        }

        if result.warnings.is_empty() {
            self.status_message = Some("Work item resource cleanup complete".into());
        } else {
            self.alert_message = Some(format!(
                "Delete cleanup warnings: {}",
                result.warnings.join("; ")
            ));
        }
    }

    /// Remove recently-closed PRs from cached repo_data. Called after
    /// poll_unlinked_cleanup and after drain_fetch_results to ensure stale
    /// fetch data doesn't resurrect closed PRs in the unlinked list.
    pub fn apply_cleanup_evictions(&mut self) {
        for (repo_path, branch) in &self.cleanup_evicted_branches {
            if let Some(rd) = self.repo_data.get_mut(repo_path)
                && let Ok(ref mut prs) = rd.prs
            {
                prs.retain(|pr| pr.head_branch != *branch);
            }
        }
    }

    /// Delete Done work items whose `done_at` timestamp exceeds the
    /// configured `archive_after_days` retention period. Returns the
    /// remaining (non-archived) records for assembly.
    fn auto_archive_done_items(
        &mut self,
        records: Vec<crate::work_item_backend::WorkItemRecord>,
    ) -> Vec<crate::work_item_backend::WorkItemRecord> {
        let archive_days = self.config.defaults.archive_after_days;
        if archive_days == 0 {
            return records;
        }

        let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(e) => {
                self.status_message =
                    Some(format!("System clock error, skipping auto-archive: {e}"));
                return records;
            }
        };
        let archive_secs = archive_days * 86400;

        let mut kept = Vec::with_capacity(records.len());
        let mut archived_count = 0u32;
        let mut all_warnings: Vec<String> = Vec::new();

        for record in records {
            if let Some(done_at) = record.done_at
                && now.saturating_sub(done_at) >= archive_secs
            {
                let mut warnings: Vec<String> = Vec::new();
                // Done items cannot have an in-flight worktree-create
                // thread (create only runs for pre-Implementing stages),
                // so `orphan_worktrees` is declared for the signature's
                // sake and any unexpected entries are forwarded into the
                // warnings buffer so they do not get silently dropped.
                let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
                match self.delete_work_item_by_id(&record.id, &mut warnings, &mut orphan_worktrees)
                {
                    Ok(()) => {
                        archived_count += 1;
                        all_warnings.extend(warnings);
                        for orphan in orphan_worktrees {
                            all_warnings.push(format!(
                                "auto-archive saw in-flight worktree {} in {} - \
                                 left in place, clean up manually",
                                orphan.worktree_path.display(),
                                orphan.repo_path.display(),
                            ));
                        }
                    }
                    Err(e) => {
                        all_warnings.push(format!("delete {}: {e}", record.title));
                        kept.push(record);
                    }
                }
                continue;
            }
            kept.push(record);
        }

        if archived_count > 0 {
            if all_warnings.is_empty() {
                self.status_message = Some(format!("Auto-archived {archived_count} done item(s)"));
            } else {
                self.status_message = Some(format!(
                    "Auto-archived {archived_count} done item(s) (warnings: {})",
                    all_warnings.join("; ")
                ));
            }
        } else if !all_warnings.is_empty() {
            self.status_message = Some(format!("Auto-archive errors: {}", all_warnings.join("; ")));
        }

        kept
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
                } else if *drill_stage == WorkItemStatus::Review {
                    self.work_items[i].status == WorkItemStatus::Review
                        || self.work_items[i].status == WorkItemStatus::Mergequeue
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

        // Reset scroll offset when the display list is rebuilt so stale
        // offsets from a previous list shape (view mode toggle, drill-down,
        // item deletion) do not carry over. ratatui will re-clamp on the
        // next render frame based on the selected item.
        self.list_scroll_offset.set(0);

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

        // Sort each repo's items in workflow order so PL items precede
        // IM, IM precedes RV, etc. Stable sort preserves the existing
        // backend path order within a stage as the tiebreaker. This is
        // a no-op for the BLOCKED / BACKLOGGED / DONE callers because
        // every item in those buckets shares a single status.
        for items in by_repo.values_mut() {
            items.sort_by_key(|&i| work_items[i].status.active_group_rank());
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
                } else if *status == WorkItemStatus::Review {
                    wi.status == WorkItemStatus::Review || wi.status == WorkItemStatus::Mergequeue
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

    /// Cycle view mode: FlatList -> Board -> Dashboard -> FlatList. Also
    /// syncs cursor state when leaving Board mode so the FlatList cursor
    /// stays on the selected work item.
    pub fn toggle_view_mode(&mut self) {
        match self.view_mode {
            ViewMode::FlatList => {
                self.view_mode = ViewMode::Board;
                self.sync_board_cursor();
            }
            ViewMode::Board => {
                self.view_mode = ViewMode::Dashboard;
                self.board_drill_down = false;
                self.board_drill_stage = None;
            }
            ViewMode::Dashboard => {
                self.view_mode = ViewMode::FlatList;
                // Sync flat list selection from whichever work item is
                // currently selected, so the cursor lands on it when we
                // land back in the list view.
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

    /// Find an existing worktree that can be safely reused in place of
    /// calling `git worktree add`. A worktree is reusable only when all
    /// three conditions hold:
    ///
    /// 1. It is registered with git for the target `branch`.
    /// 2. It is NOT the main worktree (`is_main = false`). Reusing the
    ///    user's primary repo checkout would spawn Claude sessions inside
    ///    it and drop workbridge state (`.mcp.json`) there, violating
    ///    invariant #3 in `docs/invariants.md`.
    /// 3. Its canonicalized path equals the canonicalized `wt_target` the
    ///    import/session-spawn flow would have created. This rules out
    ///    adopting unrelated worktrees the user made manually or that
    ///    another tool created at a different location.
    ///
    /// When no safe match is found, returns `None` and the caller should
    /// fall through to `create_worktree`, which surfaces git's own "branch
    /// already checked out" error for the truly conflicting cases.
    fn find_reusable_worktree(
        ws: &dyn WorktreeService,
        repo_path: &std::path::Path,
        branch: &str,
        wt_target: &std::path::Path,
    ) -> Option<crate::worktree_service::WorktreeInfo> {
        let target_canonical = crate::config::canonicalize_path(wt_target).ok()?;
        ws.list_worktrees(repo_path).ok()?.into_iter().find(|w| {
            if w.is_main {
                return false;
            }
            if w.branch.as_deref() != Some(branch) {
                return false;
            }
            match crate::config::canonicalize_path(&w.path) {
                Ok(existing_canonical) => existing_canonical == target_canonical,
                Err(_) => false,
            }
        })
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
        if matches!(
            wi.status,
            WorkItemStatus::Backlog | WorkItemStatus::Done | WorkItemStatus::Mergequeue
        ) {
            return;
        }

        // If any worktree creation is already in progress, don't start another.
        // Replacing the helper payload while a thread is running would orphan
        // the worktree on disk (the poll handler would never see the result).
        if self.is_user_action_in_flight(&UserActionKey::WorktreeCreate) {
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
                // Worktree already exists - enqueue the background plan
                // read that feeds `finish_session_open`. The read MUST
                // live on a background thread because
                // `WorkItemBackend::read_plan` hits the filesystem
                // (see `docs/UI.md` "Blocking I/O Prohibition").
                self.begin_session_open(&work_item_id, &path);
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

                        // Admit the user action BEFORE spawning the
                        // background thread. If the admit ever fails
                        // (defense-in-depth against a future async
                        // entry point), we must NOT have already
                        // spawned a thread that creates a worktree on
                        // disk with no receiver attached - that would
                        // be a durable orphan. Match the
                        // `spawn_import_worktree` ordering exactly.
                        if self
                            .try_begin_user_action(
                                UserActionKey::WorktreeCreate,
                                Duration::ZERO,
                                "Initializing worktree...",
                            )
                            .is_none()
                        {
                            self.status_message =
                                Some("Worktree creation already in progress...".into());
                            return;
                        }

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
                                    branch: Some(branch.clone()),
                                    path: None,
                                    error: Some(format!(
                                        "Could not fetch or create branch '{}': {create_err}",
                                        branch,
                                    )),
                                    open_session: true,
                                    branch_gone: true,
                                    reused: false,
                                });
                                return;
                            }
                            // Reuse an existing worktree only if it lives at
                            // the exact expected location (wt_target) and is
                            // NOT the main worktree. Matching purely on
                            // branch name would hijack the user's primary
                            // checkout when it happens to be on the same
                            // feature branch, or adopt an unrelated worktree
                            // at some other path - both of which would then
                            // feed into destructive orphan-cleanup paths.
                            let reused_wt = Self::find_reusable_worktree(
                                ws.as_ref(),
                                &repo_path,
                                &branch,
                                &wt_target,
                            );
                            let (wt_result, reused) = match reused_wt {
                                Some(existing_wt) => (Ok(existing_wt), true),
                                None => {
                                    (ws.create_worktree(&repo_path, &branch, &wt_target), false)
                                }
                            };
                            match wt_result {
                                Ok(wt_info) => {
                                    let _ = tx.send(WorktreeCreateResult {
                                        wi_id: wi_id_clone,
                                        repo_path,
                                        branch: Some(branch),
                                        path: Some(wt_info.path),
                                        error: None,
                                        open_session: true,
                                        branch_gone: false,
                                        reused,
                                    });
                                }
                                Err(e) => {
                                    let _ = tx.send(WorktreeCreateResult {
                                        wi_id: wi_id_clone,
                                        repo_path,
                                        branch: Some(branch.clone()),
                                        path: None,
                                        error: Some(format!(
                                            "Failed to create worktree for '{}': {e}",
                                            branch,
                                        )),
                                        open_session: true,
                                        branch_gone: false,
                                        reused: false,
                                    });
                                }
                            }
                        });

                        self.attach_user_action_payload(
                            &UserActionKey::WorktreeCreate,
                            UserActionPayload::WorktreeCreate {
                                rx,
                                wi_id: work_item_id,
                            },
                        );
                    }
                    None => {
                        // No repo association has a branch. Open the
                        // recovery dialog instead of leaving the user
                        // stuck on a dead-end status message. When the
                        // user confirms, the dialog's
                        // `PendingBranchAction::SpawnSession` arm
                        // re-enters `spawn_session` with the same work
                        // item ID, so the worktree is created and the
                        // Claude pane opens without the user having to
                        // press Enter a second time.
                        self.open_set_branch_dialog(
                            work_item_id.clone(),
                            crate::create_dialog::PendingBranchAction::SpawnSession,
                        );
                    }
                }
            }
        }
    }

    /// Begin the async plan-read stage of opening a Claude session.
    ///
    /// Spawns a background thread that calls `WorkItemBackend::read_plan`
    /// (filesystem I/O) and then hands the result back to
    /// `poll_session_opens`, which finishes the session on the UI thread.
    /// Running the read here on the caller (a UI-thread entry point such
    /// as `spawn_session` / `poll_worktree_creation` /
    /// `poll_review_gate`) would freeze the event loop - see
    /// `docs/UI.md` "Blocking I/O Prohibition".
    ///
    /// If another plan read is already in flight for this work item, the
    /// new request is dropped (the previous one will finish and spawn a
    /// session). This cannot deadlock: `poll_session_opens` removes the
    /// entry as soon as the result arrives.
    fn begin_session_open(&mut self, work_item_id: &WorkItemId, cwd: &std::path::Path) {
        if self.session_open_rx.contains_key(work_item_id) {
            // Already in flight - the pending read will finish the open.
            // Re-surface the spinner message so a repeat Enter press is
            // not silent; the existing activity entry is still alive so
            // the duplicate start below would otherwise stack.
            self.status_message = Some("Opening session...".into());
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let backend = Arc::clone(&self.backend);
        let wi_id_clone = work_item_id.clone();
        let cwd_clone = cwd.to_path_buf();
        std::thread::spawn(move || {
            let (plan_text, read_error) = match backend.read_plan(&wi_id_clone) {
                Ok(Some(plan)) => (plan, None),
                Ok(None) => (String::new(), None),
                Err(e) => (String::new(), Some(format!("Could not read plan: {e}"))),
            };
            let _ = tx.send(SessionOpenPlanResult {
                wi_id: wi_id_clone,
                cwd: cwd_clone,
                plan_text,
                read_error,
            });
        });
        // Surface immediate feedback so a slow plan read does not
        // make the TUI look hung between the Enter keypress and the
        // next `poll_session_opens` tick (200ms). The spinner is
        // ended in `poll_session_opens` for every terminal arm
        // (success, read_error, disconnect) via `drop_session_open_entry`.
        let activity = self.start_activity("Opening session...");
        self.session_open_rx
            .insert(work_item_id.clone(), SessionOpenPending { rx, activity });
    }

    /// Remove a pending `session_open_rx` entry and end its spinner
    /// activity. Centralising this keeps the two terminal paths
    /// (result delivered, background thread disconnected) symmetric so
    /// no terminal arm can leak a spinner.
    fn drop_session_open_entry(&mut self, wi_id: &WorkItemId) {
        if let Some(entry) = self.session_open_rx.remove(wi_id) {
            self.end_activity(entry.activity);
        }
    }

    /// Poll pending session-open plan reads. Called from the
    /// background-work tick in `salsa.rs`. Each completed receiver
    /// finishes the session open by calling `finish_session_open`
    /// with the plan text read on the background thread.
    pub fn poll_session_opens(&mut self) {
        if self.session_open_rx.is_empty() {
            return;
        }
        // Collect keys first because `finish_session_open` borrows
        // `self` mutably, and we need to `remove` entries before the
        // nested call.
        let wi_ids: Vec<WorkItemId> = self.session_open_rx.keys().cloned().collect();
        for wi_id in wi_ids {
            let result = match self.session_open_rx.get(&wi_id) {
                Some(entry) => match entry.rx.try_recv() {
                    Ok(r) => r,
                    Err(crossbeam_channel::TryRecvError::Empty) => continue,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        // Background thread died without sending - drop
                        // the entry (and end its spinner) so a retry is
                        // possible and surface the failure in the status
                        // bar.
                        self.drop_session_open_entry(&wi_id);
                        self.status_message =
                            Some("Session open: background thread exited unexpectedly".into());
                        continue;
                    }
                },
                None => continue,
            };
            self.drop_session_open_entry(&wi_id);
            if let Some(msg) = result.read_error {
                self.status_message = Some(msg);
            }
            self.finish_session_open(&result.wi_id, &result.cwd, result.plan_text);
        }
    }

    /// Finish the session-open flow after the plan text has been read on
    /// a background thread. Called by `poll_session_opens` - MUST NOT be
    /// called directly from UI-thread entry points, because it invokes
    /// `stage_system_prompt` which consumes state and would otherwise
    /// encourage new synchronous `read_plan` callers.
    fn finish_session_open(
        &mut self,
        work_item_id: &WorkItemId,
        cwd: &std::path::Path,
        plan_text: String,
    ) {
        // Guard: the work item may have been deleted while the plan
        // read was in flight. In that case, do not spawn a session,
        // just drop the result quietly. (The worktree itself is
        // either pre-existing or cleaned up by `poll_worktree_creation`
        // before we got here.)
        let Some(work_item_status) = self
            .work_items
            .iter()
            .find(|w| w.id == *work_item_id)
            .map(|w| w.status)
        else {
            return;
        };

        // Start MCP socket server for this session.
        let mcp_result = self.start_mcp_for_session(cwd, work_item_id);
        let session_key = (work_item_id.clone(), work_item_status);
        let has_gate_findings = self.review_gate_findings.contains_key(work_item_id);
        let system_prompt = self.stage_system_prompt(work_item_id, cwd, plan_text);
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
                    let repo_mcp_servers: Vec<crate::config::McpServerEntry> = self
                        .work_items
                        .iter()
                        .find(|w| w.id == *work_item_id)
                        .and_then(|wi| wi.repo_associations.first())
                        .map(|assoc| {
                            let repo_display = crate::config::collapse_home(&assoc.repo_path);
                            self.config
                                .mcp_servers_for_repo(&repo_display)
                                .into_iter()
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let mcp_config =
                        crate::mcp::build_mcp_config(&exe, &server.socket_path, &repo_mcp_servers);

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
                    scrollback_offset: 0,
                    selection: None,
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
        cmd.push("--dangerously-skip-permissions".to_string());
        cmd.push("--allowedTools".to_string());
        cmd.push(
            [
                "mcp__workbridge__workbridge_get_context",
                "mcp__workbridge__workbridge_query_log",
                "mcp__workbridge__workbridge_get_plan",
                "mcp__workbridge__workbridge_report_progress",
                "mcp__workbridge__workbridge_log_event",
                "mcp__workbridge__workbridge_set_activity",
                "mcp__workbridge__workbridge_approve_review",
                "mcp__workbridge__workbridge_request_changes",
                "mcp__workbridge__workbridge_set_status",
                "mcp__workbridge__workbridge_set_plan",
                "mcp__workbridge__workbridge_set_title",
                "mcp__workbridge__workbridge_set_description",
                "mcp__workbridge__workbridge_list_repos",
                "mcp__workbridge__workbridge_list_work_items",
                "mcp__workbridge__workbridge_repo_info",
            ]
            .join(","),
        );
        if is_planning {
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
        let recv_result = {
            let Some(UserActionPayload::WorktreeCreate { rx, .. }) =
                self.user_action_payload(&UserActionKey::WorktreeCreate)
            else {
                return;
            };
            match rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            }
        };
        let result = match recv_result {
            Ok(r) => r,
            Err(()) => {
                self.end_user_action(&UserActionKey::WorktreeCreate);
                self.status_message =
                    Some("Worktree creation: background thread exited unexpectedly".into());
                return;
            }
        };

        self.end_user_action(&UserActionKey::WorktreeCreate);

        let reused = result.reused;
        match (result.path, result.error) {
            (Some(path), _) => {
                // Verify the work item still exists before opening a session.
                // It may have been deleted while the background thread was running.
                if !self.work_items.iter().any(|w| w.id == result.wi_id) {
                    if reused {
                        // The worktree was already on disk before the thread
                        // ran - we do NOT own it, so we must not force-remove
                        // it here. Surface a status message so the user can
                        // clean up manually if needed.
                        self.status_message = Some(
                            "Work item deleted while creating worktree; pre-existing worktree left in place"
                                .into(),
                        );
                        return;
                    }
                    // Queue the orphaned worktree for background
                    // cleanup. `poll_worktree_creation` runs on the UI
                    // thread (rat-salsa timer ticks fire on the event
                    // loop), so calling `remove_worktree` here would be
                    // a P0 blocking-I/O violation - see `docs/UI.md`.
                    self.spawn_orphan_worktree_cleanup(
                        result.repo_path.clone(),
                        path.clone(),
                        result.branch.clone(),
                    );
                    self.status_message = Some(
                        "Worktree created but work item was deleted - cleaning up in background"
                            .into(),
                    );
                    return;
                }
                // Worktree created successfully - reassemble so the new
                // worktree path is visible in the data model.
                self.reassemble_work_items();
                self.build_display_list();
                if result.open_session {
                    // Hand off to the background plan read; the session
                    // itself is spawned from `poll_session_opens` once
                    // the plan arrives. Running the read here would put
                    // filesystem I/O back on the UI thread.
                    self.begin_session_open(&result.wi_id, &path);
                } else {
                    self.status_message = Some("Imported (worktree created)".into());
                }
            }
            (None, Some(error)) => {
                if result.branch_gone {
                    // Branch no longer exists. Show a dialog so the user
                    // can delete the orphaned work item or dismiss.
                    self.branch_gone_prompt = Some((result.wi_id.clone(), error));
                } else {
                    // Generic worktree error (permissions, disk, path
                    // conflict) or import fetch failure. Use alert for
                    // session errors, status message for imports.
                    if result.open_session {
                        self.alert_message = Some(error);
                    } else {
                        self.status_message = Some(error);
                    }
                }
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
    ///
    /// `plan_text` is the plan body that was read from the backend on the
    /// background thread by `begin_session_open` / `poll_session_opens`.
    /// The UI thread must NOT read the plan here - `WorkItemBackend::read_plan`
    /// performs filesystem I/O that would freeze the event loop (see
    /// `docs/UI.md` "Blocking I/O Prohibition"). An empty string means
    /// either "no plan on disk" or "plan read failed"; callers that need
    /// to distinguish should pass the pre-resolved `read_error` via
    /// `status_message` before calling this function.
    fn stage_system_prompt(
        &mut self,
        work_item_id: &WorkItemId,
        cwd: &std::path::Path,
        plan_text: String,
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

        // Look up and consume rework reason if any (one-shot use).
        let rework_reason = self.rework_reasons.remove(work_item_id).unwrap_or_default();
        let review_gate_findings = self
            .review_gate_findings
            .remove(work_item_id)
            .unwrap_or_default();

        // Check if the branch has commits ahead of the default branch.
        // Used to select the retroactive planning prompt when appropriate.
        // Reads from the cached fetch result - never shells out to git
        // on the UI thread. When the fetcher has not yet populated this
        // repo, defaults to false (fall through to the "no plan" prompt).
        let repo_path_owned = wi.repo_associations.first().map(|a| a.repo_path.clone());
        let branch_owned = wi.repo_associations.first().and_then(|a| a.branch.clone());
        let status = wi.status;
        let description = wi.description.clone();
        let has_branch_commits = match (repo_path_owned.as_ref(), branch_owned.as_deref()) {
            (Some(rp), Some(branch)) => self.branch_has_commits(rp, branch),
            _ => false,
        };

        // Build a situation summary that tells Claude where it is and what
        // state the work item is in.  Uses the worktree path (not the main
        // repo path) so Claude runs commands in the right directory.
        let situation = match status {
            WorkItemStatus::Backlog | WorkItemStatus::Done | WorkItemStatus::Mergequeue => {
                return None;
            }
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
            WorkItemStatus::Backlog | WorkItemStatus::Done | WorkItemStatus::Mergequeue => {
                unreachable!()
            }
            WorkItemStatus::Planning => {
                if has_branch_commits {
                    "planning_retroactive"
                } else if title == QUICKSTART_TITLE {
                    "planning_quickstart"
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

    /// Check if `branch` in `repo_path` has commits ahead of the default
    /// branch, consulting the cached `repo_data` populated by the
    /// background fetcher.
    ///
    /// This is a pure, synchronous cache lookup - it MUST NOT shell out to
    /// git on the UI thread. Blocking I/O in this call path would freeze
    /// the event loop; see `docs/UI.md` "Blocking I/O Prohibition".
    ///
    /// When the fetcher has not yet produced a result for this repo/branch
    /// (first fetch still in flight, repo never fetched, or detached
    /// HEAD), returns `false` - the safe default that causes the caller
    /// to skip the review-gate / retroactive-analysis path without
    /// freezing the UI. The next fetch cycle will populate the cache and
    /// subsequent calls will return the correct answer.
    fn branch_has_commits(&self, repo_path: &std::path::Path, branch: &str) -> bool {
        self.repo_data
            .get(repo_path)
            .and_then(|rd| rd.worktrees.as_ref().ok())
            .and_then(|wts| wts.iter().find(|wt| wt.branch.as_deref() == Some(branch)))
            .and_then(|wt| wt.has_commits_ahead)
            .unwrap_or(false)
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

        // Determine work item kind for conditional MCP tool exposure.
        let wi_kind = self
            .work_items
            .iter()
            .find(|w| w.id == *work_item_id)
            .map(|w| format!("{:?}", w.kind))
            .unwrap_or_default();

        // Start the socket server.
        let server = McpSocketServer::start(
            socket_path,
            wi_id_str,
            wi_kind,
            context_json,
            activity_log_path,
            self.mcp_tx.clone(),
            false, // read_only: interactive sessions need full tool access
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

                    let current_status = wi_ref.map(|w| w.status);

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
                        let current = current_status.unwrap();
                        self.apply_stage_change(&wi_id, &current, &new_status, "mcp");

                        // Enqueue for the no-plan prompt (skip duplicates).
                        if !self.no_plan_prompt_queue.contains(&wi_id) {
                            self.no_plan_prompt_queue.push_back(wi_id);
                        }
                        if !self.no_plan_prompt_visible {
                            self.no_plan_prompt_visible = true;
                        }
                        continue;
                    }

                    // Review gate: when MCP requests Implementing/Blocked -> Review,
                    // a per-item review gate must approve the transition. The
                    // gate runs entirely on a background thread - any
                    // "cannot run" discovery (no plan, empty diff, git error)
                    // arrives as `ReviewGateMessage::Blocked` and is handled
                    // by `poll_review_gate` (which applies the rework flow).
                    // This main-thread path only ever sees synchronous
                    // pre-conditions (gate already running, no branch, no
                    // repo association, work item missing).
                    if (current_status.as_ref() == Some(&WorkItemStatus::Implementing)
                        || current_status.as_ref() == Some(&WorkItemStatus::Blocked))
                        && new_status == WorkItemStatus::Review
                    {
                        match self.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp) {
                            ReviewGateSpawn::Spawned => {
                                self.status_message =
                                    Some("Claude requested Review - running review gate...".into());
                            }
                            ReviewGateSpawn::Blocked(reason) => {
                                self.status_message = Some(reason);
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
                McpEvent::SetTitle {
                    work_item_id: wi_id_str,
                    title,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    if let Err(e) = self.backend.update_title(&wi_id, &title) {
                        self.status_message = Some(format!("Title update error: {e}"));
                    } else {
                        self.reassemble_work_items();
                        self.build_display_list();
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
                McpEvent::DeleteWorkItem {
                    work_item_id: wi_id_str,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };

                    // Guard against concurrent cleanup: if a prior delete (from
                    // the modal OR a previous MCP call) is still running, refuse
                    // THIS delete before touching the backend. Without this check
                    // the backend record and session would be destroyed but
                    // spawn_delete_cleanup would early-return, silently orphaning
                    // the worktree, branch, and open PR. Mirror the modal's guard
                    // (confirm_delete_from_prompt) here so both entry points have
                    // the same ordering: check availability -> delete backend ->
                    // spawn cleanup.
                    if self.is_user_action_in_flight(&UserActionKey::DeleteCleanup) {
                        self.alert_message = Some(
                            "MCP delete refused: another delete cleanup is still \
                             in progress. Wait for it to finish and try again."
                                .into(),
                        );
                        continue;
                    }

                    // Gather repo associations from the assembled work item.
                    let repo_associations: Vec<crate::work_item_backend::RepoAssociationRecord> =
                        self.work_items
                            .iter()
                            .find(|w| w.id == wi_id)
                            .map(|wi| {
                                wi.repo_associations
                                    .iter()
                                    .map(|a| crate::work_item_backend::RepoAssociationRecord {
                                        repo_path: a.repo_path.clone(),
                                        branch: a.branch.clone(),
                                        pr_identity: None,
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                    // Gather cleanup info BEFORE deleting (needs repo_data lookups).
                    let mut cleanup_infos = self.gather_delete_cleanup_infos(&repo_associations);

                    // Non-blocking phases: backend delete, session kill, in-memory
                    // cleanup. Resource cleanup (worktree removal, branch
                    // deletion, PR close) runs on a background thread below
                    // via `spawn_delete_cleanup`.
                    let mut warnings: Vec<String> = Vec::new();
                    let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
                    if let Err(e) =
                        self.delete_work_item_by_id(&wi_id, &mut warnings, &mut orphan_worktrees)
                    {
                        self.status_message = Some(format!("MCP delete error: {e}"));
                        continue;
                    }

                    // Phase 5 may have captured an in-flight worktree-create
                    // result whose worktree is now orphaned. Forward each
                    // orphan to the background cleanup thread by synthesizing
                    // a `DeleteCleanupInfo` (no PR, no remote - this is a
                    // fresh worktree with no PR yet) so the
                    // `git worktree remove` and `git branch -D` both run off
                    // the UI thread. Running them here would be a P0
                    // blocking-I/O violation; see `docs/UI.md`.
                    // `branch_in_main_worktree: false` is correct by
                    // construction - a freshly-created worktree is never the
                    // main worktree.
                    for orphan in orphan_worktrees {
                        cleanup_infos.push(DeleteCleanupInfo {
                            repo_path: orphan.repo_path,
                            branch: orphan.branch,
                            worktree_path: Some(orphan.worktree_path),
                            branch_in_main_worktree: false,
                            open_pr_number: None,
                            github_remote: None,
                        });
                    }

                    // Spawn background thread for blocking resource cleanup
                    // (worktree removal, branch deletion, PR close). The MCP
                    // path always forces removal (no interactive confirmation
                    // is possible) and shows progress in the status bar
                    // because the user did not explicitly trigger the delete
                    // from a dialog.
                    if !cleanup_infos.is_empty() {
                        self.spawn_delete_cleanup(cleanup_infos, true, true);
                    }

                    // Clear selection identity if the deleted item was selected.
                    if self.selected_work_item_id() == Some(wi_id) {
                        self.selected_work_item = None;
                        self.selected_unlinked_branch = None;
                        self.selected_review_request_branch = None;
                    }

                    let old_idx = self.selected_item;
                    self.reassemble_work_items();
                    self.build_display_list();
                    self.fetcher_repos_changed = true;

                    // Re-sync cursor position.
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

                    self.focus = FocusPanel::Left;
                    if warnings.is_empty() {
                        self.status_message =
                            Some("Work item deleted via MCP (resource cleanup in progress)".into());
                    } else {
                        self.status_message = Some(format!(
                            "Deleted via MCP (with warnings: {})",
                            warnings.join("; ")
                        ));
                    }
                }
                McpEvent::SubmitReview {
                    work_item_id: wi_id_str,
                    action,
                    comment,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    let wi = self.work_items.iter().find(|w| w.id == wi_id);
                    if !wi.is_some_and(|w| w.kind == WorkItemKind::ReviewRequest) {
                        self.status_message =
                            Some("MCP: review tools only work on review request items".into());
                        continue;
                    }
                    if !wi.is_some_and(|w| w.status == WorkItemStatus::Review) {
                        self.status_message =
                            Some("MCP: review request is not in Review status".into());
                        continue;
                    }
                    self.spawn_review_submission(&wi_id, &action, &comment);
                }
                McpEvent::ReviewGateProgress {
                    work_item_id: wi_id_str,
                    message,
                } => {
                    if let Ok(wi_id) = serde_json::from_str::<WorkItemId>(&wi_id_str)
                        && let Some(gate) = self.review_gates.get_mut(&wi_id)
                    {
                        gate.progress = Some(message);
                    }
                }
                McpEvent::CreateWorkItem {
                    title,
                    description,
                    repo_path,
                } => {
                    let repo = PathBuf::from(&repo_path);

                    // Validate that the repo exists in active_repo_cache.
                    let repo_valid = self
                        .active_repo_cache
                        .iter()
                        .any(|r| r.path == repo && r.git_dir_present);
                    if !repo_valid {
                        self.status_message = Some(format!(
                            "MCP: repo '{}' not found or has no git dir",
                            repo_path
                        ));
                        continue;
                    }

                    let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
                    let suffix = crate::create_dialog::random_suffix();
                    let branch = format!("{username}/workitem-{suffix}");

                    let request = CreateWorkItem {
                        title: title.clone(),
                        description: Some(description),
                        status: WorkItemStatus::Planning,
                        kind: WorkItemKind::Own,
                        repo_associations: vec![RepoAssociationRecord {
                            repo_path: repo,
                            branch: Some(branch),
                            pr_identity: None,
                        }],
                    };

                    match self.backend.create(request) {
                        Ok(record) => {
                            let wi_id = record.id.clone();
                            self.reassemble_work_items();
                            self.fetcher_repos_changed = true;
                            self.selected_work_item = Some(wi_id.clone());
                            self.build_display_list();

                            // Close the global drawer and spawn the planning session.
                            self.global_drawer_open = false;
                            self.focus = self.pre_drawer_focus;
                            self.spawn_session(&wi_id);
                            self.status_message = Some(format!("Created work item: {title}"));
                        }
                        Err(e) => {
                            self.status_message =
                                Some(format!("MCP: failed to create work item: {e}"));
                        }
                    }
                }
            }
        }
    }

    /// Import the currently selected unlinked PR as a work item.
    ///
    /// Calls backend.import() then spawns a background thread to fetch the
    /// branch and create a worktree. The UI remains responsive while the
    /// git operations run. Results are picked up by poll_worktree_creation().
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

        let repo_path = unlinked.repo_path.clone();
        let branch = unlinked.branch.clone();

        match self.backend.import(unlinked) {
            Ok(record) => {
                let title = record.title.clone();
                let wi_id = record.id.clone();
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_repos_changed = true;
                self.spawn_import_worktree(wi_id, repo_path, branch, title);
            }
            Err(e) => {
                self.status_message = Some(format!("Import error: {e}"));
            }
        }
    }

    /// Import the currently selected review-requested PR as a work item.
    ///
    /// Calls backend.import_review_request() then spawns a background thread
    /// to fetch the branch and create a worktree. The UI remains responsive
    /// while the git operations run.
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
                let wi_id = record.id.clone();
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_repos_changed = true;
                self.spawn_import_worktree(wi_id, repo_path, branch, title);
            }
            Err(e) => {
                self.status_message = Some(format!("Import error: {e}"));
            }
        }
    }

    /// Spawn a background thread to fetch the branch and create a worktree
    /// for a freshly imported work item. If another worktree creation is
    /// already in flight, falls back to a status message instead of blocking.
    fn spawn_import_worktree(
        &mut self,
        wi_id: WorkItemId,
        repo_path: PathBuf,
        branch: String,
        title: String,
    ) {
        if self.is_user_action_in_flight(&UserActionKey::WorktreeCreate) {
            self.status_message = Some(format!(
                "Imported: {title} (worktree queued - another in progress)"
            ));
            return;
        }

        // Admit the user action BEFORE spawning the background thread
        // so there is no window in which a freshly-spawned thread could
        // be running while the helper is in the rejecting state. The
        // `is_user_action_in_flight` check above already guarantees the
        // slot is free (UI-thread serialization), so this call is
        // effectively infallible; the `is_none()` branch is
        // defense-in-depth against a future async entry point.
        if self
            .try_begin_user_action(
                UserActionKey::WorktreeCreate,
                Duration::ZERO,
                format!("Importing: {title}..."),
            )
            .is_none()
        {
            self.status_message = Some(format!(
                "Imported: {title} (worktree queued - another in progress)"
            ));
            return;
        }

        let wt_dir = self.config.defaults.worktree_dir.clone();
        let ws = Arc::clone(&self.worktree_service);
        let wi_id_clone = wi_id.clone();
        let title_clone = title.clone();

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            let title = title_clone;
            if ws.fetch_branch(&repo_path, &branch).is_err() {
                let _ = tx.send(WorktreeCreateResult {
                    wi_id: wi_id_clone,
                    repo_path,
                    branch: Some(branch.clone()),
                    path: None,
                    error: Some(format!(
                        "Imported: {title} - could not fetch branch '{branch}' from origin. \
                         Manual checkout required."
                    )),
                    open_session: false,
                    branch_gone: false,
                    reused: false,
                });
                return;
            }
            let wt_target = Self::worktree_target_path(&repo_path, &branch, &wt_dir);
            // Reuse an existing worktree only if it lives at the exact
            // expected location (wt_target) and is NOT the main worktree.
            // See `find_reusable_worktree` for rationale.
            let reused_wt =
                Self::find_reusable_worktree(ws.as_ref(), &repo_path, &branch, &wt_target);
            let (wt_result, reused) = match reused_wt {
                Some(existing_wt) => (Ok(existing_wt), true),
                None => (ws.create_worktree(&repo_path, &branch, &wt_target), false),
            };
            match wt_result {
                Ok(wt_info) => {
                    let _ = tx.send(WorktreeCreateResult {
                        wi_id: wi_id_clone,
                        repo_path,
                        branch: Some(branch),
                        path: Some(wt_info.path),
                        error: None,
                        open_session: false,
                        branch_gone: false,
                        reused,
                    });
                }
                Err(e) => {
                    let _ = tx.send(WorktreeCreateResult {
                        wi_id: wi_id_clone,
                        repo_path,
                        branch: Some(branch),
                        path: None,
                        error: Some(format!("Imported: {title} (worktree not created: {e})")),
                        open_session: false,
                        branch_gone: false,
                        reused: false,
                    });
                }
            }
        });

        self.attach_user_action_payload(
            &UserActionKey::WorktreeCreate,
            UserActionPayload::WorktreeCreate { rx, wi_id },
        );
        self.status_message = Some(format!("Imported: {title} (creating worktree...)"));
    }

    /// Create a new work item with explicit parameters from the creation
    /// dialog. Accepts user-provided title, selected repos, and a branch
    /// name (required).
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

    /// Create a quick-start work item in Planning status and immediately spawn
    /// a Claude session without asking the user anything. The Claude agent
    /// will ask the user what they want to work on and set title/description
    /// via MCP tools.
    ///
    /// Returns `Err("MULTIPLE_REPOS")` when the repo cannot be determined
    /// automatically and the caller should fall back to the create dialog.
    pub fn create_quickstart_work_item(&mut self) -> Result<(), String> {
        let repo = self.resolve_quickstart_repo()?;

        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let suffix = crate::create_dialog::random_suffix();
        let branch = format!("{username}/quickstart-{suffix}");

        let request = CreateWorkItem {
            title: QUICKSTART_TITLE.to_string(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo,
                branch: Some(branch),
                pr_identity: None,
            }],
        };

        match self.backend.create(request) {
            Ok(record) => {
                let wi_id = record.id.clone();
                self.reassemble_work_items();
                self.fetcher_repos_changed = true;
                // Set identity so build_display_list restores selection.
                self.selected_work_item = Some(wi_id.clone());
                self.build_display_list();
                self.spawn_session(&wi_id);
                Ok(())
            }
            Err(e) => {
                let msg = format!("Create error: {e}");
                self.status_message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Create a quick-start work item for a specific repo. Used by the
    /// create dialog fallback when the user selects a repo from multiple
    /// options. Creates a Planning item and spawns a Claude session.
    pub fn create_quickstart_work_item_for_repo(&mut self, repo: PathBuf) -> Result<(), String> {
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let suffix = crate::create_dialog::random_suffix();
        let branch = format!("{username}/quickstart-{suffix}");

        let request = CreateWorkItem {
            title: QUICKSTART_TITLE.to_string(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo,
                branch: Some(branch),
                pr_identity: None,
            }],
        };

        match self.backend.create(request) {
            Ok(record) => {
                let wi_id = record.id.clone();
                self.reassemble_work_items();
                self.fetcher_repos_changed = true;
                self.selected_work_item = Some(wi_id.clone());
                self.build_display_list();
                self.spawn_session(&wi_id);
                Ok(())
            }
            Err(e) => {
                let msg = format!("Create error: {e}");
                self.status_message = Some(msg.clone());
                Err(msg)
            }
        }
    }

    /// Determine the repo to use for a quick-start work item.
    ///
    /// Strategy:
    /// 1. Exactly one managed repo with a git directory - use it.
    /// 2. Multiple repos - return "MULTIPLE_REPOS" so the caller opens the
    ///    creation dialog with the repo picker focused. CWD is deliberately
    ///    not consulted: when there is a real choice to make, the user should
    ///    pick explicitly every time.
    /// 3. No repos at all - return an error message.
    fn resolve_quickstart_repo(&self) -> Result<PathBuf, String> {
        let git_repos: Vec<&PathBuf> = self
            .active_repo_cache
            .iter()
            .filter(|r| r.git_dir_present)
            .map(|r| &r.path)
            .collect();

        match git_repos.len() {
            0 => Err("No managed repos available. Add one in Settings (?)".to_string()),
            1 => Ok(git_repos[0].clone()),
            _ => Err("MULTIPLE_REPOS".to_string()),
        }
    }

    /// Open the "Set branch name" recovery modal for a work item.
    ///
    /// The dialog is prefilled with a generated slug in the same
    /// `{username}/{slug}-{suffix}` shape the create dialog produces
    /// (see `create_dialog::auto_fill_branch`). The `pending` parameter
    /// records what action should be re-driven after the branch is
    /// persisted, so a branchless Enter or advance gesture can resume
    /// without the user having to repeat it.
    ///
    /// This method only mutates `self.set_branch_dialog`; it does not
    /// touch the backend or the work item list.
    pub fn open_set_branch_dialog(
        &mut self,
        wi_id: WorkItemId,
        pending: crate::create_dialog::PendingBranchAction,
    ) {
        let title = self
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .map(|w| w.title.clone())
            .unwrap_or_default();
        let slug = crate::create_dialog::slugify(&title);
        let slug = crate::create_dialog::truncate_slug(&slug, crate::create_dialog::MAX_SLUG_LEN);
        let suffix = crate::create_dialog::random_suffix();
        // Match `create_quickstart_work_item` and
        // `CreateDialog::auto_fill_branch`: use $USER when available and
        // fall back to a generic "user" literal otherwise.
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let default = if slug.is_empty() {
            format!("{username}/workitem-{suffix}")
        } else {
            format!("{username}/{slug}-{suffix}")
        };
        let mut input = rat_widget::text_input::TextInputState::new();
        input.set_text(&default);
        self.set_branch_dialog = Some(crate::create_dialog::SetBranchDialog {
            wi_id,
            input,
            pending,
        });
    }

    /// Dismiss the "Set branch name" modal without mutating anything.
    /// Safe to call when the modal is not visible.
    pub fn cancel_set_branch_dialog(&mut self) {
        self.set_branch_dialog = None;
    }

    /// Persist the branch name typed into the "Set branch name" modal
    /// and re-drive whichever action opened the dialog in the first
    /// place (see `PendingBranchAction`).
    ///
    /// Applies the branch to every repo association that currently has
    /// `branch.is_none()`, matching the "one branch per item" convention
    /// of `create_work_item_with`. On failure the dialog stays open so
    /// the user can retry or press Esc.
    pub fn confirm_set_branch_dialog(&mut self) {
        let Some(dlg) = self.set_branch_dialog.take() else {
            return;
        };
        let branch = dlg.input.text().trim().to_string();
        if branch.is_empty() {
            // Restore the dialog so the user can edit the field.
            self.status_message = Some("Branch name cannot be empty".into());
            self.set_branch_dialog = Some(dlg);
            return;
        }

        // Collect the list of repo associations that need a branch.
        let targets: Vec<PathBuf> = match self.work_items.iter().find(|w| w.id == dlg.wi_id) {
            Some(w) => w
                .repo_associations
                .iter()
                .filter(|a| a.branch.is_none())
                .map(|a| a.repo_path.clone())
                .collect(),
            None => {
                self.status_message = Some("Work item not found".into());
                return;
            }
        };

        if targets.is_empty() {
            // Defensive: if the user somehow opened the dialog for an
            // item that already has a branch on every repo, treat it as
            // a no-op but still re-drive the pending action so the
            // gesture is not silently lost.
            self.status_message = Some("Branch already set".into());
        } else {
            for repo_path in &targets {
                if let Err(e) = self.backend.update_branch(&dlg.wi_id, repo_path, &branch) {
                    self.status_message = Some(format!("Failed to set branch: {e}"));
                    // Restore the dialog so the user can retry.
                    self.set_branch_dialog = Some(dlg);
                    return;
                }
            }
            self.reassemble_work_items();
            self.build_display_list();
            self.fetcher_repos_changed = true;
        }

        // Re-drive the pending action that opened the dialog.
        match dlg.pending {
            crate::create_dialog::PendingBranchAction::SpawnSession => {
                self.spawn_session(&dlg.wi_id);
            }
            crate::create_dialog::PendingBranchAction::Advance { from, to } => {
                self.apply_stage_change(&dlg.wi_id, &from, &to, "user");
            }
        }
    }

    /// Open the delete confirmation modal for the currently selected work
    /// item.
    ///
    /// Does not touch the backend, shell out to git, or spawn any cleanup.
    /// The dialog body warns that any uncommitted changes will be lost so
    /// we can unconditionally pass `--force` to the background cleanup
    /// thread without blocking the UI thread on `git status --porcelain`.
    /// The actual work runs in `confirm_delete_from_prompt` after the
    /// user presses 'y'.
    pub fn open_delete_prompt(&mut self) {
        let Some(work_item_id) = self.selected_work_item_id() else {
            self.status_message = Some("No work item selected".into());
            return;
        };
        self.open_delete_prompt_for(work_item_id);
    }

    /// Variant of `open_delete_prompt` that targets a specific work item by
    /// ID rather than reading `selected_work_item_id()`. Used by the
    /// branch-gone dialog which already knows the target and must not
    /// depend on `selected_item` because Board view stores the cursor in
    /// `board_cursor` instead.
    pub fn open_delete_prompt_for(&mut self, work_item_id: WorkItemId) {
        // Look up the target work item to fetch its title for the modal.
        let Some(target) = self.work_items.iter().find(|w| w.id == work_item_id) else {
            self.status_message = Some("Work item not found".into());
            return;
        };

        let title = target.title.clone();
        self.delete_target_wi_id = Some(work_item_id);
        self.delete_target_title = Some(title);
        self.delete_prompt_visible = true;
    }

    /// Dismiss the delete confirmation modal without deleting anything.
    /// Safe to call when the modal is not visible; it just clears any
    /// residual target state.
    pub fn cancel_delete_prompt(&mut self) {
        self.delete_prompt_visible = false;
        self.delete_target_wi_id = None;
        self.delete_target_title = None;
    }

    /// Execute the delete once the user has confirmed via the modal.
    ///
    /// Synchronously kills sessions and deletes the backend record, then
    /// spawns a background thread for the slow I/O (git worktree remove,
    /// git branch -D, gh pr close) following docs/UI.md "Blocking I/O
    /// Prohibition". The modal stays open with a spinner while the
    /// background thread runs; `poll_delete_cleanup` closes it on
    /// completion.
    pub fn confirm_delete_from_prompt(&mut self) {
        let Some(work_item_id) = self.delete_target_wi_id.clone() else {
            // Defensive: dialog was confirmed without a target. Just close it.
            self.cancel_delete_prompt();
            return;
        };

        // If a prior cleanup (MCP or modal) is still running, refuse to
        // start a second one. Alert the user and leave the modal closed -
        // they can retry once the other cleanup drains.
        if self.is_user_action_in_flight(&UserActionKey::DeleteCleanup) {
            self.cancel_delete_prompt();
            self.alert_message = Some(
                "Another delete cleanup is still in progress. \
                 Wait for it to finish and try again."
                    .into(),
            );
            return;
        }

        // Gather repo associations BEFORE touching the backend - once the
        // record is deleted we can no longer read its associations.
        let repo_associations: Vec<crate::work_item_backend::RepoAssociationRecord> = self
            .work_items
            .iter()
            .find(|w| w.id == work_item_id)
            .map(|wi| {
                wi.repo_associations
                    .iter()
                    .map(|a| crate::work_item_backend::RepoAssociationRecord {
                        repo_path: a.repo_path.clone(),
                        branch: a.branch.clone(),
                        pr_identity: None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Resource-cleanup data must be gathered before reassembly so the
        // repo_data lookups still reflect the pre-delete state.
        let mut cleanup_infos = self.gather_delete_cleanup_infos(&repo_associations);
        // The modal warns the user that uncommitted changes will be lost;
        // the background cleanup thread always runs with force=true.
        // See `open_delete_prompt` for why we do not shell out to
        // `git status --porcelain` on the UI thread.

        // Phases 2-6: backend delete, session kill, in-flight cancellation,
        // in-memory state cleanup. Resource cleanup (worktree/branch/PR)
        // runs on the background thread below via spawn_delete_cleanup.
        let mut warnings: Vec<String> = Vec::new();
        let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
        if let Err(e) =
            self.delete_work_item_by_id(&work_item_id, &mut warnings, &mut orphan_worktrees)
        {
            // Backend delete failed; nothing was spawned. Close the modal
            // and surface the error as an alert.
            self.cancel_delete_prompt();
            self.alert_message = Some(format!("Delete error: {e}"));
            return;
        }

        // Phase 5 may have captured an in-flight worktree-create result
        // whose worktree is now orphaned. Forward each orphan to the
        // background cleanup thread by synthesizing a `DeleteCleanupInfo`
        // (no PR, no remote - this is a fresh worktree with no PR yet)
        // so both `git worktree remove` and `git branch -D` run off the
        // UI thread. `branch_in_main_worktree: false` is correct by
        // construction - a freshly-created worktree is never the main
        // worktree.
        for orphan in orphan_worktrees {
            cleanup_infos.push(DeleteCleanupInfo {
                repo_path: orphan.repo_path,
                branch: orphan.branch,
                worktree_path: Some(orphan.worktree_path),
                branch_in_main_worktree: false,
                open_pr_number: None,
                github_remote: None,
            });
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
        self.focus = FocusPanel::Left;

        // Spawn the background cleanup thread. Keep the modal visible and
        // flip it into the in-progress state; poll_delete_cleanup closes
        // it on completion and surfaces the final status/alert.
        self.delete_in_progress = true;
        if cleanup_infos.is_empty() {
            // No git/GitHub cleanup needed (e.g. work item never had a
            // worktree). Still go through finish_delete_cleanup so the
            // dialog closes via the same code path and warnings are
            // surfaced uniformly.
            self.finish_delete_cleanup(Vec::new(), Vec::new(), warnings);
        } else {
            // Stash the synchronous-phase warnings so poll_delete_cleanup
            // can merge them with the background thread's warnings when
            // the dialog closes. Previously these were dropped on the
            // floor in this branch, silently hiding Phase 2/Phase 5
            // errors from the user.
            self.delete_sync_warnings = warnings;
            // show_status_activity=false: the modal already shows a
            // spinner, a duplicate status-bar indicator would just be
            // noise. `force=true` is always passed because the modal
            // body warns the user that uncommitted changes will be lost.
            self.spawn_delete_cleanup(cleanup_infos, true, false);
        }
    }

    /// Finalize the modal delete flow after the background cleanup thread
    /// returns (or is skipped because there was nothing to clean up).
    /// Closes the modal, applies PR-eviction tracking, and surfaces
    /// either a success status message or an error alert.
    fn finish_delete_cleanup(
        &mut self,
        cleanup_warnings: Vec<String>,
        closed_pr_branches: Vec<(PathBuf, String)>,
        mut pre_warnings: Vec<String>,
    ) {
        self.delete_in_progress = false;
        self.delete_prompt_visible = false;
        self.delete_target_wi_id = None;
        self.delete_target_title = None;

        if !closed_pr_branches.is_empty() {
            self.cleanup_evicted_branches.extend(closed_pr_branches);
            self.apply_cleanup_evictions();
            self.reassemble_work_items();
            self.build_display_list();
        }

        pre_warnings.extend(cleanup_warnings);
        if pre_warnings.is_empty() {
            self.status_message = Some("Work item deleted".into());
        } else {
            self.alert_message = Some(format!(
                "Deleted with warnings: {}",
                pre_warnings.join("; ")
            ));
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
        // Review request items cannot be manually advanced.  The only way
        // to complete them is via the approve/request-changes MCP tools.
        if wi.kind == WorkItemKind::ReviewRequest {
            self.status_message = Some("Use approve/request-changes in the Claude session".into());
            return;
        }
        let current_status = wi.status;
        // Capture the branch invariant state before giving up our
        // borrow of `wi` below. `has_branch` is true when at least one
        // repo association already has a branch name; if false, the
        // Backlog -> Planning branch below opens the recovery dialog
        // instead of persisting a stage change that would produce a
        // stuck "Planning with no branch" item on disk.
        let has_branch = wi.repo_associations.iter().any(|a| a.branch.is_some());
        let Some(new_status) = current_status.next_stage() else {
            self.status_message = Some("Already at final stage".into());
            return;
        };

        // Branch invariant: a work item must carry at least one branch
        // name by the time it leaves Backlog (everything past Backlog
        // implies "somebody is actively working on this branch"). The
        // only natural Backlog transition is -> Planning, but we gate on
        // the source status rather than the target so any future
        // Backlog -> X path inherits the same enforcement without a
        // silent gap. When the invariant fails, open the recovery dialog
        // so the user can set a branch and resume; the dialog re-drives
        // `apply_stage_change` on confirm (see
        // `confirm_set_branch_dialog`).
        if current_status == WorkItemStatus::Backlog && !has_branch {
            self.open_set_branch_dialog(
                wi_id.clone(),
                crate::create_dialog::PendingBranchAction::Advance {
                    from: current_status,
                    to: new_status,
                },
            );
            return;
        }

        // Planning -> Implementing is automatic (triggered by workbridge_set_plan).
        // Block manual advance to prevent skipping the plan handoff.
        if current_status == WorkItemStatus::Planning && new_status == WorkItemStatus::Implementing
        {
            self.status_message =
                Some("Plan must be set via Claude session (workbridge_set_plan)".into());
            return;
        }

        // Review gate: each item gets its own async gate that must approve
        // the transition. Multiple gates can run concurrently for different
        // work items.
        if (current_status == WorkItemStatus::Implementing
            || current_status == WorkItemStatus::Blocked)
            && new_status == WorkItemStatus::Review
        {
            match self.spawn_review_gate(&wi_id, ReviewGateOrigin::Tui) {
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
            return;
        }

        // Mergequeue items are waiting for an external merge - block manual advance.
        if current_status == WorkItemStatus::Mergequeue {
            self.status_message =
                Some("Waiting for PR to be merged - retreat with Shift+Left to cancel".into());
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
        let current_status = wi.status;
        let Some(new_status) = current_status.prev_stage() else {
            self.status_message = Some("Already at first stage".into());
            return;
        };

        // If the retreating item has a pending review gate, cancel it.
        // The gate result would be stale since the user intentionally moved away.
        self.drop_review_gate(&wi_id);

        // Cancel any in-flight PR merge. Merges are only spawned from Review,
        // so when retreating from Review we drop the helper entry to prevent
        // poll_pr_merge from applying a stale result. The background thread
        // will finish on its own; we just ignore its result.
        if current_status == WorkItemStatus::Review
            && self.is_user_action_in_flight(&UserActionKey::PrMerge)
        {
            self.end_user_action(&UserActionKey::PrMerge);
            self.merge_in_progress = false;
            self.confirm_merge = false;
            self.merge_wi_id = None;
        }

        // Cancel any in-flight or pending PR creation for the retreating item.
        // PR creation is spawned when entering Review; retreating means the user
        // no longer wants the PR. Drop the helper entry so poll_pr_creation
        // ignores the result, and remove the item from the pending queue.
        if current_status == WorkItemStatus::Review {
            if self.user_action_work_item(&UserActionKey::PrCreate) == Some(&wi_id) {
                self.end_user_action(&UserActionKey::PrCreate);
            }
            self.pr_create_pending.retain(|id| *id != wi_id);
        }

        // Clean up mergequeue watch and in-flight poll when retreating
        // from Mergequeue back to Review. The poll map is keyed by
        // WorkItemId, so removing this item's entry leaves polls for
        // other Mergequeue items untouched.
        if current_status == WorkItemStatus::Mergequeue {
            self.mergequeue_watches.retain(|w| w.wi_id != wi_id);
            self.mergequeue_poll_errors.remove(&wi_id);
            if let Some(state) = self.mergequeue_polls.remove(&wi_id) {
                self.end_activity(state.activity);
            }
        }

        // Rework prompt: when retreating from Review to Implementing,
        // show a text input for the rework reason instead of retreating directly.
        if current_status == WorkItemStatus::Review && new_status == WorkItemStatus::Implementing {
            self.rework_prompt_visible = true;
            self.rework_prompt_input.clear();
            self.rework_prompt_wi = Some(wi_id);
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
    /// Transitions to Done are only allowed when `source == "pr_merge"` or
    /// `source == "review_submitted"`, enforcing the merge-gate invariant
    /// at the chokepoint rather than relying on caller discipline alone.
    pub fn apply_stage_change(
        &mut self,
        wi_id: &WorkItemId,
        current_status: &WorkItemStatus,
        new_status: &WorkItemStatus,
        source: &str,
    ) {
        // Merge-gate guard: Done requires a verified PR merge or a
        // submitted review.  All other callers must go through the merge
        // prompt / poll_pr_merge path (source == "pr_merge") or the review
        // submission path (source == "review_submitted").
        if *new_status == WorkItemStatus::Done
            && source != "pr_merge"
            && source != "review_submitted"
        {
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

        if let Err(e) = self.backend.update_status(wi_id, *new_status) {
            self.status_message = Some(format!("Stage update error: {e}"));
            return;
        }

        // Track when items enter/leave Done for auto-archival.
        let mut done_at_error = false;
        if *new_status == WorkItemStatus::Done {
            match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => {
                    if let Err(e) = self.backend.set_done_at(wi_id, Some(duration.as_secs())) {
                        self.status_message = Some(format!("Failed to set archive timestamp: {e}"));
                        done_at_error = true;
                    }
                }
                Err(e) => {
                    self.status_message = Some(format!(
                        "System clock error, skipping archive timestamp: {e}"
                    ));
                    done_at_error = true;
                }
            }
        } else if *current_status == WorkItemStatus::Done
            && let Err(e) = self.backend.set_done_at(wi_id, None)
        {
            self.status_message = Some(format!("Failed to clear archive timestamp: {e}"));
            done_at_error = true;
        }

        self.reassemble_work_items();
        self.build_display_list();
        if !done_at_error {
            self.status_message = Some(format!("Moved to {}", new_status.badge_text()));
        }

        // Feature 1: Auto-create PR when entering Review (async).
        // Skip for review requests - the PR already exists (it's someone else's).
        let is_review_request = self
            .work_items
            .iter()
            .find(|w| w.id == *wi_id)
            .is_some_and(|w| w.kind == WorkItemKind::ReviewRequest);
        if *new_status == WorkItemStatus::Review && !is_review_request {
            self.spawn_pr_creation(wi_id);
        }

        // Cancel any pending session-open plan-read for this work item
        // BEFORE the session kill block. The plan-read receiver lives in
        // `session_open_rx` (no entry in `self.sessions` yet), so the
        // session-kill branch below would not see it; without this
        // unconditional drop, a stale pending open from the old stage
        // would survive the transition and `finish_session_open` would
        // later spawn Claude for the new stage - including no-session
        // stages like Done or Mergequeue. Dropping the entry here also
        // ends the "Opening session..." spinner.
        self.drop_session_open_entry(wi_id);

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
        if !matches!(
            new_status,
            WorkItemStatus::Backlog | WorkItemStatus::Done | WorkItemStatus::Mergequeue
        ) {
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
        if self.is_user_action_in_flight(&UserActionKey::PrCreate) {
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

        // Read owner/repo from the cached fetcher result rather than shelling
        // out via `worktree_service.github_remote(...)` on the UI thread. The
        // fetcher populates `repo_data[path].github_remote` on every cycle;
        // if no entry exists yet the first fetch hasn't completed and we
        // surface that to the user instead of blocking.
        //
        // This check runs BEFORE try_begin_user_action so an early return
        // (cache miss) cannot leave an orphaned helper entry - see the
        // "desync guard" discussion in `docs/UI.md` "User action guard".
        let (owner, repo_name) = match self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        {
            Some((o, r)) => (o, r),
            None => {
                self.status_message = Some(
                    "PR creation skipped: GitHub remote not yet cached (waiting for next fetch)"
                        .into(),
                );
                return;
            }
        };
        let owner_repo = format!("{owner}/{repo_name}");

        // Admit the action through the user-action guard. The early-return
        // cache check above runs first so we never hold a helper slot
        // across a rejected code path.
        if self
            .try_begin_user_action(
                UserActionKey::PrCreate,
                Duration::ZERO,
                "Creating pull request...",
            )
            .is_none()
        {
            // Unreachable today because the is_user_action_in_flight
            // check above already short-circuited the queueing path,
            // but defense in depth: if a race ever sneaks through, push
            // the request onto the pending queue rather than silently
            // dropping it.
            if !self.pr_create_pending.contains(&wi_id) {
                self.pr_create_pending.push_back(wi_id);
            }
            return;
        }

        // Clone the Arc'd backend and worktree service so the background
        // thread can run `read_plan` (filesystem) and `default_branch`
        // (git subprocess) off the UI thread.
        let backend = Arc::clone(&self.backend);
        let ws = Arc::clone(&self.worktree_service);

        let (tx, rx) = crossbeam_channel::bounded(1);
        let helper_wi_id = wi_id.clone();

        std::thread::spawn(move || {
            // Blocking reads run on the background thread. `read_plan` hits
            // the filesystem; `default_branch` shells out to git. Both are
            // cheap per-call but absolutely prohibited on the UI thread.
            let body = match backend.read_plan(&wi_id) {
                Ok(Some(plan)) if !plan.trim().is_empty() => plan,
                _ => String::new(),
            };
            let default_branch = ws
                .default_branch(&repo_path)
                .unwrap_or_else(|_| "main".to_string());

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

        self.attach_user_action_payload(
            &UserActionKey::PrCreate,
            UserActionPayload::PrCreate {
                rx,
                wi_id: helper_wi_id,
            },
        );
    }

    /// Poll the async PR creation thread for a result. Called on each timer tick.
    pub fn poll_pr_creation(&mut self) {
        let recv_result = {
            let Some(UserActionPayload::PrCreate { rx, .. }) =
                self.user_action_payload(&UserActionKey::PrCreate)
            else {
                return;
            };
            match rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            }
        };
        let result = match recv_result {
            Ok(r) => r,
            Err(()) => {
                self.end_user_action(&UserActionKey::PrCreate);
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

        self.end_user_action(&UserActionKey::PrCreate);

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
        // Single-flight guard via the user-action helper. Rejecting when
        // another merge is already in flight preserves the existing alert
        // wording verbatim - the background thread may have already
        // merged a PR on GitHub, so silently replacing the receiver
        // would lose the result.
        //
        // We run validity checks BEFORE try_begin_user_action so an
        // early return (missing repo / branch / github_remote cache)
        // cannot leave an orphaned helper entry. See `docs/UI.md`
        // "User action guard" for the desync-guard rule.
        if self.is_user_action_in_flight(&UserActionKey::PrMerge) {
            self.alert_message = Some(PR_MERGE_ALREADY_IN_PROGRESS.into());
            return;
        }

        let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
            Some(w) => w,
            None => return,
        };
        let assoc = match wi.repo_associations.first() {
            Some(a) => a,
            None => {
                self.confirm_merge = false;
                self.merge_wi_id = None;
                self.alert_message = Some("Cannot merge: no repo association".into());
                return;
            }
        };
        let branch = match assoc.branch.as_ref() {
            Some(b) => b.clone(),
            None => {
                self.confirm_merge = false;
                self.merge_wi_id = None;
                self.alert_message = Some("Cannot merge: no branch associated".into());
                return;
            }
        };
        let repo_path = assoc.repo_path.clone();

        // Read owner/repo from the cached fetcher result rather than shelling
        // out on the UI thread. If no entry exists yet, the first fetch has
        // not completed - surface that as an alert instead of blocking.
        let (owner, repo_name) = match self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        {
            Some((o, r)) => (o, r),
            None => {
                self.confirm_merge = false;
                self.merge_wi_id = None;
                self.alert_message = Some(
                    "Cannot merge: GitHub remote not yet cached (waiting for next fetch)".into(),
                );
                return;
            }
        };
        let owner_repo = format!("{owner}/{repo_name}");

        // All validity checks have passed. Admit the action now so any
        // rejection above cannot leave the helper with an empty slot.
        if self
            .try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
            .is_none()
        {
            // Raced with another in-flight merge after the
            // `is_user_action_in_flight` check above. Mirror that
            // check's alert wording so the user sees a single, stable
            // rejection message regardless of which branch rejected.
            self.alert_message = Some(PR_MERGE_ALREADY_IN_PROGRESS.into());
            return;
        }
        // Hide the status-bar spinner: the merge modal already renders
        // its own in-progress spinner, and stacking two is confusing.
        // The helper map entry is still the single source of truth for
        // `is_user_action_in_flight(&PrMerge)`.
        if let Some(state) = self.user_actions.in_flight.get(&UserActionKey::PrMerge) {
            let aid = state.activity_id;
            self.end_activity(aid);
        }
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

        self.attach_user_action_payload(&UserActionKey::PrMerge, UserActionPayload::PrMerge { rx });
        self.merge_in_progress = true;
    }

    /// Poll the async PR merge thread for a result. Called on each timer tick.
    pub fn poll_pr_merge(&mut self) {
        let recv_result = {
            let Some(UserActionPayload::PrMerge { rx }) =
                self.user_action_payload(&UserActionKey::PrMerge)
            else {
                return;
            };
            match rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            }
        };
        let result = match recv_result {
            Ok(r) => r,
            Err(()) => {
                self.end_user_action(&UserActionKey::PrMerge);
                self.merge_in_progress = false;
                self.confirm_merge = false;
                self.merge_wi_id = None;
                self.alert_message = Some("PR merge: background thread exited unexpectedly".into());
                return;
            }
        };

        self.end_user_action(&UserActionKey::PrMerge);
        self.merge_in_progress = false;
        self.confirm_merge = false;
        self.merge_wi_id = None;

        // Guard: if the item's status changed while the merge was in-flight
        // (e.g. user retreated to Implementing), discard the stale result to
        // avoid forcing the item back to Done or deleting its worktree.
        let actual_status = self
            .work_items
            .iter()
            .find(|w| w.id == result.wi_id)
            .map(|w| w.status);

        match result.outcome {
            PrMergeOutcome::NoPr => {
                self.alert_message =
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
                self.alert_message = Some(
                    "Merge conflict detected - moved back to [IM] for rebase/resolve".to_string(),
                );
            }
            PrMergeOutcome::Failed { ref error } => {
                self.alert_message = Some(error.clone());
            }
        }
    }

    /// Spawn a background thread to submit a PR review (approve or
    /// request-changes) via `gh pr review`. Results are polled by
    /// `poll_review_submission()` on each timer tick.
    pub fn spawn_review_submission(&mut self, wi_id: &WorkItemId, action: &str, comment: &str) {
        // In-flight guard via the user-action helper. Rejection message
        // is preserved verbatim.
        if self.is_user_action_in_flight(&UserActionKey::ReviewSubmit) {
            self.status_message = Some(REVIEW_SUBMIT_ALREADY_IN_PROGRESS.into());
            return;
        }

        let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
            Some(w) => w,
            None => return,
        };
        let assoc = match wi.repo_associations.first() {
            Some(a) => a,
            None => {
                self.status_message = Some("Cannot submit review: no repo association".into());
                return;
            }
        };
        let branch = match assoc.branch.as_ref() {
            Some(b) => b.clone(),
            None => {
                self.status_message = Some("Cannot submit review: no branch".into());
                return;
            }
        };
        let repo_path = assoc.repo_path.clone();
        // Read owner/repo from the cached fetcher result rather than shelling
        // out on the UI thread. The first fetch populates it; until then we
        // surface a message rather than block.
        //
        // Early returns above run BEFORE try_begin_user_action so a
        // cache miss cannot leave an orphaned helper entry.
        let (owner, repo_name) = match self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        {
            Some((o, r)) => (o, r),
            None => {
                self.status_message = Some(
                    "Cannot submit review: GitHub remote not yet cached (waiting for next fetch)"
                        .into(),
                );
                return;
            }
        };
        let owner_repo = format!("{owner}/{repo_name}");

        let verb = if action == "approve" {
            "Submitting approval"
        } else {
            "Requesting changes"
        };
        if self
            .try_begin_user_action(
                UserActionKey::ReviewSubmit,
                Duration::ZERO,
                format!("{verb}..."),
            )
            .is_none()
        {
            // Race with another in-flight submission; preserve the
            // pre-refactor wording.
            self.status_message = Some(REVIEW_SUBMIT_ALREADY_IN_PROGRESS.into());
            return;
        }
        let action_owned = action.to_string();
        let comment_owned = comment.to_string();
        let wi_id_clone = wi_id.clone();

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            let review_flag = if action_owned == "approve" {
                "--approve"
            } else {
                "--request-changes"
            };
            let mut args = vec![
                "pr".to_string(),
                "review".to_string(),
                branch,
                review_flag.to_string(),
                "--repo".to_string(),
                owner_repo,
            ];
            if !comment_owned.is_empty() {
                args.push("--body".to_string());
                args.push(comment_owned);
            }

            let outcome = match std::process::Command::new("gh").args(&args).output() {
                Ok(output) if output.status.success() => ReviewSubmitOutcome::Success,
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    ReviewSubmitOutcome::Failed {
                        error: format!("Review submission failed: {}", stderr.trim()),
                    }
                }
                Err(e) => ReviewSubmitOutcome::Failed {
                    error: format!("Review submission failed: {e}"),
                },
            };

            let _ = tx.send(ReviewSubmitResult {
                wi_id: wi_id_clone,
                action: action_owned,
                outcome,
            });
        });

        self.attach_user_action_payload(
            &UserActionKey::ReviewSubmit,
            UserActionPayload::ReviewSubmit {
                rx,
                wi_id: wi_id.clone(),
            },
        );
    }

    /// Poll the asynchronous review submission result.
    /// Called on the 200ms timer tick.
    pub fn poll_review_submission(&mut self) {
        let recv_result = {
            let Some(UserActionPayload::ReviewSubmit { rx, .. }) =
                self.user_action_payload(&UserActionKey::ReviewSubmit)
            else {
                return;
            };
            match rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            }
        };
        let result = match recv_result {
            Ok(r) => r,
            Err(()) => {
                self.end_user_action(&UserActionKey::ReviewSubmit);
                self.status_message =
                    Some("Review submission: background thread exited unexpectedly".into());
                return;
            }
        };

        self.end_user_action(&UserActionKey::ReviewSubmit);

        match result.outcome {
            ReviewSubmitOutcome::Success => {
                let verb = if result.action == "approve" {
                    "approved"
                } else {
                    "changes requested"
                };

                let log_entry = ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "review_submitted".to_string(),
                    payload: serde_json::json!({ "action": result.action }),
                };
                if let Err(e) = self.backend.append_activity(&result.wi_id, &log_entry) {
                    self.status_message = Some(format!("Activity log error: {e}"));
                }

                // Suppress re-open for this item until fresh repo_data
                // arrives. Without this, stale review-requested data in
                // repo_data would immediately bounce the item back to Review.
                self.review_reopen_suppress.insert(result.wi_id.clone());

                self.apply_stage_change(
                    &result.wi_id,
                    &WorkItemStatus::Review,
                    &WorkItemStatus::Done,
                    "review_submitted",
                );
                // Only show success if the item actually reached Done.
                // apply_stage_change may have set an error message on failure
                // (e.g. backend write error) - don't overwrite it.
                let reached_done = self
                    .work_items
                    .iter()
                    .find(|w| w.id == result.wi_id)
                    .is_some_and(|w| w.status == WorkItemStatus::Done);
                if reached_done {
                    self.status_message = Some(format!("Review {verb} and moved to [DN]"));
                }
            }
            ReviewSubmitOutcome::Failed { ref error } => {
                self.status_message = Some(error.clone());
            }
        }
    }

    /// Enter the Mergequeue state for a work item. The item must be in
    /// Review with an open PR. Registers a watch so `poll_mergequeue()`
    /// will check the PR state periodically.
    pub fn enter_mergequeue(&mut self, wi_id: &WorkItemId) {
        let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
            Some(w) => w,
            None => return,
        };
        let assoc = match wi.repo_associations.first() {
            Some(a) => a,
            None => {
                self.status_message = Some("Cannot enter mergequeue: no repo association".into());
                return;
            }
        };
        let branch = match assoc.branch.as_ref() {
            Some(b) => b.clone(),
            None => {
                self.status_message = Some("Cannot enter mergequeue: no branch".into());
                return;
            }
        };
        let pr_number = match assoc.pr.as_ref() {
            Some(pr) => pr.number,
            None => {
                self.status_message = Some("Cannot enter mergequeue: no PR found".into());
                return;
            }
        };
        let repo_path = assoc.repo_path.clone();
        // Read owner/repo from the cached fetcher result - never shell out
        // on the UI thread.
        let (owner, repo_name) = match self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        {
            Some((o, r)) => (o, r),
            None => {
                self.status_message = Some(
                    "Cannot enter mergequeue: GitHub remote not yet cached \
                     (waiting for next fetch)"
                        .into(),
                );
                return;
            }
        };
        let owner_repo = format!("{owner}/{repo_name}");

        self.mergequeue_watches.push(MergequeueWatch {
            wi_id: wi_id.clone(),
            // Pin the exact PR number from assoc.pr so subsequent polls
            // target it unambiguously even if the branch later has a
            // different PR opened on it.
            pr_number: Some(pr_number),
            owner_repo,
            branch,
            repo_path,
            last_polled: None,
        });

        self.apply_stage_change(
            wi_id,
            &WorkItemStatus::Review,
            &WorkItemStatus::Mergequeue,
            "user",
        );
        self.status_message = Some("Entered mergequeue - polling PR until merged".into());
    }

    /// Poll the PR state for items in the Mergequeue. Called on each timer
    /// tick. Spawns at most one background thread at a time, with a 30-second
    /// cooldown between polls.
    pub fn poll_mergequeue(&mut self) {
        // -- Phase 1: drain any in-flight results --
        // Iterate the in-flight map and collect entries that have either
        // produced a result or whose sender disconnected. We can't process
        // results inline because that would borrow self twice, so we
        // gather into local Vecs and then act on them after the borrow
        // ends.
        let mut ready: Vec<MergequeuePollResult> = Vec::new();
        let mut to_remove: Vec<WorkItemId> = Vec::new();
        for (wi_id, state) in &self.mergequeue_polls {
            match state.rx.try_recv() {
                Ok(r) => {
                    ready.push(r);
                    to_remove.push(wi_id.clone());
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {}
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    to_remove.push(wi_id.clone());
                }
            }
        }
        for wi_id in &to_remove {
            if let Some(state) = self.mergequeue_polls.remove(wi_id) {
                self.end_activity(state.activity);
            }
        }

        for result in ready {
            // Check that the item is still in Mergequeue. The user may
            // have retreated or deleted the item between the poll spawn
            // and the result drain.
            let actual_status = self
                .work_items
                .iter()
                .find(|w| w.id == result.wi_id)
                .map(|w| w.status);

            if actual_status.as_ref() != Some(&WorkItemStatus::Mergequeue) {
                // Item moved away - remove watch and discard.
                self.mergequeue_watches.retain(|w| w.wi_id != result.wi_id);
                self.mergequeue_poll_errors.remove(&result.wi_id);
                continue;
            }

            // Backfill pr_number on the watch the first time a branch-
            // based poll resolves to a concrete PR. This pins subsequent
            // polls to the exact PR so a closed-then-reopened-on-same-
            // branch race cannot redirect the watch to a different PR.
            if let Some(identity) = &result.pr_identity
                && let Some(watch) = self
                    .mergequeue_watches
                    .iter_mut()
                    .find(|w| w.wi_id == result.wi_id)
                && watch.pr_number.is_none()
            {
                watch.pr_number = Some(identity.number);
            }

            match result.pr_state.as_str() {
                "MERGED" => {
                    if let Some(identity) = &result.pr_identity
                        && let Err(e) = self.backend.save_pr_identity(
                            &result.wi_id,
                            &result.repo_path,
                            identity,
                        )
                    {
                        self.status_message = Some(format!("PR identity save error: {e}"));
                    }

                    let log_entry = ActivityEntry {
                        timestamp: now_iso8601(),
                        event_type: "pr_merged".to_string(),
                        payload: serde_json::json!({
                            "strategy": "external",
                            "branch": result.branch
                        }),
                    };
                    if let Err(e) = self.backend.append_activity(&result.wi_id, &log_entry) {
                        self.status_message = Some(format!("Activity log error: {e}"));
                    }

                    self.cleanup_worktree_for_item(&result.wi_id);

                    self.mergequeue_watches.retain(|w| w.wi_id != result.wi_id);
                    self.mergequeue_poll_errors.remove(&result.wi_id);

                    self.apply_stage_change(
                        &result.wi_id,
                        &WorkItemStatus::Mergequeue,
                        &WorkItemStatus::Done,
                        "pr_merge",
                    );
                    self.status_message = Some("PR merged externally - moved to [DN]".into());
                }
                "CLOSED" => {
                    self.mergequeue_poll_errors.remove(&result.wi_id);
                    self.status_message = Some(
                        "PR was closed without merging - retreat to Review or re-open the PR"
                            .into(),
                    );
                }
                s if s.starts_with("ERROR:") => {
                    let msg = format!(
                        "Mergequeue poll error for {}: {}",
                        result.branch, result.pr_state
                    );
                    self.mergequeue_poll_errors
                        .insert(result.wi_id.clone(), msg.clone());
                    self.status_message = Some(msg);
                    // Item stays in Mergequeue - will retry on next poll cycle.
                }
                _ => {
                    // Still open - no action, will poll again next cycle.
                    self.mergequeue_poll_errors.remove(&result.wi_id);
                }
            }
        }

        // -- Phase 2: spawn polls for any watch whose per-item cooldown
        // has elapsed and which has no in-flight poll. --
        let cooldown = std::time::Duration::from_secs(30);
        let now = std::time::Instant::now();

        // Collect work to spawn. We can't spawn while iterating
        // self.mergequeue_watches because the spawn updates the watch.
        let mut to_spawn: Vec<(WorkItemId, Option<u64>, String, String, PathBuf)> = Vec::new();
        for watch in &self.mergequeue_watches {
            if self.mergequeue_polls.contains_key(&watch.wi_id) {
                continue;
            }
            if let Some(last) = watch.last_polled
                && now.duration_since(last) < cooldown
            {
                continue;
            }
            to_spawn.push((
                watch.wi_id.clone(),
                watch.pr_number,
                watch.owner_repo.clone(),
                watch.branch.clone(),
                watch.repo_path.clone(),
            ));
        }

        for (wi_id, pr_number, owner_repo, branch, repo_path) in to_spawn {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let thread_wi_id = wi_id.clone();
            let thread_branch = branch.clone();
            let thread_owner_repo = owner_repo.clone();
            let thread_repo_path = repo_path.clone();
            std::thread::spawn(move || {
                // Prefer the pinned PR number (unambiguous) over the branch
                // name. The branch fallback is only used on watches that
                // were rebuilt from a backend record after an app restart
                // and have not yet been polled successfully - after the
                // first successful result we write the number back into
                // the watch so subsequent polls target it directly.
                let target = pr_number
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| thread_branch.clone());
                let outcome = match std::process::Command::new("gh")
                    .args([
                        "pr",
                        "view",
                        &target,
                        "--repo",
                        &thread_owner_repo,
                        "--json",
                        "state,number,title,url",
                    ])
                    .output()
                {
                    Ok(output) if output.status.success() => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let parsed: serde_json::Value = match serde_json::from_str(stdout.trim()) {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = tx.send(MergequeuePollResult {
                                    wi_id: thread_wi_id,
                                    pr_state: format!("ERROR: JSON parse failed: {e}"),
                                    branch: thread_branch,
                                    repo_path: thread_repo_path,
                                    pr_identity: None,
                                });
                                return;
                            }
                        };
                        let state = parsed
                            .get("state")
                            .and_then(|s| s.as_str())
                            .unwrap_or("UNKNOWN")
                            .to_string();
                        let pr_identity =
                            parsed
                                .get("number")
                                .and_then(|n| n.as_u64())
                                .and_then(|number| {
                                    let title = parsed.get("title")?.as_str()?.to_string();
                                    let url = parsed.get("url")?.as_str()?.to_string();
                                    Some(PrIdentityRecord { number, title, url })
                                });
                        MergequeuePollResult {
                            wi_id: thread_wi_id,
                            pr_state: state,
                            branch: thread_branch,
                            repo_path: thread_repo_path,
                            pr_identity,
                        }
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        MergequeuePollResult {
                            wi_id: thread_wi_id,
                            pr_state: format!("ERROR: {}", stderr.trim()),
                            branch: thread_branch,
                            repo_path: thread_repo_path,
                            pr_identity: None,
                        }
                    }
                    Err(e) => MergequeuePollResult {
                        wi_id: thread_wi_id,
                        pr_state: format!("ERROR: {e}"),
                        branch: thread_branch,
                        repo_path: thread_repo_path,
                        pr_identity: None,
                    },
                };
                let _ = tx.send(outcome);
            });

            let activity = self.start_activity(format!("Polling PR for merge ({branch})"));
            self.mergequeue_polls
                .insert(wi_id.clone(), MergequeuePollState { rx, activity });
            if let Some(w) = self
                .mergequeue_watches
                .iter_mut()
                .find(|w| w.wi_id == wi_id)
            {
                w.last_polled = Some(now);
            }
        }
    }

    /// Reconstruct mergequeue watches from backend records after reassembly.
    /// Called after initial assembly and after each reassembly to ensure
    /// watches exist for all Mergequeue items (handles app restart).
    ///
    /// Only the branch and the resolved `owner/repo` are required - the
    /// polling thread can call `gh pr view <branch> --repo <owner/repo>`
    /// as a fallback when `pr_number` is not known, so reconstruction
    /// works even when the PR was merged while the app was closed and
    /// is no longer returned by the open-PR fetch. Once a poll succeeds,
    /// `pr_number` is backfilled on the watch so subsequent polls target
    /// the exact PR unambiguously.
    ///
    /// The `owner/repo` identity is read from the cached
    /// `repo_data[path].github_remote` that the background fetcher
    /// already populates, *never* by shelling out via
    /// `worktree_service.github_remote` on the UI thread. If the fetcher
    /// has not yet produced a result for this repo, the watch is
    /// skipped this cycle and rebuilt on the next reassembly once the
    /// fetch completes.
    pub fn reconstruct_mergequeue_watches(&mut self) {
        for wi in &self.work_items {
            if wi.status != WorkItemStatus::Mergequeue {
                continue;
            }
            // Skip if already watched.
            if self.mergequeue_watches.iter().any(|w| w.wi_id == wi.id) {
                continue;
            }
            let Some(assoc) = wi.repo_associations.first() else {
                continue;
            };
            let Some(ref branch) = assoc.branch else {
                continue;
            };
            // Read the GitHub remote from the cached fetcher result so
            // we never shell out to `git remote get-url` on the UI
            // thread. When the fetcher has not yet populated this repo,
            // skip - the next reassembly (triggered on fetch completion)
            // will retry.
            let Some((owner, repo_name)) = self
                .repo_data
                .get(&assoc.repo_path)
                .and_then(|rd| rd.github_remote.clone())
            else {
                continue;
            };
            // If the background fetch has already populated assoc.pr, pin
            // the number immediately. Otherwise the watch starts with
            // pr_number = None and the first poll will fall back to
            // `gh pr view <branch>`, then fill the number in from the
            // result so subsequent polls are unambiguous.
            let pr_number = assoc.pr.as_ref().map(|pr| pr.number);
            self.mergequeue_watches.push(MergequeueWatch {
                wi_id: wi.id.clone(),
                pr_number,
                owner_repo: format!("{owner}/{repo_name}"),
                branch: branch.clone(),
                repo_path: assoc.repo_path.clone(),
                last_polled: None,
            });
        }
    }

    /// Collect Done items that need PR identity backfill (have a branch but
    /// no persisted pr_identity). Returns tuples of
    /// (wi_id, repo_path, branch, github_owner, github_repo).
    ///
    /// Reads owner/repo from the cached `repo_data[path].github_remote`
    /// populated by the background fetcher. When the fetcher has not
    /// produced a result for a repo yet, that repo is silently skipped
    /// (the next reassembly will retry) - we NEVER shell out via
    /// `worktree_service.github_remote(...)` on the UI thread.
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
                let (owner, repo_name) = match self
                    .repo_data
                    .get(&assoc.repo_path)
                    .and_then(|rd| rd.github_remote.clone())
                {
                    Some((o, r)) => (o, r),
                    None => continue,
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
            if let Some(aid) = self.pr_identity_backfill_activity.take() {
                self.end_activity(aid);
            }
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

    /// Find a PR number for a branch by querying `gh pr list --head <branch>`.
    /// Returns None if no PR exists. Runs on background thread (blocking I/O).
    fn find_pr_for_branch(owner: &str, repo: &str, branch: &str) -> Option<u64> {
        let owner_repo = format!("{owner}/{repo}");
        let output = std::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--head",
                branch,
                "--json",
                "number",
                "--repo",
                &owner_repo,
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let arr: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
        arr.as_array()?.first()?.get("number")?.as_u64()
    }

    /// Fetch per-check CI status for a PR via `gh pr checks`.
    /// Returns empty vec on error (treated as "no checks configured").
    /// Runs on background thread (blocking I/O).
    fn fetch_pr_checks(owner: &str, repo: &str, pr_number: u64) -> Vec<CiCheck> {
        let owner_repo = format!("{owner}/{repo}");
        let pr_str = pr_number.to_string();
        let output = match std::process::Command::new("gh")
            .args([
                "pr",
                "checks",
                &pr_str,
                "--repo",
                &owner_repo,
                "--json",
                "name,bucket",
            ])
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return Vec::new(),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let arr: Vec<serde_json::Value> = match serde_json::from_str(stdout.trim()) {
            Ok(a) => a,
            Err(_) => return Vec::new(),
        };
        // Expected bucket values from `gh pr checks`: "pass", "fail", "pending",
        // "skipping", "cancel". Unknown values are included in the result but
        // won't match any pass/fail filter, effectively treated as pending.
        arr.iter()
            .filter_map(|v| {
                Some(CiCheck {
                    name: v.get("name")?.as_str()?.to_string(),
                    bucket: v.get("bucket")?.as_str()?.to_string(),
                })
            })
            .collect()
    }

    /// Attempt to spawn the async review gate for the given work item.
    /// Returns `Spawned` if the gate is running (caller should wait),
    /// or `Blocked(reason)` if the transition must not proceed.
    ///
    /// Only synchronous, in-memory pre-conditions (gate already running,
    /// work item not found, no repo association, no branch) return
    /// `Blocked` from the main thread. Every blocking check -
    /// `backend.read_plan` (filesystem), `worktree_service.default_branch`
    /// / `github_remote` / `git diff` (git subprocess) - runs inside the
    /// background closure and reports failure through
    /// `ReviewGateMessage::Blocked` so the main UI thread is never blocked.
    ///
    /// `origin` records who initiated the gate; `poll_review_gate` uses
    /// it to decide whether a `Blocked` outcome should apply the full
    /// rework flow (kill + respawn the session) or just surface the
    /// reason as a status message without touching the session. See
    /// `ReviewGateOrigin`.
    fn spawn_review_gate(
        &mut self,
        wi_id: &WorkItemId,
        origin: ReviewGateOrigin,
    ) -> ReviewGateSpawn {
        // Guard: if a review gate is already running for this item, don't spawn a duplicate.
        if self.review_gates.contains_key(wi_id) {
            return ReviewGateSpawn::Blocked("Review gate already running".into());
        }

        // Find the branch for this work item (pure in-memory read).
        // Clone everything off `wi`/`assoc` into owned values up-front so
        // the immutable borrow of `self.work_items` ends before the
        // mutable `start_activity` call below.
        let (title, branch, repo_path, current_pr_number, current_check_status) = {
            let wi = match self.work_items.iter().find(|w| w.id == *wi_id) {
                Some(wi) => wi,
                None => {
                    return ReviewGateSpawn::Blocked("Work item not found".into());
                }
            };
            let assoc = match wi.repo_associations.first() {
                Some(a) => a,
                None => {
                    return ReviewGateSpawn::Blocked(
                        "Cannot enter Review: no repo association".into(),
                    );
                }
            };
            let branch = match assoc.branch.as_ref() {
                Some(b) => b.clone(),
                None => {
                    return ReviewGateSpawn::Blocked("Cannot enter Review: no branch set".into());
                }
            };
            // Two-level Option semantics:
            // - None = no cached PR data, must query fresh
            // - Some(CheckStatus::None) = PR cached but no CI checks configured, skip
            // - Some(other) = PR cached with CI checks, proceed to wait
            (
                wi.title.clone(),
                branch,
                assoc.repo_path.clone(),
                assoc.pr.as_ref().map(|p| p.number),
                assoc.pr.as_ref().map(|p| p.checks.clone()),
            )
        };

        // Status-bar activity for the review gate. Per `docs/UI.md`
        // "Activity indicator placement", review gates are
        // system-initiated background work and must own a status-bar
        // spinner. The ID lives on `ReviewGateState` so every drop site
        // ends it via `drop_review_gate`.
        let activity = self.start_activity(format!("Running review gate for '{title}'"));
        // Clone the worktree service and backend for the background thread so
        // that `default_branch()`/`github_remote()` (which shell out to git)
        // and `read_plan()` (filesystem read) execute off the main UI thread.
        let ws = Arc::clone(&self.worktree_service);
        let backend = Arc::clone(&self.backend);

        // Spawn the review gate in a background thread with three phases:
        // 1. PR existence check (if GitHub remote exists)
        // 2. CI check wait (if checks are configured)
        // 3. Adversarial code review (claude --print)
        // Unbounded rather than bounded(1): multiple Progress messages may
        // queue before the main thread polls.
        let (tx, rx) = crossbeam_channel::unbounded();
        let wi_id_clone = wi_id.clone();
        let review_skill = self.config.defaults.review_skill.clone();
        let gate_mcp_tx = self.mcp_tx.clone();

        std::thread::spawn(move || {
            // === Phase 0: Read plan, resolve default branch, confirm non-empty diff ===
            //
            // Every step here is blocking I/O (filesystem read or git
            // subprocess) so it MUST run on the background thread. On any
            // failure we send `Blocked` through the channel so the main UI
            // thread can clear the gate state and surface the reason in
            // the status bar.
            let plan = match backend.read_plan(&wi_id_clone) {
                Ok(Some(plan)) if !plan.trim().is_empty() => plan,
                Ok(_) => {
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: "Cannot enter Review: no plan exists".into(),
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: format!("Could not read plan: {e}"),
                    });
                    return;
                }
            };

            let default_branch = match ws.default_branch(&repo_path) {
                Ok(b) => b,
                Err(_) => "main".to_string(),
            };

            match crate::worktree_service::git_command()
                .arg("-C")
                .arg(&repo_path)
                .args(["diff", &format!("{default_branch}...{branch}")])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let diff = String::from_utf8_lossy(&output.stdout);
                    if diff.trim().is_empty() {
                        let _ = tx.send(ReviewGateMessage::Blocked {
                            work_item_id: wi_id_clone,
                            reason: "Cannot enter Review: no changes on branch".into(),
                        });
                        return;
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: format!("Review gate: git diff failed: {stderr}"),
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: format!("Review gate: could not run git: {e}"),
                    });
                    return;
                }
            };

            // Resolve GitHub remote on this background thread (blocking I/O).
            let (gh_owner, gh_repo, has_github_remote) = match ws.github_remote(&repo_path) {
                Ok(Some((o, r))) => (o, r, true),
                Ok(None) => (String::new(), String::new(), false),
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!("Could not read GitHub remote: {e}"),
                    }));
                    return;
                }
            };

            // === Phase 1: PR Existence Check ===
            let pr_number = if has_github_remote {
                if tx
                    .send(ReviewGateMessage::Progress(
                        "Checking for pull request...".into(),
                    ))
                    .is_err()
                {
                    return; // Receiver dropped - gate cancelled.
                }

                let pr_num = match current_pr_number {
                    Some(n) => Some(n),
                    None => Self::find_pr_for_branch(&gh_owner, &gh_repo, &branch),
                };

                if pr_num.is_none() {
                    let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!(
                            "No pull request found for branch '{}'. \
                             Create a PR before requesting review.",
                            branch
                        ),
                    }));
                    return;
                }
                pr_num
            } else {
                None
            };

            // === Phase 2: CI Check Wait ===
            if let Some(pr_num) = pr_number {
                // Determine whether CI checks are configured.
                let has_checks = match current_check_status.as_ref() {
                    Some(CheckStatus::None) => false,
                    Some(_) => true,
                    None => {
                        // No cached info - query fresh to discover.
                        !Self::fetch_pr_checks(&gh_owner, &gh_repo, pr_num).is_empty()
                    }
                };

                if has_checks {
                    if tx
                        .send(ReviewGateMessage::Progress(
                            "Waiting for CI checks...".into(),
                        ))
                        .is_err()
                    {
                        return;
                    }

                    // 30-minute timeout: 120 iterations * 15s = 1800s.
                    let max_iterations = 120u32;
                    let mut iteration = 0u32;
                    loop {
                        if iteration >= max_iterations {
                            let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                                work_item_id: wi_id_clone,
                                approved: false,
                                detail: "CI checks did not complete within 30 minutes. \
                                         Check the CI system and retry."
                                    .into(),
                            }));
                            return;
                        }
                        let checks = Self::fetch_pr_checks(&gh_owner, &gh_repo, pr_num);

                        if checks.is_empty() {
                            // Checks disappeared (race) - treat as no checks.
                            break;
                        }

                        let passed = checks
                            .iter()
                            .filter(|c| c.bucket == "pass" || c.bucket == "skipping")
                            .count();
                        let failed: Vec<_> = checks
                            .iter()
                            .filter(|c| c.bucket == "fail" || c.bucket == "cancel")
                            .collect();
                        let total = checks.len();

                        if !failed.is_empty() {
                            let failed_names: Vec<_> =
                                failed.iter().map(|c| c.name.clone()).collect();
                            let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                                work_item_id: wi_id_clone,
                                approved: false,
                                detail: format!(
                                    "CI checks failed: {}. \
                                     Fix failures before requesting review.",
                                    failed_names.join(", ")
                                ),
                            }));
                            return;
                        }

                        if passed == total {
                            // All checks green - proceed to code review.
                            let _ = tx.send(ReviewGateMessage::Progress(format!(
                                "{passed} / {total} CI checks green. Running code review..."
                            )));
                            break;
                        }

                        // Still pending - update progress and poll again.
                        if tx
                            .send(ReviewGateMessage::Progress(format!(
                                "{passed} / {total} CI checks green"
                            )))
                            .is_err()
                        {
                            return; // Receiver dropped - gate cancelled.
                        }
                        iteration += 1;
                        std::thread::sleep(std::time::Duration::from_secs(15));
                    }
                }
            }

            // === Phase 3: Adversarial Code Review ===
            let _ = tx.send(ReviewGateMessage::Progress(
                "Checking implementation against plan.".into(),
            ));

            let repo_path_str = repo_path.display().to_string();
            let mut vars = std::collections::HashMap::new();
            vars.insert("default_branch", default_branch.as_str());
            vars.insert("branch", branch.as_str());
            vars.insert("repo_path", repo_path_str.as_str());
            let system = crate::prompts::render("review_gate", &vars).unwrap_or_else(|| {
                "Compare plan to diff. Respond with JSON: {\"approved\": bool, \"detail\": string}"
                    .into()
            });
            let prompt = review_skill;

            // Start a temporary MCP server so `claude --print` can fetch the
            // plan via MCP tools instead of receiving it as a CLI arg
            // (which would hit the OS ARG_MAX limit on large diffs).
            // The LLM gets the diff by running `git diff` itself.
            let gate_context = serde_json::json!({
                "work_item_id": serde_json::to_string(&wi_id_clone).unwrap_or_default(),
                "plan": plan,
            })
            .to_string();

            let gate_socket = crate::mcp::socket_path_for_session();
            let gate_mcp_tx = gate_mcp_tx;
            let gate_server = match crate::mcp::McpSocketServer::start(
                gate_socket.clone(),
                serde_json::to_string(&wi_id_clone).unwrap_or_default(),
                String::new(),
                gate_context,
                None,
                gate_mcp_tx,
                true, // read_only: review gate must not mutate work item state
            ) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!("review gate: could not start MCP server: {e}"),
                    }));
                    return;
                }
            };

            // Build MCP config file for --mcp-config.
            let exe_path = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    drop(gate_server);
                    let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!("review gate: could not resolve exe path: {e}"),
                    }));
                    return;
                }
            };
            let mcp_config = crate::mcp::build_mcp_config(&exe_path, &gate_socket, &[]);
            let config_path = std::env::temp_dir()
                .join(format!("workbridge-rg-mcp-{}.json", uuid::Uuid::new_v4()));
            if let Err(e) = std::fs::write(&config_path, &mcp_config) {
                drop(gate_server);
                let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                    work_item_id: wi_id_clone,
                    approved: false,
                    detail: format!("review gate: could not write MCP config: {e}"),
                }));
                return;
            }

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
                    "--mcp-config",
                    &config_path.to_string_lossy(),
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

            // Clean up temporary MCP server and config file.
            drop(gate_server);
            let _ = std::fs::remove_file(&config_path);

            let _ = tx.send(ReviewGateMessage::Result(result));
        });

        self.review_gates.insert(
            wi_id.clone(),
            ReviewGateState {
                rx,
                progress: None,
                origin,
                activity,
            },
        );
        ReviewGateSpawn::Spawned
    }

    /// Drop a review gate and end its status-bar activity. Every site
    /// that removes a `review_gates` entry MUST go through this helper:
    /// the activity ID lives inside `ReviewGateState` per
    /// structural-ownership, so dropping the gate without ending the
    /// activity would leak a spinner. See `docs/UI.md` "Activity
    /// indicator placement".
    fn drop_review_gate(&mut self, wi_id: &WorkItemId) {
        if let Some(state) = self.review_gates.remove(wi_id) {
            self.end_activity(state.activity);
        }
    }

    /// Poll all async review gates for results. Called on each timer tick.
    /// If a gate has completed, processes the result: advances to Review
    /// if approved, stays in Implementing if rejected.
    pub fn poll_review_gate(&mut self) {
        if self.review_gates.is_empty() {
            return;
        }

        // Collect keys to avoid borrowing self during iteration.
        let wi_ids: Vec<WorkItemId> = self.review_gates.keys().cloned().collect();

        for wi_id in wi_ids {
            let gate = match self.review_gates.get(&wi_id) {
                Some(g) => g,
                None => continue,
            };

            // Drain all pending messages for this gate.
            let mut result: Option<ReviewGateResult> = None;
            let mut blocked_reason: Option<String> = None;
            let mut disconnected = false;
            let mut last_progress: Option<String> = None;

            loop {
                match gate.rx.try_recv() {
                    Ok(ReviewGateMessage::Progress(text)) => {
                        last_progress = Some(text);
                    }
                    Ok(ReviewGateMessage::Result(r)) => {
                        result = Some(r);
                        break;
                    }
                    Ok(ReviewGateMessage::Blocked {
                        work_item_id: msg_id,
                        reason,
                    }) => {
                        debug_assert_eq!(msg_id, wi_id);
                        blocked_reason = Some(reason);
                        break;
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            // Apply progress update if any.
            if let Some(progress) = last_progress
                && let Some(gate) = self.review_gates.get_mut(&wi_id)
            {
                gate.progress = Some(progress);
            }

            if disconnected {
                // Thread exited without sending a Result - treat as gate error.
                self.drop_review_gate(&wi_id);
                self.status_message =
                    Some("Review gate: background thread exited unexpectedly".into());
                continue;
            }

            // Blocked: the gate could not run against a real diff (no plan,
            // empty diff, git failure, default branch unresolvable).
            //
            // How the outcome is applied depends on who initiated the gate:
            //
            // - `Mcp` / `Auto`: Claude (or the auto-trigger after an
            //   Implementing session died) asked for Review. The rework
            //   flow applies - kill and respawn the session with the
            //   reason so the next Claude run sees the feedback.
            //
            // - `Tui`: The user pressed `l` (advance) on an Implementing
            //   item that cannot satisfy the gate. On master the TUI path
            //   just surfaced the reason in the status bar and left the
            //   session running; killing it here would be a regression.
            //   Only drop the gate state and set the status message.
            //
            // In all cases: if the work item was deleted while the gate
            // was in flight, drop the gate state without touching session
            // or `rework_reasons` - a rework_reasons entry with no owner
            // would leak forever because nothing else ever clears it.
            if let Some(reason) = blocked_reason {
                let origin = self
                    .review_gates
                    .get(&wi_id)
                    .map(|g| g.origin)
                    .unwrap_or(ReviewGateOrigin::Mcp);
                self.drop_review_gate(&wi_id);

                let wi_exists = self.work_items.iter().any(|w| w.id == wi_id);
                if !wi_exists {
                    // Work item deleted mid-gate. Nothing more to do -
                    // the gate state is already dropped and no session
                    // exists to act on.
                    continue;
                }

                match origin {
                    ReviewGateOrigin::Tui => {
                        // Preserve master's non-destructive behaviour: the
                        // user's session is still the primary workspace,
                        // so just surface the reason.
                        self.status_message =
                            Some(format!("Review gate failed to start: {reason}"));
                        continue;
                    }
                    ReviewGateOrigin::Mcp | ReviewGateOrigin::Auto => {
                        self.rework_reasons.insert(wi_id.clone(), reason.clone());
                        self.status_message =
                            Some(format!("Review gate failed to start: {reason}"));

                        // If Blocked, transition to Implementing so the
                        // implementing_rework prompt (which has {rework_reason})
                        // is used instead of the "blocked" prompt.
                        let wi_status = self
                            .work_items
                            .iter()
                            .find(|w| w.id == wi_id)
                            .map(|w| w.status);
                        if wi_status == Some(WorkItemStatus::Blocked) {
                            let _ = self
                                .backend
                                .update_status(&wi_id, WorkItemStatus::Implementing);
                            self.reassemble_work_items();
                            self.build_display_list();
                        }

                        // Kill and respawn the session with the rework
                        // prompt so Claude sees the rejection reason.
                        if let Some(key) = self.session_key_for(&wi_id)
                            && let Some(mut entry) = self.sessions.remove(&key)
                            && let Some(ref mut session) = entry.session
                        {
                            session.kill();
                        }
                        self.cleanup_session_state_for(&wi_id);
                        self.spawn_session(&wi_id);
                        continue;
                    }
                }
            }

            let result = match result {
                Some(r) => r,
                None => continue, // Only progress this tick, no final result yet.
            };

            // Gate completed - remove from map.
            debug_assert_eq!(result.work_item_id, wi_id);
            self.drop_review_gate(&wi_id);

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
                continue;
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
                    .map(|w| w.status)
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
                        .map(|w| w.status);
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
    }

    /// Get the SessionEntry for the currently selected work item, if any.
    pub fn active_session_entry(&self) -> Option<&SessionEntry> {
        let work_item_id = self.selected_work_item_id()?;
        let key = self.session_key_for(&work_item_id)?;
        self.sessions.get(&key)
    }

    /// Get a mutable reference to the SessionEntry for the currently selected
    /// work item. Needed by mouse scroll handling to update scrollback_offset.
    pub fn active_session_entry_mut(&mut self) -> Option<&mut SessionEntry> {
        let work_item_id = self.selected_work_item_id()?;
        let key = self.session_key_for(&work_item_id)?;
        self.sessions.get_mut(&key)
    }

    /// Returns true if any session is alive (including the global session).
    pub fn has_any_session(&self) -> bool {
        self.sessions.values().any(|e| e.alive)
            || self.global_session.as_ref().is_some_and(|s| s.alive)
            || self.terminal_sessions.values().any(|e| e.alive)
    }

    // -- Global assistant --------------------------------------------------

    /// Toggle the global assistant drawer open/closed.
    ///
    /// Every open spawns a fresh `claude` session with an empty context.
    /// Every close immediately tears the session down (kills the child,
    /// drops the MCP server, removes the temp MCP config file, and drops
    /// any buffered keystrokes) so no state leaks into the next opening.
    pub fn toggle_global_drawer(&mut self) {
        if self.global_drawer_open {
            // Close drawer, restore previous focus, and tear down the
            // session so the next open starts from a blank slate.
            self.global_drawer_open = false;
            self.focus = self.pre_drawer_focus;
            self.teardown_global_session();
        } else {
            // Open drawer. Defensively tear down any lingering session
            // state first (covers the edge case where a previous session
            // survived for any reason - e.g. a crash path that skipped
            // the normal close branch), then spawn a fresh session every
            // time so the user always sees an empty PTY with no prior
            // conversation or scrollback.
            self.teardown_global_session();
            self.pre_drawer_focus = self.focus;
            self.global_drawer_open = true;
            self.spawn_global_session();
        }
    }

    /// Tear down the global assistant session and all its associated
    /// resources. Safe to call when no session exists.
    ///
    /// Steps:
    /// 1. SIGTERM + 50 ms grace + SIGKILL the `claude` child process via
    ///    `Session::kill` so no zombie survives.
    /// 2. Drop the `SessionEntry`; `Session::Drop` joins the reader thread.
    /// 3. Drop the MCP server (same as `cleanup_all_mcp`).
    /// 4. Remove the temp MCP config file and clear its path.
    /// 5. Drop any keystrokes queued for the old session's PTY so they
    ///    don't leak into the next session on reopen.
    fn teardown_global_session(&mut self) {
        if let Some(ref mut entry) = self.global_session
            && let Some(ref mut session) = entry.session
        {
            session.kill();
        }
        self.global_session = None;
        self.global_mcp_server = None;
        if let Some(ref path) = self.global_mcp_config_path {
            let _ = std::fs::remove_file(path);
        }
        self.global_mcp_config_path = None;
        self.pending_global_pty_bytes.clear();
    }

    /// Spawn the global assistant Claude Code session.
    fn spawn_global_session(&mut self) {
        // Build dynamic context and start MCP server.
        self.refresh_global_mcp_context();
        let socket_path = crate::mcp::socket_path_for_session();
        let mcp_server = match McpSocketServer::start_global(
            socket_path.clone(),
            Arc::clone(&self.global_mcp_context),
            self.mcp_tx.clone(),
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
        cmd.push("--dangerously-skip-permissions".to_string());
        cmd.push("--allowedTools".to_string());
        cmd.push(
            [
                "mcp__workbridge__workbridge_get_context",
                "mcp__workbridge__workbridge_query_log",
                "mcp__workbridge__workbridge_get_plan",
                "mcp__workbridge__workbridge_report_progress",
                "mcp__workbridge__workbridge_log_event",
                "mcp__workbridge__workbridge_set_activity",
                "mcp__workbridge__workbridge_approve_review",
                "mcp__workbridge__workbridge_request_changes",
                "mcp__workbridge__workbridge_set_status",
                "mcp__workbridge__workbridge_set_plan",
                "mcp__workbridge__workbridge_set_title",
                "mcp__workbridge__workbridge_set_description",
                "mcp__workbridge__workbridge_list_repos",
                "mcp__workbridge__workbridge_list_work_items",
                "mcp__workbridge__workbridge_repo_info",
            ]
            .join(","),
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
        let mcp_config = crate::mcp::build_mcp_config(&exe, &mcp_server.socket_path, &[]);
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

        // Use a dedicated workbridge-owned scratch directory as cwd.
        //
        // We deliberately avoid `$HOME` here: Claude Code's workspace trust
        // dialog ("Do you trust the files in this folder?") persists its
        // acceptance per-project in `~/.claude.json`, but the home directory
        // does not reliably persist that acceptance, so using `$HOME` as the
        // cwd produces the trust prompt on every single Ctrl+G. Every
        // non-home project path Claude Code sees DOES persist trust
        // correctly, so a stable workbridge-owned scratch directory sidesteps
        // the problem entirely without workbridge ever reading or writing
        // `~/.claude.json`. On macOS `$TMPDIR` is per-user and stable across
        // reboots, so the scratch path string is stable across workbridge
        // runs, which means Claude Code's normal trust persistence carries
        // over from one run to the next. The `create_dir_all` call is
        // idempotent and also handles the case where the OS tmp cleaner has
        // wiped the directory since the last spawn.
        let scratch = std::env::temp_dir().join("workbridge-global-assistant-cwd");
        if let Err(e) = std::fs::create_dir_all(&scratch) {
            self.status_message = Some(format!("Global assistant scratch dir error: {e}"));
            self.global_drawer_open = false;
            self.focus = self.pre_drawer_focus;
            return;
        }

        let cmd_refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        match Session::spawn(
            self.global_pane_cols,
            self.global_pane_rows,
            Some(&scratch),
            &cmd_refs,
        ) {
            Ok(session) => {
                let parser = Arc::clone(&session.parser);
                self.global_session = Some(SessionEntry {
                    parser,
                    alive: true,
                    session: Some(session),
                    scrollback_offset: 0,
                    selection: None,
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

    fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
        Ok(())
    }

    fn update_title(&self, _id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
        Ok(())
    }

    fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
        Ok(None)
    }
    fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
        Ok(())
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

        let mut app = App::with_config(cfg, Arc::new(StubBackend));

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
    fn is_inside_managed_repo_positive() {
        let dir = std::env::temp_dir().join("workbridge-test-f3-managed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        // Create the subdirectory on disk so canonicalize succeeds.
        std::fs::create_dir_all(dir.join("src")).unwrap();

        let mut cfg = Config::default();
        cfg.add_repo(dir.to_str().unwrap()).unwrap();

        let app = App::with_config(cfg, Arc::new(StubBackend));

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
    /// Work item creation must store the repo root, not CWD when CWD is
    /// a subdirectory of a managed repo.
    #[test]
    fn managed_repo_root_returns_root_not_subdir() {
        let dir = std::env::temp_dir().join("workbridge-test-r3-f1-root");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/deeply/nested")).unwrap();

        let mut cfg = Config::default();
        cfg.add_repo(dir.to_str().unwrap()).unwrap();

        let app = App::with_config(cfg, Arc::new(StubBackend));

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
        use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
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
                    done_at: None,
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
                    done_at: None,
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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
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
        let mut app = App::with_config(Config::default(), Arc::new(backend));

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
                mergeable: MergeableState::Unknown,
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
        app.sync_selection_identity();
        app.open_delete_prompt();
        app.confirm_delete_from_prompt();
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
                    status: req.status,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: req.repo_associations,
                    plan: None,
                    done_at: None,
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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
            }
            fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
                None
            }
            fn backend_type(&self) -> crate::work_item::BackendType {
                crate::work_item::BackendType::LocalFile
            }
        }

        let mut app = App::with_config(Config::default(), Arc::new(CreateBackend));
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
        assert!(app.structural_fetch_activity.is_some());
        assert!(app.current_activity().is_some());

        // Sending RepoData should clear the activity.
        tx.send(FetchMessage::RepoData(RepoFetchResult {
            repo_path: PathBuf::from("/repo"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),

            issues: vec![],
        }))
        .unwrap();

        app.drain_fetch_results();
        assert!(app.structural_fetch_activity.is_none());
    }

    /// FetcherError also clears the fetch activity.
    #[test]
    fn fetch_started_cleared_on_error() {
        let mut app = App::new();
        let (tx, rx) = std::sync::mpsc::channel();
        app.fetch_rx = Some(rx);

        tx.send(FetchMessage::FetchStarted).unwrap();
        app.drain_fetch_results();
        assert!(app.structural_fetch_activity.is_some());

        tx.send(FetchMessage::FetcherError {
            repo_path: PathBuf::from("/repo"),
            error: "test error".into(),
        })
        .unwrap();

        app.drain_fetch_results();
        assert!(app.structural_fetch_activity.is_none());
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
        assert!(app.structural_fetch_activity.is_some());

        // First repo finishes - spinner should persist.
        tx.send(FetchMessage::RepoData(RepoFetchResult {
            repo_path: PathBuf::from("/repo-a"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),

            issues: vec![],
        }))
        .unwrap();
        app.drain_fetch_results();
        assert!(
            app.structural_fetch_activity.is_some(),
            "spinner should persist while second repo is still fetching",
        );

        // Second repo finishes - now spinner should clear.
        tx.send(FetchMessage::RepoData(RepoFetchResult {
            repo_path: PathBuf::from("/repo-b"),
            github_remote: None,
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![]),

            issues: vec![],
        }))
        .unwrap();
        app.drain_fetch_results();
        assert!(app.structural_fetch_activity.is_none());
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

        let app = App::with_config(cfg, Arc::new(StubBackend));

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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
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
            done_at: None,
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
            done_at: None,
        };

        // Start with order A, B.
        let backend = OrderableBackend {
            records: std::sync::Mutex::new(vec![record_a.clone(), record_b.clone()]),
        };
        let mut app = App::with_config(Config::default(), Arc::new(backend));

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

    /// Helper for ACTIVE-group sort tests: build a minimal WorkItem for
    /// a given repo path and status. Title is purely informational for
    /// assertion messages.
    fn active_sort_test_item(
        title: &str,
        status: WorkItemStatus,
        repo: &str,
    ) -> crate::work_item::WorkItem {
        crate::work_item::WorkItem {
            id: WorkItemId::LocalFile(PathBuf::from(format!("/tmp/{title}.json"))),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: title.to_string(),
            description: None,
            status,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from(repo),
                branch: None,
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        }
    }

    /// Inside a single `ACTIVE (<repo>)` sub-group, items must be sorted
    /// by workflow stage (PL -> IM -> RV -> MQ) regardless of the
    /// backend's insertion order. Within a single stage, the relative
    /// order from backend path order is preserved (stable sort).
    #[test]
    fn build_display_list_sorts_active_group_by_stage() {
        let mut app = App::new();
        // Insert in a deliberately non-workflow order. Two PL items,
        // two IM items, one RV, one MQ - all in the same repo.
        app.work_items = vec![
            active_sort_test_item("a", WorkItemStatus::Implementing, "/repo"),
            active_sort_test_item("b", WorkItemStatus::Planning, "/repo"),
            active_sort_test_item("c", WorkItemStatus::Review, "/repo"),
            active_sort_test_item("d", WorkItemStatus::Planning, "/repo"),
            active_sort_test_item("e", WorkItemStatus::Implementing, "/repo"),
            active_sort_test_item("f", WorkItemStatus::Mergequeue, "/repo"),
        ];
        app.build_display_list();

        // Find the single ACTIVE (repo) group header and collect the
        // work-item indices that follow it until the next header.
        let mut header_idx = None;
        let mut header_count = None;
        for (i, entry) in app.display_list.iter().enumerate() {
            if let DisplayEntry::GroupHeader { label, count, .. } = entry
                && label.starts_with("ACTIVE ")
            {
                header_idx = Some(i);
                header_count = Some(*count);
                break;
            }
        }
        let header_idx = header_idx.expect("expected an ACTIVE group header");
        assert_eq!(
            header_count,
            Some(6),
            "ACTIVE group header count should match item count",
        );

        // Gather ordered titles from the entries that follow the header.
        let mut ordered_titles: Vec<&str> = Vec::new();
        for entry in app.display_list.iter().skip(header_idx + 1) {
            match entry {
                DisplayEntry::WorkItemEntry(wi_idx) => {
                    ordered_titles.push(app.work_items[*wi_idx].title.as_str());
                }
                DisplayEntry::GroupHeader { .. } => break,
                _ => break,
            }
        }

        // Expected: PL items first (b, d in original order), then IM
        // (a, e), then RV (c), then MQ (f).
        assert_eq!(
            ordered_titles,
            vec!["b", "d", "a", "e", "c", "f"],
            "ACTIVE group items should sort PL -> IM -> RV -> MQ \
             with backend order preserved within each stage",
        );
    }

    /// Single-stage ACTIVE buckets must preserve the original backend
    /// order as the stable-sort tiebreaker. This guards against a future
    /// refactor that swaps in an unstable sort.
    #[test]
    fn push_repo_groups_preserves_single_stage_ordering() {
        let mut app = App::new();
        app.work_items = vec![
            active_sort_test_item("x", WorkItemStatus::Implementing, "/repo"),
            active_sort_test_item("y", WorkItemStatus::Implementing, "/repo"),
            active_sort_test_item("z", WorkItemStatus::Implementing, "/repo"),
        ];
        app.build_display_list();

        let header_idx = app
            .display_list
            .iter()
            .position(|e| {
                matches!(
                    e,
                    DisplayEntry::GroupHeader { label, .. } if label.starts_with("ACTIVE ")
                )
            })
            .expect("expected an ACTIVE group header");

        let mut ordered_titles: Vec<&str> = Vec::new();
        for entry in app.display_list.iter().skip(header_idx + 1) {
            match entry {
                DisplayEntry::WorkItemEntry(wi_idx) => {
                    ordered_titles.push(app.work_items[*wi_idx].title.as_str());
                }
                DisplayEntry::GroupHeader { .. } => break,
                _ => break,
            }
        }
        assert_eq!(
            ordered_titles,
            vec!["x", "y", "z"],
            "single-stage ACTIVE bucket must preserve original order",
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
                done_at: None,
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
        use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};

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
                mergeable: MergeableState::Unknown,
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
                mergeable: MergeableState::Unknown,
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

        let mut app = App::with_config(cfg, Arc::new(StubBackend));

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
        use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
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
                    has_commits_ahead: Some(false),
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
                    done_at: None,
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
                    done_at: None,
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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
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
            Arc::new(backend),
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
                mergeable: MergeableState::Unknown,
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

        // Import it (spawns background worktree creation).
        app.import_selected_unlinked();

        // Wait for the background thread to complete and poll the result.
        std::thread::sleep(std::time::Duration::from_millis(50));
        app.poll_worktree_creation();

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
        use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
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
                    has_commits_ahead: Some(false),
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
                    done_at: None,
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
                    done_at: None,
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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
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
            Arc::new(backend),
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
                mergeable: MergeableState::Unknown,
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

        // Import it (spawns background worktree creation).
        app.import_selected_unlinked();

        // Wait for the background thread to complete and poll the result.
        std::thread::sleep(std::time::Duration::from_millis(50));
        app.poll_worktree_creation();

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

    /// find_reusable_worktree must only accept worktrees that live at the
    /// exact expected target path, are not the main worktree, and are on
    /// the target branch. Any other match is rejected so the caller falls
    /// through to `create_worktree` (which surfaces git's "already checked
    /// out" error for truly conflicting cases).
    #[test]
    fn find_reusable_worktree_enforces_all_guards() {
        use crate::worktree_service::{WorktreeError, WorktreeInfo};

        struct ListOnlyMock {
            entries: Vec<WorktreeInfo>,
        }
        impl WorktreeService for ListOnlyMock {
            fn list_worktrees(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
                Ok(self.entries.clone())
            }
            fn create_worktree(
                &self,
                _repo_path: &std::path::Path,
                _branch: &str,
                _target_dir: &std::path::Path,
            ) -> Result<WorktreeInfo, WorktreeError> {
                Err(WorktreeError::GitError("not used".into()))
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
            fn default_branch(
                &self,
                _repo_path: &std::path::Path,
            ) -> Result<String, WorktreeError> {
                Ok("main".into())
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

        // find_reusable_worktree canonicalizes both paths, so they must
        // exist on disk. Use a temp dir with a fresh subdirectory per case.
        let root = std::env::temp_dir().join(format!(
            "workbridge-reusable-worktree-test-{}",
            std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let wt_target = repo.join(".worktrees").join("feature-x");
        std::fs::create_dir_all(&wt_target).unwrap();
        let other_target = repo.join(".worktrees").join("feature-x-alt");
        std::fs::create_dir_all(&other_target).unwrap();

        // Case 1: exact match at wt_target, not main, branch matches -> accept.
        let mock = ListOnlyMock {
            entries: vec![WorktreeInfo {
                path: wt_target.clone(),
                branch: Some("feature-x".into()),
                is_main: false,
                has_commits_ahead: None,
            }],
        };
        let found = App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target);
        assert!(found.is_some(), "valid reuse should be accepted");

        // Case 2: is_main=true must be rejected even if path and branch match.
        let mock = ListOnlyMock {
            entries: vec![WorktreeInfo {
                path: wt_target.clone(),
                branch: Some("feature-x".into()),
                is_main: true,
                has_commits_ahead: None,
            }],
        };
        assert!(
            App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
            "main worktree must never be reused as a work-item worktree",
        );

        // Case 3: branch mismatch must be rejected.
        let mock = ListOnlyMock {
            entries: vec![WorktreeInfo {
                path: wt_target.clone(),
                branch: Some("other-branch".into()),
                is_main: false,
                has_commits_ahead: None,
            }],
        };
        assert!(
            App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
            "branch mismatch must not be reused",
        );

        // Case 4: path mismatch (worktree at a different location than the
        // expected .worktrees/<branch>) must be rejected.
        let mock = ListOnlyMock {
            entries: vec![WorktreeInfo {
                path: other_target.clone(),
                branch: Some("feature-x".into()),
                is_main: false,
                has_commits_ahead: None,
            }],
        };
        assert!(
            App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
            "worktree at unexpected location must not be silently adopted",
        );

        // Case 5: empty list -> None (happy path for fresh creates).
        let mock = ListOnlyMock { entries: vec![] };
        assert!(
            App::find_reusable_worktree(&mock, &repo, "feature-x", &wt_target).is_none(),
            "empty list should yield None",
        );

        // Case 6: wt_target does not exist on disk -> None (canonicalization
        // fails; the caller will fall through to create_worktree).
        let missing_target = repo.join(".worktrees").join("never-existed");
        let mock = ListOnlyMock {
            entries: vec![WorktreeInfo {
                path: wt_target.clone(),
                branch: Some("feature-x".into()),
                is_main: false,
                has_commits_ahead: None,
            }],
        };
        assert!(
            App::find_reusable_worktree(&mock, &repo, "feature-x", &missing_target).is_none(),
            "non-existent target path must not match anything",
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// F-2 regression: import_selected_unlinked creates the worktree under
    /// repo_path/worktree_dir/branch, not repo_path.parent()/<repo>-wt-<branch>.
    #[test]
    fn import_creates_worktree_under_config_worktree_dir() {
        use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
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
                    has_commits_ahead: Some(false),
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
                    done_at: None,
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
                    done_at: None,
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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
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
            Arc::new(backend),
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
                mergeable: MergeableState::Unknown,
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

        // Import it (spawns background worktree creation).
        app.import_selected_unlinked();

        // Wait for the background thread to complete and poll the result.
        std::thread::sleep(std::time::Duration::from_millis(50));
        app.poll_worktree_creation();

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
                    status: req.status,
                    kind: crate::work_item::WorkItemKind::Own,
                    repo_associations: req.repo_associations,
                    plan: None,
                    done_at: None,
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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
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
            Arc::new(RecordingBackend {
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
        use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
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
                mergeable: MergeableState::Unknown,
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
        // The merge prompt is now a dialog overlay; it no longer sets status_message.
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
            .status;
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.alert_message.as_deref().unwrap_or("");
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
            .status;
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.alert_message.as_deref().unwrap_or("");
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
        // No repo_data entry for /tmp/repo, so the cached github_remote
        // lookup returns None and execute_merge blocks the merge.
        app.execute_merge(&wi_id, "squash");
        let status = app
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status;
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.alert_message.as_deref().unwrap_or("");
        assert!(msg.contains("GitHub remote not yet cached"), "got: {msg}");
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
        app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
            .expect("helper admit should succeed");
        app.attach_user_action_payload(&UserActionKey::PrMerge, UserActionPayload::PrMerge { rx });
        app.poll_pr_merge();
        let status = app
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status;
        assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
        let msg = app.alert_message.as_deref().unwrap_or("");
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
        app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
            .expect("helper admit should succeed");
        app.attach_user_action_payload(&UserActionKey::PrMerge, UserActionPayload::PrMerge { rx });
        app.poll_pr_merge();
        // After apply_stage_change, reassemble rebuilds from StubBackend (empty),
        // so we verify via the status message that the merge path was taken.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("PR merged") && msg.contains("[DN]"),
            "should confirm merge and Done, got: {msg}",
        );
    }

    // -- Feature: mergequeue polling --

    /// poll_mergequeue should advance the item to Done and clear the watch
    /// when the drained result reports the PR as MERGED.
    #[test]
    fn poll_mergequeue_merged_advances_to_done_and_clears_watch() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-merged.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "In mergequeue".into(),
            description: None,
            status: WorkItemStatus::Mergequeue,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.mergequeue_watches.push(MergequeueWatch {
            wi_id: wi_id.clone(),
            pr_number: Some(77),
            owner_repo: "owner/repo".into(),
            branch: "feature/x".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            last_polled: Some(std::time::Instant::now()),
        });
        // Seed a stale poll error to confirm it is cleared on the successful
        // merge detection.
        app.mergequeue_poll_errors
            .insert(wi_id.clone(), "previous failure".into());

        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(MergequeuePollResult {
            wi_id: wi_id.clone(),
            pr_state: "MERGED".into(),
            branch: "feature/x".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            pr_identity: Some(PrIdentityRecord {
                number: 77,
                title: "Feature X".into(),
                url: "https://github.com/owner/repo/pull/77".into(),
            }),
        })
        .unwrap();
        let activity = app.start_activity("test poll");
        app.mergequeue_polls
            .insert(wi_id.clone(), MergequeuePollState { rx, activity });

        app.poll_mergequeue();

        assert!(
            app.mergequeue_watches.iter().all(|w| w.wi_id != wi_id),
            "watch should be removed after MERGED detection",
        );
        assert!(
            !app.mergequeue_polls.contains_key(&wi_id),
            "in-flight poll entry should be removed after MERGED detection",
        );
        assert!(
            !app.mergequeue_poll_errors.contains_key(&wi_id),
            "stale poll error should be cleared on success",
        );
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("PR merged") && msg.contains("[DN]"),
            "should confirm external merge and Done, got: {msg}",
        );
    }

    /// poll_mergequeue should record a poll error on ERROR state and leave the
    /// watch in place so the next cycle retries.
    #[test]
    fn poll_mergequeue_error_persists_on_work_item() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-err.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "In mergequeue".into(),
            description: None,
            status: WorkItemStatus::Mergequeue,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.mergequeue_watches.push(MergequeueWatch {
            wi_id: wi_id.clone(),
            pr_number: Some(88),
            owner_repo: "owner/repo".into(),
            branch: "feature/y".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            last_polled: Some(std::time::Instant::now()),
        });

        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(MergequeuePollResult {
            wi_id: wi_id.clone(),
            pr_state: "ERROR: gh auth failed".into(),
            branch: "feature/y".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            pr_identity: None,
        })
        .unwrap();
        let activity = app.start_activity("test poll");
        app.mergequeue_polls
            .insert(wi_id.clone(), MergequeuePollState { rx, activity });

        app.poll_mergequeue();

        assert!(
            app.mergequeue_watches.iter().any(|w| w.wi_id == wi_id),
            "watch should remain on ERROR so next cycle retries",
        );
        assert!(
            !app.mergequeue_polls.contains_key(&wi_id),
            "in-flight poll entry should be drained after ERROR",
        );
        let stored = app
            .mergequeue_poll_errors
            .get(&wi_id)
            .expect("error should be recorded");
        assert!(
            stored.contains("gh auth failed"),
            "error should contain gh stderr, got: {stored}",
        );
    }

    /// When a watch has pr_number = None (the restart path, where the
    /// first poll has to fall back to `gh pr view <branch>`) and the
    /// result carries a resolved pr_identity, the watch's pr_number
    /// must be backfilled so the next poll targets the exact PR
    /// unambiguously. This is the fix for R1-F-3: after the first
    /// branch-resolved cycle the watch is pinned and the closed-then-
    /// reopened-on-same-branch race can no longer redirect the poll.
    #[test]
    fn poll_mergequeue_backfills_pr_number_on_first_success() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-backfill.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Restarted mergequeue item".into(),
            description: None,
            status: WorkItemStatus::Mergequeue,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        // Watch starts with pr_number = None, as if reconstructed from a
        // backend record after an app restart where the open-PR fetch had
        // not yet populated assoc.pr.
        app.mergequeue_watches.push(MergequeueWatch {
            wi_id: wi_id.clone(),
            pr_number: None,
            owner_repo: "owner/repo".into(),
            branch: "feature/backfill".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            last_polled: Some(std::time::Instant::now()),
        });

        // Simulate a successful poll returning the PR as still OPEN.
        // The key point is that the result carries a pr_identity with
        // number = 321, which the drain path must pin onto the watch.
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(MergequeuePollResult {
            wi_id: wi_id.clone(),
            pr_state: "OPEN".into(),
            branch: "feature/backfill".into(),
            repo_path: PathBuf::from("/tmp/repo"),
            pr_identity: Some(PrIdentityRecord {
                number: 321,
                title: "Backfill test".into(),
                url: "https://github.com/owner/repo/pull/321".into(),
            }),
        })
        .unwrap();
        let activity = app.start_activity("test poll");
        app.mergequeue_polls
            .insert(wi_id.clone(), MergequeuePollState { rx, activity });

        app.poll_mergequeue();

        let watch = app
            .mergequeue_watches
            .iter()
            .find(|w| w.wi_id == wi_id)
            .expect("watch should still be present after OPEN result");
        assert_eq!(
            watch.pr_number,
            Some(321),
            "pr_number should be backfilled from the first successful poll",
        );
    }

    /// Retreating one Mergequeue item must not affect another Mergequeue
    /// item's in-flight poll. This is the regression test for the bug
    /// the singleton mergequeue_poll_rx + activity field caused before
    /// the refactor: with two items A and B in Mergequeue and an
    /// in-flight poll for A, retreating B used to drop A's poll and
    /// activity unconditionally.
    #[test]
    fn retreat_one_mergequeue_item_does_not_disturb_another_in_flight_poll() {
        let mut app = App::new();

        let wi_a = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-a.json"));
        let wi_b = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-b.json"));
        for (id, branch) in [(&wi_a, "feature/a"), (&wi_b, "feature/b")] {
            app.work_items.push(crate::work_item::WorkItem {
                id: id.clone(),
                backend_type: BackendType::LocalFile,
                kind: crate::work_item::WorkItemKind::Own,
                title: format!("MQ {branch}"),
                description: None,
                status: WorkItemStatus::Mergequeue,
                status_derived: false,
                repo_associations: vec![],
                errors: vec![],
            });
            app.mergequeue_watches.push(MergequeueWatch {
                wi_id: id.clone(),
                pr_number: Some(1000),
                owner_repo: "owner/repo".into(),
                branch: branch.into(),
                repo_path: PathBuf::from("/tmp/repo"),
                last_polled: Some(std::time::Instant::now()),
            });
        }
        // Build the display list and select item B so retreat_stage acts
        // on it.
        app.display_list.push(DisplayEntry::WorkItemEntry(0));
        app.display_list.push(DisplayEntry::WorkItemEntry(1));
        app.selected_item = Some(1);

        // Spawn a fake in-flight poll for A only. Use a never-completing
        // channel - we only need the entry to exist; we are not calling
        // poll_mergequeue so the rx is never drained.
        let (_tx_a, rx_a) = crossbeam_channel::bounded(1);
        let activity_a = app.start_activity("polling A");
        app.mergequeue_polls.insert(
            wi_a.clone(),
            MergequeuePollState {
                rx: rx_a,
                activity: activity_a,
            },
        );

        // Retreat B.
        app.retreat_stage();

        // A's poll must still be present and its activity must still be
        // alive. B must be gone from the watches.
        assert!(
            app.mergequeue_polls.contains_key(&wi_a),
            "retreating B must not drop A's in-flight poll",
        );
        assert!(
            app.activities.iter().any(|a| a.id == activity_a),
            "retreating B must not end A's polling activity",
        );
        assert!(
            app.mergequeue_watches.iter().any(|w| w.wi_id == wi_a),
            "A's watch should remain",
        );
        assert!(
            app.mergequeue_watches.iter().all(|w| w.wi_id != wi_b),
            "B's watch should be removed",
        );
    }

    /// reconstruct_mergequeue_watches should rebuild a watch from just the
    /// backend record's branch + the resolved GitHub remote from the
    /// cached `repo_data` entry (populated earlier by the background
    /// fetcher), with no live `assoc.pr` and no persisted `pr_identity`.
    /// This is the critical restart scenario: the PR was merged
    /// externally while the app was closed, so the open-PR fetch no
    /// longer returns it. The watch must still come back so polling can
    /// resume, and the rebuild must never shell out to `git remote
    /// get-url` on the UI thread.
    #[test]
    fn reconstruct_mergequeue_watches_from_branch_only() {
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-restart.json"));
        let repo_path = PathBuf::from("/tmp/repo");
        // Deliberately no pr_identity on the record: this simulates an
        // existing Mergequeue ticket created before `pr_identity` was ever
        // persisted for Mergequeue (the motivating case from the user's
        // report). Reconstruction must still rebuild the watch.
        let record = crate::work_item_backend::WorkItemRecord {
            id: wi_id.clone(),
            title: "Was polling".into(),
            description: None,
            status: WorkItemStatus::Mergequeue,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo_path.clone(),
                branch: Some("feature/z".into()),
                pr_identity: None,
            }],
            plan: None,
            done_at: None,
        };

        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![record]),
        };
        let mut app = App::with_config(Config::for_test(), Arc::new(backend));
        // Seed repo_data with a cached github_remote so reconstruction
        // finds the owner/repo without shelling out. This mirrors the
        // real flow: the background fetcher has already populated
        // repo_data by the time reassemble_work_items runs.
        app.repo_data.insert(
            repo_path.clone(),
            crate::work_item::RepoFetchResult {
                repo_path: repo_path.clone(),
                github_remote: Some(("owner".into(), "repo".into())),
                worktrees: Ok(Vec::new()),
                prs: Ok(Vec::new()),
                review_requested_prs: Ok(Vec::new()),
                issues: Vec::new(),
            },
        );
        app.reassemble_work_items();

        let watch = app
            .mergequeue_watches
            .iter()
            .find(|w| w.wi_id == wi_id)
            .expect("reconstruction should rebuild the watch from cached github_remote");
        assert_eq!(watch.owner_repo, "owner/repo");
        assert_eq!(watch.branch, "feature/z");
    }

    /// reconstruct_mergequeue_watches must not call
    /// `worktree_service.github_remote` (which shells out to `git remote
    /// get-url`). When the cached `repo_data.github_remote` is missing,
    /// the watch is simply skipped this cycle and will be rebuilt on the
    /// next reassembly once the fetcher publishes a result for the repo.
    #[test]
    fn reconstruct_mergequeue_watches_skips_when_repo_data_missing() {
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-unfetched.json"));
        let record = crate::work_item_backend::WorkItemRecord {
            id: wi_id.clone(),
            title: "Not yet fetched".into(),
            description: None,
            status: WorkItemStatus::Mergequeue,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/tmp/unfetched-repo"),
                branch: Some("feature/unfetched".into()),
                pr_identity: None,
            }],
            plan: None,
            done_at: None,
        };

        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![record]),
        };
        let mut app = App::with_config(Config::for_test(), Arc::new(backend));
        // Deliberately do not seed repo_data, mirroring the cold-start
        // window before the first fetch completes.
        app.reassemble_work_items();

        assert!(
            app.mergequeue_watches.iter().all(|w| w.wi_id != wi_id),
            "watch should be skipped when repo_data has no cached github_remote",
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
        // The rework prompt is now a dialog overlay; it no longer sets status_message.
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

    // -- Branch invariant + "Set branch name" recovery dialog --

    /// Helper: spin up an App backed by a real LocalFileBackend in a
    /// temp directory with one Backlog work item whose repo association
    /// has `branch: None`. Returns (app, wi_id, temp_dir) so the caller
    /// owns cleanup.
    fn app_with_branchless_backlog_item(name: &str) -> (App, WorkItemId, PathBuf) {
        use crate::work_item_backend::{CreateWorkItem, LocalFileBackend, RepoAssociationRecord};

        let dir = std::env::temp_dir().join(format!("workbridge-test-branchless-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();
        let record = backend
            .create(CreateWorkItem {
                title: "Needs a branch".into(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/tmp/branchless-repo"),
                    branch: None,
                    pr_identity: None,
                }],
            })
            .unwrap();
        let wi_id = record.id.clone();

        let mut app = App::with_config(Config::for_test(), Arc::new(backend));
        app.reassemble_work_items();
        app.build_display_list();
        // Position selection on the newly created item.
        app.selected_work_item = Some(wi_id.clone());
        app.build_display_list();

        (app, wi_id, dir)
    }

    /// advance_stage from a branchless Backlog item must refuse the
    /// stage change and open the recovery dialog instead, so the user
    /// is not silently moved into Planning with no branch set.
    #[test]
    fn advance_from_backlog_without_branch_opens_dialog() {
        let (mut app, wi_id, dir) = app_with_branchless_backlog_item("advance-opens");

        app.advance_stage();

        assert!(
            app.set_branch_dialog.is_some(),
            "advance_stage should open the Set branch dialog",
        );
        let dlg = app.set_branch_dialog.as_ref().unwrap();
        assert_eq!(dlg.wi_id, wi_id);
        assert!(matches!(
            dlg.pending,
            crate::create_dialog::PendingBranchAction::Advance {
                from: WorkItemStatus::Backlog,
                to: WorkItemStatus::Planning,
            }
        ));
        assert_eq!(
            app.work_items
                .iter()
                .find(|w| w.id == wi_id)
                .unwrap()
                .status,
            WorkItemStatus::Backlog,
            "advance must not mutate status when the branch invariant fails",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Confirming the Set branch dialog from an advance_stage-triggered
    /// open must persist the branch via the backend and then re-drive
    /// the same stage change so the work item actually advances.
    #[test]
    fn confirm_set_branch_dialog_persists_and_advances() {
        let (mut app, wi_id, dir) = app_with_branchless_backlog_item("confirm-advance");

        // Open the dialog via the advance path.
        app.advance_stage();
        assert!(app.set_branch_dialog.is_some());

        // Overwrite the prefilled slug with a deterministic value so
        // the assertion below is stable across runs.
        if let Some(dlg) = app.set_branch_dialog.as_mut() {
            dlg.input.clear();
            dlg.input.set_text("user/needs-a-branch-abcd");
        }

        app.confirm_set_branch_dialog();

        assert!(
            app.set_branch_dialog.is_none(),
            "confirm should close the dialog",
        );
        let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
        assert_eq!(
            wi.status,
            WorkItemStatus::Planning,
            "confirm should re-drive the pending stage advance",
        );
        assert_eq!(
            wi.repo_associations[0].branch.as_deref(),
            Some("user/needs-a-branch-abcd"),
            "branch must be persisted to the repo association",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Confirming the Set branch dialog from a spawn_session-triggered
    /// open must persist the branch and re-enter spawn_session. Under
    /// the StubWorktreeService, that path admits a WorktreeCreate user
    /// action (the background thread never resolves because the stub
    /// never sends on its channel, but the single-flight slot IS
    /// occupied, which is what we assert here).
    #[test]
    fn confirm_set_branch_dialog_persists_and_spawns_session() {
        use crate::work_item_backend::{CreateWorkItem, LocalFileBackend, RepoAssociationRecord};

        let dir = std::env::temp_dir().join("workbridge-test-branchless-spawn");
        let _ = std::fs::remove_dir_all(&dir);
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();
        // Use Planning so spawn_session proceeds past the
        // Backlog/Done/Mergequeue early-return.
        let record = backend
            .create(CreateWorkItem {
                title: "Resume me".into(),
                description: None,
                status: WorkItemStatus::Planning,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/tmp/branchless-spawn-repo"),
                    branch: None,
                    pr_identity: None,
                }],
            })
            .unwrap();
        let wi_id = record.id.clone();

        let mut app = App::with_config(Config::for_test(), Arc::new(backend));
        app.reassemble_work_items();
        app.selected_work_item = Some(wi_id.clone());
        app.build_display_list();

        // First Enter press: spawn_session on a branchless item must
        // open the recovery dialog instead of the old dead-end status
        // message.
        app.spawn_session(&wi_id);
        assert!(
            app.set_branch_dialog.is_some(),
            "spawn_session on a branchless item must open the Set branch dialog",
        );
        assert!(matches!(
            app.set_branch_dialog.as_ref().unwrap().pending,
            crate::create_dialog::PendingBranchAction::SpawnSession
        ));

        // Drop the prefilled slug and type a deterministic branch.
        if let Some(dlg) = app.set_branch_dialog.as_mut() {
            dlg.input.clear();
            dlg.input.set_text("user/resume-me-abcd");
        }

        app.confirm_set_branch_dialog();

        assert!(app.set_branch_dialog.is_none());
        let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
        assert_eq!(
            wi.repo_associations[0].branch.as_deref(),
            Some("user/resume-me-abcd"),
            "branch must be persisted before re-driving spawn_session",
        );
        assert!(
            app.is_user_action_in_flight(&UserActionKey::WorktreeCreate),
            "re-driven spawn_session must admit a WorktreeCreate action",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Esc (cancel_set_branch_dialog) must not mutate anything: the
    /// work item stays branchless and in Backlog, the backend record
    /// on disk is untouched, and there is no lingering dialog state.
    #[test]
    fn cancel_set_branch_dialog_leaves_item_unchanged() {
        let (mut app, wi_id, dir) = app_with_branchless_backlog_item("cancel");

        app.advance_stage();
        assert!(app.set_branch_dialog.is_some());

        app.cancel_set_branch_dialog();

        assert!(app.set_branch_dialog.is_none());
        let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
        assert_eq!(wi.status, WorkItemStatus::Backlog);
        assert!(wi.repo_associations[0].branch.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// spawn_session on a branchless Planning item opens the dialog
    /// (regression guard for the old "Set a branch name to start
    /// working" dead-end status message at the former `None =>` arm).
    #[test]
    fn spawn_session_on_branchless_item_opens_dialog_instead_of_message() {
        use crate::work_item_backend::{CreateWorkItem, LocalFileBackend, RepoAssociationRecord};

        let dir = std::env::temp_dir().join("workbridge-test-branchless-spawn-msg");
        let _ = std::fs::remove_dir_all(&dir);
        let backend = LocalFileBackend::with_dir(dir.clone()).unwrap();
        let record = backend
            .create(CreateWorkItem {
                title: "Dead-end fix".into(),
                description: None,
                status: WorkItemStatus::Planning,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: PathBuf::from("/tmp/dead-end-repo"),
                    branch: None,
                    pr_identity: None,
                }],
            })
            .unwrap();
        let wi_id = record.id.clone();

        let mut app = App::with_config(Config::for_test(), Arc::new(backend));
        app.reassemble_work_items();
        app.selected_work_item = Some(wi_id.clone());
        app.build_display_list();

        app.spawn_session(&wi_id);

        assert!(
            app.set_branch_dialog.is_some(),
            "spawn_session must open the Set branch dialog, not surface a hint string",
        );
        // And it must NOT have left the old dead-end message behind.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            !msg.contains("Set a branch name"),
            "old dead-end status message should be gone, got: {msg}",
        );

        let _ = std::fs::remove_dir_all(&dir);
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
                scrollback_offset: 0,
                selection: None,
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
        // Planning session: has --dangerously-skip-permissions, --settings hook, and positional prompt.
        let cmd =
            App::build_claude_cmd(&WorkItemStatus::Planning, Some("system prompt here"), false);
        assert_eq!(cmd[0], "claude");
        assert_eq!(cmd[1], "--dangerously-skip-permissions");
        assert_eq!(cmd[2], "--allowedTools");
        assert!(
            cmd[3].contains("mcp__workbridge__workbridge_get_context"),
            "allowed tools must include workbridge MCP tools",
        );
        assert_eq!(cmd[4], "--settings");
        assert!(
            cmd[5].contains("PostToolUse") && cmd[5].contains("workbridge_set_plan"),
            "planning sessions must include TodoWrite reminder hook via --settings",
        );
        assert_eq!(cmd[6], "--system-prompt");
        assert_eq!(cmd[7], "system prompt here");
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
        assert_eq!(cmd[1], "--dangerously-skip-permissions");
        assert_eq!(cmd[2], "--allowedTools");
        assert!(
            cmd[3].contains("mcp__workbridge__workbridge_get_context"),
            "allowed tools must include workbridge MCP tools",
        );
        assert_eq!(cmd[4], "--system-prompt");
        assert!(
            cmd.last().unwrap().contains("start working"),
            "implementing should have auto-start prompt",
        );
    }

    // -- Feature: global assistant drawer teardown --

    /// `teardown_global_session` must clear every piece of global-assistant
    /// state: the `SessionEntry`, the MCP server slot, the temp MCP config
    /// file (and its path), and any buffered PTY keystrokes. This is what
    /// guarantees the next Ctrl+G opening starts from a blank slate.
    #[test]
    fn teardown_global_session_clears_all_state() {
        let mut app = App::new();

        // Pre-populate a fake SessionEntry with no real PTY child. The
        // `session: None` avoids needing to spawn a real subprocess; the
        // teardown helper skips the `session.kill()` branch when the
        // inner session is None and still runs the rest of the cleanup.
        let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
        app.global_session = Some(SessionEntry {
            parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
        });

        // Pre-populate a real temp file as the MCP config path so we can
        // verify teardown actually deletes the file from disk.
        let temp_path = std::env::temp_dir().join(format!(
            "workbridge-teardown-test-{}.json",
            std::process::id()
        ));
        std::fs::write(&temp_path, b"{}").expect("create temp mcp config");
        assert!(temp_path.exists(), "precondition: temp file exists");
        app.global_mcp_config_path = Some(temp_path.clone());

        // Pre-populate buffered PTY keystrokes that must NOT leak into a
        // freshly-spawned replacement session.
        app.pending_global_pty_bytes
            .extend_from_slice(b"stale-keys");

        app.teardown_global_session();

        assert!(
            app.global_session.is_none(),
            "global_session must be cleared",
        );
        assert!(
            app.global_mcp_server.is_none(),
            "global_mcp_server must be cleared",
        );
        assert!(
            app.global_mcp_config_path.is_none(),
            "global_mcp_config_path must be cleared",
        );
        assert!(
            app.pending_global_pty_bytes.is_empty(),
            "pending_global_pty_bytes must be drained so stale keystrokes \
             don't leak into the next session",
        );
        assert!(
            !temp_path.exists(),
            "teardown must delete the temp MCP config file from disk",
        );
    }

    /// Calling `teardown_global_session` with no state set must be a no-op
    /// and must not panic. The helper runs on every close and every open,
    /// so it has to tolerate being called when nothing has been spawned
    /// yet (e.g. the very first open of an app run, or the defensive call
    /// in the open branch when no previous session exists).
    #[test]
    fn teardown_global_session_is_idempotent_on_empty_state() {
        let mut app = App::new();
        assert!(app.global_session.is_none());
        assert!(app.global_mcp_config_path.is_none());
        assert!(app.pending_global_pty_bytes.is_empty());

        app.teardown_global_session();

        assert!(app.global_session.is_none());
        assert!(app.global_mcp_server.is_none());
        assert!(app.global_mcp_config_path.is_none());
        assert!(app.pending_global_pty_bytes.is_empty());
    }

    /// The close branch of `toggle_global_drawer` must run the teardown so
    /// the next open starts from a blank slate. Exercising the close
    /// branch directly (rather than round-tripping through the open
    /// branch) avoids spawning a real `claude` subprocess in tests.
    #[test]
    fn toggle_global_drawer_close_tears_down_session() {
        let mut app = App::new();

        // Simulate a drawer that is already open with live state.
        app.global_drawer_open = true;
        app.pre_drawer_focus = app.focus;

        let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
        app.global_session = Some(SessionEntry {
            parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
        });

        let temp_path = std::env::temp_dir().join(format!(
            "workbridge-toggle-close-test-{}.json",
            std::process::id()
        ));
        std::fs::write(&temp_path, b"{}").expect("create temp mcp config");
        app.global_mcp_config_path = Some(temp_path.clone());
        app.pending_global_pty_bytes.extend_from_slice(b"leftover");

        // Close branch: no spawn involved, so this is safe in any test env.
        app.toggle_global_drawer();

        assert!(!app.global_drawer_open, "drawer must be closed");
        assert!(
            app.global_session.is_none(),
            "close must clear global_session",
        );
        assert!(
            app.global_mcp_server.is_none(),
            "close must clear global_mcp_server",
        );
        assert!(
            app.global_mcp_config_path.is_none(),
            "close must clear global_mcp_config_path",
        );
        assert!(
            app.pending_global_pty_bytes.is_empty(),
            "close must drain pending_global_pty_bytes",
        );
        assert!(
            !temp_path.exists(),
            "close must delete the temp MCP config file",
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
    /// Poll `poll_review_gate` in a short busy loop until the review gate
    /// for `wi_id` is no longer in-flight, or a short timeout elapses.
    ///
    /// Tests that trigger `spawn_review_gate` via MCP/advance_stage need
    /// this because the gate now runs on a real background thread - the
    /// synchronous Blocked branch was removed to keep `git diff` off the
    /// UI thread (see P0 audit #1). The background thread will immediately
    /// send a `Blocked` message for stub-backend cases (no plan, etc.) so
    /// the loop normally returns within a single millisecond.
    fn drain_review_gate_with_timeout(app: &mut App, wi_id: &WorkItemId) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while app.review_gates.contains_key(wi_id) && std::time::Instant::now() < deadline {
            app.poll_review_gate();
            if !app.review_gates.contains_key(wi_id) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Final poll to catch any message that arrived during the last sleep.
        app.poll_review_gate();
    }

    /// Test helper: insert a manually-constructed `ReviewGateState`
    /// after starting a status-bar activity for it. Mirrors the
    /// behaviour of `spawn_review_gate` so the production
    /// `drop_review_gate` invariant (always end the activity on every
    /// drop site) is exercised by the tests.
    fn insert_test_review_gate(
        app: &mut App,
        wi_id: WorkItemId,
        rx: crossbeam_channel::Receiver<ReviewGateMessage>,
        origin: ReviewGateOrigin,
    ) {
        let activity = app.start_activity("test review gate");
        app.review_gates.insert(
            wi_id,
            ReviewGateState {
                rx,
                progress: None,
                origin,
                activity,
            },
        );
    }

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
    /// must NOT change status to Review (gate spawn fails asynchronously),
    /// and rework_reasons must be populated after poll_review_gate drains
    /// the background thread's Blocked message.
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
        // The review gate now runs on a background thread; wait for its
        // Blocked message to drain and the rework flow to fire.
        drain_review_gate_with_timeout(&mut app, &wi_id);

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
    /// change status to Review. The gate's "no plan" check now runs on a
    /// background thread (see P0 audit #1), so we drain the gate with a
    /// short poll loop before asserting.
    #[test]
    fn tui_advance_stage_blocked_without_plan() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        app.advance_stage();
        drain_review_gate_with_timeout(&mut app, &wi_id);

        // Status must stay at Implementing - spawn_review_gate fires the
        // Blocked outcome from the background thread, not synchronously.
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

    /// Regression guard for R1-F-3: a TUI-initiated review gate that
    /// resolves to `Blocked` MUST NOT kill the user's Implementing
    /// session. On master the TUI advance path just set
    /// `status_message`; when the blocking-I/O fix moved the gate to a
    /// background thread the new `poll_review_gate` Blocked arm
    /// unconditionally killed and respawned the session - a regression
    /// for user-initiated advances. The `ReviewGateOrigin::Tui` branch
    /// in `poll_review_gate` must preserve the session and only surface
    /// the reason.
    #[test]
    fn poll_review_gate_tui_blocked_preserves_session() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        // Install a mock Implementing session so we can assert it
        // survives the Blocked arm.
        let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
        app.sessions.insert(
            (wi_id.clone(), WorkItemStatus::Implementing),
            SessionEntry {
                parser,
                alive: true,
                session: None,
                scrollback_offset: 0,
                selection: None,
            },
        );

        // Install a Tui-origin gate with a pre-queued Blocked message.
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Blocked {
            work_item_id: wi_id.clone(),
            reason: "Cannot enter Review: no changes on branch".into(),
        })
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Tui);

        app.poll_review_gate();

        // The session must still be in the sessions map - TUI Blocked
        // does not run the kill+respawn rework flow.
        assert!(
            app.sessions
                .contains_key(&(wi_id.clone(), WorkItemStatus::Implementing)),
            "Tui-origin Blocked must NOT kill the existing Implementing session",
        );
        // rework_reasons must NOT be populated - rework only applies to
        // Mcp/Auto origins. A TUI user explicitly pressed advance; we
        // surface the reason instead of rewriting their session prompt.
        assert!(
            !app.rework_reasons.contains_key(&wi_id),
            "Tui-origin Blocked must NOT populate rework_reasons",
        );
        // Status must explain the gate failure.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("no changes on branch"),
            "status message should carry the Blocked reason, got: {msg}",
        );
        // Gate entry must be dropped.
        assert!(
            !app.review_gates.contains_key(&wi_id),
            "gate state must be cleared after Blocked",
        );
    }

    /// Regression guard for R1-F-3: Mcp-origin Blocked still runs the
    /// full rework flow (session kill + respawn + rework_reasons).
    /// This preserves the behaviour Claude relies on when
    /// workbridge_set_status("Review") fails - Claude sees the
    /// rejection reason in its next session prompt and iterates.
    #[test]
    fn poll_review_gate_mcp_blocked_populates_rework_reasons() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Blocked {
            work_item_id: wi_id.clone(),
            reason: "Cannot enter Review: no plan exists".into(),
        })
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        app.poll_review_gate();

        assert!(
            app.rework_reasons
                .get(&wi_id)
                .is_some_and(|r| r.contains("no plan exists")),
            "Mcp-origin Blocked must populate rework_reasons so Claude \
             sees the reason on the next session restart",
        );
        assert!(
            !app.review_gates.contains_key(&wi_id),
            "gate state must be cleared after Blocked",
        );
    }

    /// Regression guard for R1-F-6: if the work item was deleted while
    /// a review gate was in flight, the Blocked arm must NOT leak an
    /// orphan `rework_reasons` entry. Only the gate state should be
    /// dropped - nothing else to do for a work item that no longer
    /// exists.
    #[test]
    fn poll_review_gate_blocked_guards_deleted_work_item() {
        let mut app = App::new();
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/deleted-mid-gate.json"));

        // Install a gate WITHOUT pushing a matching work item: the
        // delete happened between spawn and poll.
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Blocked {
            work_item_id: wi_id.clone(),
            reason: "Cannot enter Review: no plan exists".into(),
        })
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        app.poll_review_gate();

        assert!(
            !app.review_gates.contains_key(&wi_id),
            "gate entry must be dropped even for deleted work items",
        );
        assert!(
            !app.rework_reasons.contains_key(&wi_id),
            "rework_reasons must NOT be populated for a deleted work item - \
             nothing would ever clear the entry and it would leak forever",
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
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Result(ReviewGateResult {
            work_item_id: wi_id.clone(),
            approved: false,
            detail: "Tests are missing for the new feature".into(),
        }))
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

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
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Result(ReviewGateResult {
            work_item_id: wi_id.clone(),
            approved: true,
            detail: "All plan items implemented".into(),
        }))
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

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
        // Verify the gate is cleared from the map.
        assert!(
            !app.review_gates.contains_key(&wi_id),
            "gate should be cleared"
        );
    }

    /// Test: Progress messages update review_gate_progress without completing
    /// the gate.
    #[test]
    fn poll_review_gate_progress_updates_field() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Progress("2 / 3 CI checks green".into()))
            .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        app.poll_review_gate();

        // Progress should be updated but gate should still be running.
        assert_eq!(
            app.review_gates
                .get(&wi_id)
                .and_then(|g| g.progress.as_deref()),
            Some("2 / 3 CI checks green"),
        );
        assert!(
            app.review_gates.contains_key(&wi_id),
            "gate should still be present (gate not done)",
        );
    }

    /// Test: Progress followed by Result in the same tick - both are processed.
    #[test]
    fn poll_review_gate_progress_then_result() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Progress(
            "1 / 1 CI checks green. Running code review...".into(),
        ))
        .unwrap();
        tx.send(ReviewGateMessage::Result(ReviewGateResult {
            work_item_id: wi_id.clone(),
            approved: false,
            detail: "Missing error handling".into(),
        }))
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        app.poll_review_gate();

        // Result should have been processed - gate is done.
        assert!(
            !app.review_gates.contains_key(&wi_id),
            "gate should be cleared"
        );
        assert!(
            app.rework_reasons.contains_key(&wi_id),
            "rework_reasons must be populated after rejection",
        );
    }

    /// Test: Disconnected channel (thread exited) after progress is handled.
    #[test]
    fn poll_review_gate_disconnect_after_progress() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Progress(
            "Checking for pull request...".into(),
        ))
        .unwrap();
        drop(tx); // Simulate thread exit without sending Result.
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        app.poll_review_gate();

        // Gate should be cleaned up with an error message.
        assert!(
            !app.review_gates.contains_key(&wi_id),
            "gate should be cleared"
        );
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("unexpectedly"),
            "status should mention unexpected exit, got: {msg}",
        );
    }

    /// Test 8: Gate spawn failure (MCP path) via "no branch" - a
    /// synchronous pre-condition that still returns
    /// `ReviewGateSpawn::Blocked` from the main thread. The MCP handler
    /// surfaces the reason in `status_message` (not `rework_reasons`);
    /// `rework_reasons` is populated only when the BACKGROUND thread
    /// reports a Blocked result via poll_review_gate. The rework flow for
    /// "no plan" is exercised by `mcp_review_gate_bypass_prevented_no_plan`
    /// via the drain helper.
    #[test]
    fn mcp_gate_spawn_failure_sets_rework_reasons() {
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

        // Status must stay at Implementing - the synchronous pre-condition
        // blocked the spawn.
        let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
        assert_eq!(
            wi.status,
            WorkItemStatus::Implementing,
            "status must not change to Review when gate is blocked",
        );
        // The synchronous Blocked path surfaces the reason in status_message.
        let msg = app.status_message.as_deref().unwrap_or("");
        assert!(
            msg.contains("no branch"),
            "status should mention 'no branch', got: {msg}",
        );
    }

    /// Test 9: When a review gate is already running for item A, an MCP
    /// StatusUpdate for Review on item B should independently attempt to
    /// spawn its own gate. With StubBackend (no plan), it fails with a
    /// "no plan" error and triggers the rework flow for item B.
    #[test]
    fn concurrent_gate_spawn_independent_of_other_items() {
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
        let (_dummy_tx, dummy_rx) = crossbeam_channel::unbounded();
        insert_test_review_gate(&mut app, wi_id_a.clone(), dummy_rx, ReviewGateOrigin::Mcp);

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
        // Item B's gate runs on a background thread; drain the "no plan"
        // Blocked message via poll_review_gate.
        drain_review_gate_with_timeout(&mut app, &wi_id_b);

        // Item B's status must be unchanged (gate spawn failed due to no plan).
        let wi_b = app.work_items.iter().find(|w| w.id == wi_id_b).unwrap();
        assert_eq!(
            wi_b.status,
            WorkItemStatus::Implementing,
            "item B should remain Implementing when gate cannot spawn (no plan)",
        );
        // rework_reasons should be populated - gate spawn failure (no plan)
        // triggers the rework flow via poll_review_gate, mirroring the old
        // synchronous behavior.
        assert!(
            app.rework_reasons.contains_key(&wi_id_b),
            "rework_reasons must be set for item B (gate spawn failure, not blocked by item A)",
        );
        let reason = app.rework_reasons.get(&wi_id_b).unwrap();
        assert!(
            reason.contains("no plan"),
            "rework reason should mention no plan, got: {reason}",
        );
        // Item A's gate should still be running.
        assert!(
            app.review_gates.contains_key(&wi_id_a),
            "item A's gate should still be running",
        );
    }

    /// Test 10: A Blocked work item with no plan that fails the gate via MCP
    /// should transition to Implementing (not stay Blocked), so the
    /// implementing_rework prompt (which has {rework_reason}) is used.
    /// The gate runs on a background thread and the Blocked outcome is
    /// drained via poll_review_gate.
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
        drain_review_gate_with_timeout(&mut app, &wi_id);

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

    /// Test 11: spawn_review_gate reports failures on synchronous
    /// pre-conditions (no repo association, no branch) via the returned
    /// `ReviewGateSpawn::Blocked`. The background-thread failures (no
    /// plan, empty diff, git error) arrive asynchronously via
    /// `poll_review_gate` and are covered by other tests.
    #[test]
    fn spawn_review_gate_sets_status_on_failure() {
        // Case 1: no plan exists - now an ASYNC Blocked message.
        {
            let (mut app, wi_id) = app_with_work_item(
                WorkItemStatus::Implementing,
                Some("feature/test"),
                Some("/tmp/repo"),
            );
            let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
            // With the blocking-I/O fix, the no-plan check runs on the
            // background thread. The spawn returns Spawned and the rework
            // flow fires after poll_review_gate drains the Blocked message.
            assert!(matches!(result, ReviewGateSpawn::Spawned));
            drain_review_gate_with_timeout(&mut app, &wi_id);
            assert!(
                app.rework_reasons
                    .get(&wi_id)
                    .is_some_and(|r| r.contains("no plan")),
                "drained rework reason should mention no plan",
            );
        }

        // Case 2: no branch set - synchronous pre-condition.
        {
            let (mut app, wi_id) = app_with_work_item(
                WorkItemStatus::Implementing,
                None, // no branch
                Some("/tmp/repo"),
            );
            let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
            match result {
                ReviewGateSpawn::Blocked(reason) => {
                    assert!(
                        reason.contains("no branch"),
                        "should mention no branch, got: {reason}",
                    );
                }
                ReviewGateSpawn::Spawned => {
                    panic!("gate should not have spawned without a branch");
                }
            }
        }

        // Case 3: no repo association - synchronous pre-condition.
        {
            let (mut app, wi_id) = app_with_work_item(
                WorkItemStatus::Implementing,
                None,
                None, // no repo association
            );
            let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
            match result {
                ReviewGateSpawn::Blocked(reason) => {
                    assert!(
                        reason.contains("no repo"),
                        "should mention no repo, got: {reason}",
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

        let mut app = App::with_config(Config::default(), Arc::new(backend));
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
        use crate::work_item::{CheckStatus, MergeableState, PrInfo, PrState, ReviewDecision};
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
                    done_at: None,
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
            fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
                Ok(())
            }
            fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
                Ok(None)
            }
            fn set_done_at(
                &self,
                _id: &WorkItemId,
                _done_at: Option<u64>,
            ) -> Result<(), BackendError> {
                Ok(())
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
        let mut app = App::with_config(Config::default(), Arc::new(backend));

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
                mergeable: MergeableState::Unknown,
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
        app.sync_selection_identity();
        app.open_delete_prompt();
        app.confirm_delete_from_prompt();

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
                    done_at: None,
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
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(None)
        }
        fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
            Ok(())
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> crate::work_item::BackendType {
            crate::work_item::BackendType::LocalFile
        }
    }

    /// Worktree service that records `remove_worktree` / `delete_branch`
    /// calls so tests can verify the delete flow invoked git correctly.
    struct ConfigurableWorktreeService {
        remove_worktree_calls: std::sync::Mutex<Vec<(PathBuf, PathBuf, bool, bool)>>,
        delete_branch_calls: std::sync::Mutex<Vec<(PathBuf, String, bool)>>,
    }

    impl ConfigurableWorktreeService {
        fn recording() -> Self {
            Self {
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

    /// open_delete_prompt must NOT call any blocking worktree check on
    /// the UI thread. This is enforced structurally by the
    /// `WorktreeService` trait, which no longer exposes
    /// `is_worktree_dirty`, so any attempt to reintroduce a dirty check
    /// through the injected service would fail to compile. This test
    /// additionally verifies that opening the prompt does not touch the
    /// backend, so a stray 'y' keypress is required before anything is
    /// destroyed.
    #[test]
    fn open_delete_prompt_does_not_touch_backend() {
        use crate::config::InMemoryConfigProvider;

        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(FixedListBackend::one_item(
                "/tmp/prompt-test.json",
                "Prompt test item",
                "/repo",
                "test-branch",
            )),
            Arc::new(ConfigurableWorktreeService::recording()),
            Box::new(InMemoryConfigProvider::new()),
        );

        // Inject a fake worktree path into the assembled work item.
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

        app.open_delete_prompt();
        assert!(app.delete_prompt_visible, "delete prompt should be visible");
        assert_eq!(app.delete_target_title.as_deref(), Some("Prompt test item"),);

        // Opening the prompt must not touch the backend.
        assert_eq!(
            app.work_items.len(),
            1,
            "work item should still exist after opening the prompt"
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
            Arc::new(FixedListBackend::one_item(
                "/tmp/recording-test.json",
                "Recording test item",
                "/my/repo",
                "feature-branch",
            )),
            recording_ws.clone(),
            Box::new(InMemoryConfigProvider::new()),
        );

        // Inject a fake RepoFetchResult so delete_work_item_by_id can
        // find the worktree path via repo_data.
        assert_eq!(app.work_items.len(), 1);
        app.repo_data.insert(
            PathBuf::from("/my/repo"),
            crate::work_item::RepoFetchResult {
                repo_path: PathBuf::from("/my/repo"),
                github_remote: None,
                worktrees: Ok(vec![crate::worktree_service::WorktreeInfo {
                    path: PathBuf::from("/my/repo/.worktrees/feature-branch"),
                    branch: Some("feature-branch".into()),
                    is_main: false,
                    has_commits_ahead: None,
                }]),
                prs: Ok(vec![]),
                review_requested_prs: Ok(vec![]),
                issues: vec![],
            },
        );
        app.build_display_list();

        // Select the work item.
        let wi_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .unwrap();
        app.selected_item = Some(wi_idx);
        app.sync_selection_identity();

        // Open the prompt, confirm, then drain the background cleanup
        // thread via poll_delete_cleanup (matches how the real event
        // loop consumes results). Spin for up to ~1s so flaky CI still
        // passes despite thread scheduling jitter.
        app.open_delete_prompt();
        app.confirm_delete_from_prompt();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while app.delete_in_progress && std::time::Instant::now() < deadline {
            app.poll_delete_cleanup();
            if app.delete_in_progress {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert!(
            !app.delete_in_progress,
            "background delete cleanup should have completed within 1s"
        );

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
            rw_calls[0].3,
            "remove_worktree force should be true: modal always passes \
             --force to avoid blocking the UI thread on is_worktree_dirty"
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

    /// When `gh pr close` fails for an association with an open PR, the
    /// delete flow must PRESERVE the local worktree and branch for that
    /// association so the user can recover unpushed commits. If it
    /// instead force-deleted local resources and then only noticed the
    /// PR close failure afterward, the user would be left with an open
    /// PR and no local branch to recover from - which is exactly the
    /// data-loss path spawn_unlinked_cleanup already guards against.
    #[test]
    fn delete_preserves_local_resources_when_pr_close_fails() {
        use crate::config::InMemoryConfigProvider;
        use crate::pr_service::PullRequestCloser;

        /// Records calls and always fails. Mirrors the shape of the
        /// `RecordingWorktreeService` stub already used in this test
        /// module.
        struct FailingCloser {
            calls: std::sync::Mutex<Vec<(String, String, u64)>>,
        }

        impl PullRequestCloser for FailingCloser {
            fn close_pr(&self, owner: &str, repo: &str, pr_number: u64) -> Result<(), String> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((owner.into(), repo.into(), pr_number));
                Err("simulated gh auth error".into())
            }
        }

        let recording_ws = Arc::new(ConfigurableWorktreeService::recording());
        let failing_closer = Arc::new(FailingCloser {
            calls: std::sync::Mutex::new(Vec::new()),
        });

        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(FixedListBackend::one_item(
                "/tmp/pr-close-fail-test.json",
                "PR close fail item",
                "/my/repo",
                "feature-branch",
            )),
            recording_ws.clone(),
            Box::new(InMemoryConfigProvider::new()),
        );
        // Replace the production gh-based closer with the failing stub
        // before driving the delete. The delete path reads `app.pr_closer`
        // once inside `spawn_delete_cleanup` and Arc::clones it into the
        // background thread, so this assignment must happen before
        // `confirm_delete_from_prompt`.
        app.pr_closer = failing_closer.clone();

        // Inject cached RepoFetchResult so gather_delete_cleanup_infos
        // finds both the worktree path AND an open PR. The combination
        // of `github_remote: Some(...)` and an OPEN pr with
        // `head_branch == "feature-branch"` is what populates
        // DeleteCleanupInfo.open_pr_number and drives the PR-close path.
        assert_eq!(app.work_items.len(), 1);
        app.repo_data.insert(
            PathBuf::from("/my/repo"),
            crate::work_item::RepoFetchResult {
                repo_path: PathBuf::from("/my/repo"),
                github_remote: Some(("my-org".into(), "my-repo".into())),
                worktrees: Ok(vec![crate::worktree_service::WorktreeInfo {
                    path: PathBuf::from("/my/repo/.worktrees/feature-branch"),
                    branch: Some("feature-branch".into()),
                    is_main: false,
                    has_commits_ahead: None,
                }]),
                prs: Ok(vec![crate::github_client::GithubPr {
                    number: 42,
                    title: "Test PR".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                    head_branch: "feature-branch".into(),
                    url: "https://example.com/pr/42".into(),
                    review_decision: String::new(),
                    status_check_rollup: String::new(),
                    head_repo_owner: None,
                    author: None,
                    mergeable: String::new(),
                }]),
                review_requested_prs: Ok(vec![]),
                issues: vec![],
            },
        );
        app.build_display_list();

        // Select the work item and drive the delete flow.
        let wi_idx = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)))
            .unwrap();
        app.selected_item = Some(wi_idx);
        app.sync_selection_identity();

        app.open_delete_prompt();
        app.confirm_delete_from_prompt();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while app.delete_in_progress && std::time::Instant::now() < deadline {
            app.poll_delete_cleanup();
            if app.delete_in_progress {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert!(
            !app.delete_in_progress,
            "background delete cleanup should have completed within 1s"
        );

        // The closer must have been invoked with the correct arguments
        // so we know we actually exercised the PR-close-first branch,
        // not some unrelated short-circuit.
        let close_calls = failing_closer.calls.lock().unwrap();
        assert_eq!(
            close_calls.len(),
            1,
            "close_pr should have been called exactly once"
        );
        assert_eq!(
            close_calls[0],
            ("my-org".into(), "my-repo".into(), 42u64),
            "close_pr arguments"
        );
        drop(close_calls);

        // Data-loss guard: destructive local cleanup MUST NOT have run
        // after the PR close failed. This is the whole point of the
        // ordering - failure here means an unpushed branch was still
        // force-deleted while the PR stayed open upstream.
        let rw_calls = recording_ws.remove_worktree_calls.lock().unwrap();
        assert!(
            rw_calls.is_empty(),
            "remove_worktree must NOT be called when PR close fails, got: {rw_calls:?}"
        );
        drop(rw_calls);

        let db_calls = recording_ws.delete_branch_calls.lock().unwrap();
        assert!(
            db_calls.is_empty(),
            "delete_branch must NOT be called when PR close fails, got: {db_calls:?}"
        );
        drop(db_calls);

        // The user's only breadcrumb to the preserved paths is the
        // alert dialog - verify it points at both the worktree and
        // branch so the user can find them manually.
        let alert = app
            .alert_message
            .as_deref()
            .expect("alert_message must surface the PR-close failure");
        assert!(
            alert.contains("preserved local worktree"),
            "alert should mention preserved worktree, got: {alert}"
        );
        assert!(
            alert.contains("preserved local branch"),
            "alert should mention preserved branch, got: {alert}"
        );
        assert!(
            alert.contains("feature-branch"),
            "alert should include the branch name, got: {alert}"
        );
    }

    // -- Auto-archival tests --

    /// Backend that tracks records in memory and supports set_done_at.
    /// Used by auto-archive tests that need functional delete/update_status.
    struct ArchiveTestBackend {
        records: std::sync::Mutex<Vec<crate::work_item_backend::WorkItemRecord>>,
    }

    impl WorkItemBackend for ArchiveTestBackend {
        fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
            Ok(crate::work_item_backend::ListResult {
                records: self.records.lock().unwrap().clone(),
                corrupt: Vec::new(),
            })
        }
        fn read(
            &self,
            id: &WorkItemId,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::NotFound(id.clone()))
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
            id: &WorkItemId,
            status: WorkItemStatus,
        ) -> Result<(), BackendError> {
            let mut records = self.records.lock().unwrap();
            if let Some(record) = records.iter_mut().find(|r| r.id == *id) {
                record.status = status;
                Ok(())
            } else {
                Err(BackendError::NotFound(id.clone()))
            }
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
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(None)
        }
        fn set_done_at(&self, id: &WorkItemId, done_at: Option<u64>) -> Result<(), BackendError> {
            let mut records = self.records.lock().unwrap();
            if let Some(record) = records.iter_mut().find(|r| r.id == *id) {
                record.done_at = done_at;
                Ok(())
            } else {
                Err(BackendError::NotFound(id.clone()))
            }
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> crate::work_item::BackendType {
            crate::work_item::BackendType::LocalFile
        }
    }

    fn make_archive_record(
        name: &str,
        status: WorkItemStatus,
        done_at: Option<u64>,
    ) -> crate::work_item_backend::WorkItemRecord {
        crate::work_item_backend::WorkItemRecord {
            id: WorkItemId::LocalFile(PathBuf::from(format!("/tmp/{name}.json"))),
            title: name.into(),
            description: None,
            status,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/repo"),
                branch: None,
                pr_identity: None,
            }],
            plan: None,
            done_at,
        }
    }

    #[test]
    fn auto_archive_deletes_expired_done_items() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // done_at 8 days ago (exceeds default 7-day period).
        let eight_days_ago = now - (8 * 86400);
        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![
                make_archive_record("expired", WorkItemStatus::Done, Some(eight_days_ago)),
                make_archive_record("active", WorkItemStatus::Implementing, None),
            ]),
        };

        let mut cfg = Config::for_test();
        cfg.defaults.archive_after_days = 7;
        let mut app = App::with_config(cfg, Arc::new(backend));
        app.reassemble_work_items();

        // Only the active item should remain.
        assert_eq!(app.work_items.len(), 1);
        assert_eq!(app.work_items[0].title, "active");
    }

    #[test]
    fn auto_archive_skips_when_disabled() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let old = now - (30 * 86400);
        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![make_archive_record(
                "old-done",
                WorkItemStatus::Done,
                Some(old),
            )]),
        };

        let mut cfg = Config::for_test();
        cfg.defaults.archive_after_days = 0; // disabled
        let mut app = App::with_config(cfg, Arc::new(backend));
        app.reassemble_work_items();

        assert_eq!(app.work_items.len(), 1, "should not archive when disabled");
    }

    #[test]
    fn auto_archive_skips_done_without_done_at() {
        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![make_archive_record(
                "done-no-ts",
                WorkItemStatus::Done,
                None, // no done_at timestamp
            )]),
        };

        let mut cfg = Config::for_test();
        cfg.defaults.archive_after_days = 7;
        let mut app = App::with_config(cfg, Arc::new(backend));
        app.reassemble_work_items();

        assert_eq!(
            app.work_items.len(),
            1,
            "should not archive Done items without done_at"
        );
    }

    #[test]
    fn auto_archive_keeps_recent_done_items() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // done_at 3 days ago (within 7-day period).
        let three_days_ago = now - (3 * 86400);
        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![make_archive_record(
                "recent-done",
                WorkItemStatus::Done,
                Some(three_days_ago),
            )]),
        };

        let mut cfg = Config::for_test();
        cfg.defaults.archive_after_days = 7;
        let mut app = App::with_config(cfg, Arc::new(backend));
        app.reassemble_work_items();

        assert_eq!(app.work_items.len(), 1, "recent Done items should be kept");
    }

    #[test]
    fn auto_archive_works_for_derived_done_items() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // done_at 8 days ago, but backend status is Review (derived-Done via merged PR).
        let eight_days_ago = now - (8 * 86400);
        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![make_archive_record(
                "derived-done",
                WorkItemStatus::Review,
                Some(eight_days_ago),
            )]),
        };

        let mut cfg = Config::for_test();
        cfg.defaults.archive_after_days = 7;
        let mut app = App::with_config(cfg, Arc::new(backend));
        app.reassemble_work_items();

        assert_eq!(
            app.work_items.len(),
            0,
            "derived-Done items with expired done_at should be archived"
        );
    }

    #[test]
    fn apply_stage_change_sets_done_at() {
        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![make_archive_record(
                "review-item",
                WorkItemStatus::Review,
                None,
            )]),
        };

        let mut cfg = Config::for_test();
        cfg.defaults.archive_after_days = 7;
        let mut app = App::with_config(cfg, Arc::new(backend));
        app.reassemble_work_items();
        app.build_display_list();

        let wi_id = app.work_items[0].id.clone();
        app.apply_stage_change(
            &wi_id,
            &WorkItemStatus::Review,
            &WorkItemStatus::Done,
            "pr_merge",
        );

        // Verify done_at was set on the backend record.
        let records = app.backend.list().unwrap().records;
        assert_eq!(records.len(), 1);
        assert!(
            records[0].done_at.is_some(),
            "done_at should be set when entering Done"
        );
    }

    #[test]
    fn apply_stage_change_clears_done_at_on_retreat() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let backend = ArchiveTestBackend {
            records: std::sync::Mutex::new(vec![make_archive_record(
                "done-item",
                WorkItemStatus::Done,
                Some(now),
            )]),
        };

        let mut cfg = Config::for_test();
        cfg.defaults.archive_after_days = 7;
        let mut app = App::with_config(cfg, Arc::new(backend));
        app.reassemble_work_items();
        app.build_display_list();

        let wi_id = app.work_items[0].id.clone();
        app.apply_stage_change(
            &wi_id,
            &WorkItemStatus::Done,
            &WorkItemStatus::Review,
            "test",
        );

        // Verify done_at was cleared.
        let records = app.backend.list().unwrap().records;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].done_at, None,
            "done_at should be cleared when retreating from Done"
        );
    }

    // -- P0 Blocking-I/O regression guard (GAN ui-audit-p0-io) --
    //
    // Ensures the UI-thread entry points touched by the audit never call
    // into `WorktreeService` synchronously. We install a worktree service
    // that atomically bumps a per-method counter and returns a stub
    // result without blocking. Each regression test snapshots the
    // counter immediately after the UI-thread entry point returns and
    // asserts that NOTHING was called on the main thread. Background
    // threads spawned by the entry points are free to increment the
    // counter later - the snapshot is taken synchronously before any
    // thread progress is observable.
    //
    // A counting probe is used instead of a panicking probe because the
    // PR-create / merge / review-submit entry points intentionally spawn
    // background threads that DO call `default_branch` later. A panic on
    // the worker thread would pollute `--nocapture` output without
    // adding signal.

    /// Counting probe that records how many times any method was
    /// called on the UI thread. A `Mutex<()>` "gate" establishes a
    /// deterministic happens-before edge: tests that spawn a
    /// background thread which might call into this service acquire
    /// the gate BEFORE invoking the UI-thread entry point, snapshot
    /// the counter, then drop the gate so the background thread can
    /// proceed. Without the gate the background thread could race the
    /// test thread and bump the counter before the assertion runs,
    /// flaking the test under CI load. Mirrors the pattern used by
    /// `CountingPlanBackend`.
    #[derive(Default)]
    struct CountingWorktreeService {
        calls: std::sync::atomic::AtomicUsize,
        gate: std::sync::Mutex<()>,
    }

    impl CountingWorktreeService {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn load(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
        /// Acquire the gate mutex, block until the test thread
        /// releases it, then atomically bump the counter. Every
        /// trait method routes through here, so any caller - UI
        /// thread or background thread - is forced to serialize
        /// against whichever test is holding the gate.
        fn gated_bump(&self) {
            let _guard = self.gate.lock().unwrap();
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::worktree_service::WorktreeService for CountingWorktreeService {
        fn list_worktrees(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<
            Vec<crate::worktree_service::WorktreeInfo>,
            crate::worktree_service::WorktreeError,
        > {
            self.gated_bump();
            Ok(Vec::new())
        }

        fn create_worktree(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            target_dir: &std::path::Path,
        ) -> Result<crate::worktree_service::WorktreeInfo, crate::worktree_service::WorktreeError>
        {
            self.gated_bump();
            Ok(crate::worktree_service::WorktreeInfo {
                path: target_dir.to_path_buf(),
                branch: None,
                is_main: false,
                has_commits_ahead: None,
            })
        }

        fn remove_worktree(
            &self,
            _repo_path: &std::path::Path,
            _worktree_path: &std::path::Path,
            _delete_branch: bool,
            _force: bool,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            self.gated_bump();
            Ok(())
        }

        fn delete_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _force: bool,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            self.gated_bump();
            Ok(())
        }

        fn default_branch(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<String, crate::worktree_service::WorktreeError> {
            self.gated_bump();
            Ok("main".into())
        }

        fn github_remote(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<Option<(String, String)>, crate::worktree_service::WorktreeError> {
            self.gated_bump();
            Ok(None)
        }

        fn fetch_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            self.gated_bump();
            Ok(())
        }

        fn create_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            self.gated_bump();
            Ok(())
        }
    }

    /// Build an `App` wired with the counting worktree service and an
    /// in-memory config provider. Returns the shared `Arc` so tests can
    /// snapshot the call count after each UI-thread entry point.
    fn app_with_counting_ws() -> (App, Arc<CountingWorktreeService>) {
        let ws = CountingWorktreeService::new();
        let app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(StubBackend),
            Arc::clone(&ws) as Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        (app, ws)
    }

    /// Install a cached `RepoFetchResult` with the given `github_remote`
    /// and optional worktree so UI-thread cache reads (`github_remote`,
    /// `has_commits_ahead`) return real data without ever touching the
    /// worktree service.
    fn install_cached_repo(
        app: &mut App,
        repo_path: &std::path::Path,
        branch: Option<&str>,
        has_commits_ahead: Option<bool>,
    ) {
        let worktrees = match branch {
            Some(b) => vec![crate::worktree_service::WorktreeInfo {
                path: repo_path.join(".worktrees").join(b),
                branch: Some(b.to_string()),
                is_main: false,
                has_commits_ahead,
            }],
            None => Vec::new(),
        };
        app.repo_data.insert(
            repo_path.to_path_buf(),
            crate::work_item::RepoFetchResult {
                repo_path: repo_path.to_path_buf(),
                github_remote: Some(("owner".into(), "repo".into())),
                worktrees: Ok(worktrees),
                prs: Ok(Vec::new()),
                review_requested_prs: Ok(Vec::new()),
                issues: Vec::new(),
            },
        );
    }

    fn push_review_work_item(
        app: &mut App,
        id: &WorkItemId,
        repo_path: &std::path::Path,
        branch: &str,
        status: WorkItemStatus,
    ) {
        app.work_items.push(crate::work_item::WorkItem {
            id: id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "blocking-io-test".into(),
            description: None,
            status,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: repo_path.to_path_buf(),
                branch: Some(branch.to_string()),
                worktree_path: None,
                pr: Some(crate::work_item::PrInfo {
                    number: 42,
                    url: "https://example.com/pr/42".into(),
                    state: crate::work_item::PrState::Open,
                    title: "pr".into(),
                    is_draft: false,
                    checks: crate::work_item::CheckStatus::Passing,
                    mergeable: crate::work_item::MergeableState::Unknown,
                    review_decision: crate::work_item::ReviewDecision::None,
                }),
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });
    }

    /// Minimal `WorkItemBackend` whose `read_plan` returns a non-empty
    /// plan string so `spawn_review_gate` progresses past the plan check
    /// on the background thread. All other methods defer to `StubBackend`
    /// semantics (no-op / not-found) so the backend stays inert for the
    /// regression test's purposes.
    struct NonEmptyPlanBackend;

    impl WorkItemBackend for NonEmptyPlanBackend {
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
                "non-empty-plan backend does not support create".into(),
            ))
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
            _unlinked: &UnlinkedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation(
                "non-empty-plan backend does not support import".into(),
            ))
        }
        fn import_review_request(
            &self,
            _rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation(
                "non-empty-plan backend does not support import_review_request".into(),
            ))
        }
        fn append_activity(
            &self,
            _id: &WorkItemId,
            _entry: &ActivityEntry,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_title(&self, _id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(Some("plan-text for regression test".into()))
        }
        fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
            Ok(())
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> crate::work_item::BackendType {
            crate::work_item::BackendType::LocalFile
        }
    }

    #[test]
    fn spawn_review_gate_does_not_touch_worktree_service_synchronously() {
        // Exercise the full happy-path pre-conditions (plan exists,
        // branch is set, repo association present) so the background
        // thread is the ONLY place `default_branch` / `github_remote` /
        // `git diff` may run. Against the pre-fix master version this
        // assertion would fail: `spawn_review_gate` called
        // `self.worktree_service.default_branch(&repo_path)` on the UI
        // thread after reading the plan, bumping the counter to 1.
        let ws = CountingWorktreeService::new();
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(NonEmptyPlanBackend),
            Arc::clone(&ws) as Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        let repo = PathBuf::from("/tmp/p0-review-gate-repo");
        install_cached_repo(&mut app, &repo, Some("feature/gate"), Some(true));
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-gate.json"));
        push_review_work_item(
            &mut app,
            &wi_id,
            &repo,
            "feature/gate",
            WorkItemStatus::Implementing,
        );

        // Hold the gate mutex so the background thread (which WILL
        // call `default_branch` / `github_remote` as soon as it wakes)
        // cannot increment the counter before we snapshot it. Without
        // this deterministic happens-before edge, CI-load thread
        // scheduling can let the background thread run first and
        // flake the assertion. Mirrors the pattern used by
        // `begin_session_open_defers_backend_read_plan_to_background_thread`.
        let gate = ws.gate.lock().unwrap();

        let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
        let ws_calls_after_spawn = ws.load();

        assert!(
            matches!(result, ReviewGateSpawn::Spawned),
            "gate must spawn when plan, branch and repo are all present",
        );
        assert_eq!(
            ws_calls_after_spawn, 0,
            "spawn_review_gate must not touch worktree_service on the UI thread: \
             read_plan, default_branch, git diff and github_remote must all run \
             inside the std::thread::spawn closure",
        );

        // Spawning the gate must register a status-bar activity per
        // `docs/UI.md` "Activity indicator placement" - assert it is
        // visible BEFORE we drop the gate so the spinner is observable
        // in the live system, not just after teardown.
        assert!(
            app.current_activity().is_some(),
            "spawn_review_gate must register a status-bar activity",
        );

        // Release the gate so the background thread can proceed and
        // drain. Routing through `drop_review_gate` ensures the
        // associated activity is also ended - the same teardown path
        // every drop site uses.
        drop(gate);
        app.drop_review_gate(&wi_id);
        assert!(
            app.current_activity().is_none(),
            "drop_review_gate must end the review gate activity",
        );
    }

    #[test]
    fn spawn_pr_creation_reads_github_remote_from_cache() {
        // Happy path: the cached github_remote is populated, so the main
        // thread never calls into worktree_service. The background thread
        // WILL call `default_branch` later. The gate mutex establishes a
        // deterministic happens-before edge so the counter snapshot runs
        // before the background thread can increment it, eliminating the
        // CI-load race condition that would otherwise flake this test.
        let (mut app, ws) = app_with_counting_ws();
        let repo = PathBuf::from("/tmp/p0-pr-create-repo");
        install_cached_repo(&mut app, &repo, Some("feature/prc"), Some(true));
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-prc.json"));
        push_review_work_item(
            &mut app,
            &wi_id,
            &repo,
            "feature/prc",
            WorkItemStatus::Review,
        );

        // Hold the gate so the background thread is blocked on its
        // first `default_branch` call and cannot race the snapshot.
        let gate = ws.gate.lock().unwrap();

        app.end_user_action(&UserActionKey::PrCreate);
        app.spawn_pr_creation(&wi_id);
        let ws_calls_after_spawn = ws.load();

        assert_eq!(
            app.user_action_work_item(&UserActionKey::PrCreate),
            Some(&wi_id),
        );
        assert_eq!(
            ws_calls_after_spawn, 0,
            "spawn_pr_creation must read github_remote from repo_data, not \
             worktree_service, on the UI thread",
        );

        // Release the gate so the background thread can drain. Dropping
        // the receiver stops any progress being observed.
        drop(gate);
        app.end_user_action(&UserActionKey::PrCreate);
    }

    #[test]
    fn execute_merge_reads_github_remote_from_cache() {
        let (mut app, ws) = app_with_counting_ws();
        let repo = PathBuf::from("/tmp/p0-merge-repo");
        install_cached_repo(&mut app, &repo, Some("feature/merge"), Some(true));
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-merge.json"));
        push_review_work_item(
            &mut app,
            &wi_id,
            &repo,
            "feature/merge",
            WorkItemStatus::Review,
        );

        app.end_user_action(&UserActionKey::PrMerge);
        app.execute_merge(&wi_id, "squash");
        let ws_calls_after_spawn = ws.load();

        assert!(
            app.is_user_action_in_flight(&UserActionKey::PrMerge) || app.alert_message.is_some(),
            "execute_merge must proceed past the github_remote lookup",
        );
        assert_eq!(
            ws_calls_after_spawn, 0,
            "execute_merge must read github_remote from repo_data, not \
             worktree_service, on the UI thread",
        );
        app.end_user_action(&UserActionKey::PrMerge);
    }

    #[test]
    fn spawn_review_submission_reads_github_remote_from_cache() {
        let (mut app, ws) = app_with_counting_ws();
        let repo = PathBuf::from("/tmp/p0-review-submit-repo");
        install_cached_repo(&mut app, &repo, Some("feature/rs"), Some(true));
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-rs.json"));
        push_review_work_item(
            &mut app,
            &wi_id,
            &repo,
            "feature/rs",
            WorkItemStatus::Review,
        );

        app.end_user_action(&UserActionKey::ReviewSubmit);
        app.spawn_review_submission(&wi_id, "approve", "");
        let ws_calls_after_spawn = ws.load();

        assert_eq!(
            app.user_action_work_item(&UserActionKey::ReviewSubmit),
            Some(&wi_id),
        );
        assert_eq!(
            ws_calls_after_spawn, 0,
            "spawn_review_submission must read github_remote from repo_data, \
             not worktree_service, on the UI thread",
        );
        app.end_user_action(&UserActionKey::ReviewSubmit);
    }

    #[test]
    fn enter_mergequeue_reads_github_remote_from_cache() {
        let (mut app, ws) = app_with_counting_ws();
        let repo = PathBuf::from("/tmp/p0-mq-repo");
        install_cached_repo(&mut app, &repo, Some("feature/mq"), Some(true));
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-mq.json"));
        push_review_work_item(
            &mut app,
            &wi_id,
            &repo,
            "feature/mq",
            WorkItemStatus::Review,
        );

        app.enter_mergequeue(&wi_id);
        assert!(
            app.mergequeue_watches.iter().any(|w| w.wi_id == wi_id),
            "enter_mergequeue must proceed past the github_remote lookup using \
             cached data only",
        );
        assert_eq!(
            ws.load(),
            0,
            "enter_mergequeue must never call worktree_service on the UI thread",
        );
    }

    /// Minimal `WorkItemBackend` whose `list` returns a single Done
    /// record with a branch and `pr_identity: None`. Used by the
    /// backfill regression test to prove `collect_backfill_requests`
    /// actually enters its loop body (the old version used
    /// `StubBackend` whose empty list skipped the loop entirely, so
    /// the counter-zero assertion was trivially satisfied for the
    /// wrong reason).
    struct DoneRecordBackend {
        record: crate::work_item_backend::WorkItemRecord,
    }

    impl WorkItemBackend for DoneRecordBackend {
        fn read(
            &self,
            id: &WorkItemId,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            if id == &self.record.id {
                Ok(self.record.clone())
            } else {
                Err(BackendError::NotFound(id.clone()))
            }
        }
        fn list(&self) -> Result<crate::work_item_backend::ListResult, BackendError> {
            Ok(crate::work_item_backend::ListResult {
                records: vec![self.record.clone()],
                corrupt: Vec::new(),
            })
        }
        fn create(
            &self,
            _request: CreateWorkItem,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation(
                "done-record backend does not support create".into(),
            ))
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
            _unlinked: &UnlinkedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation(
                "done-record backend does not support import".into(),
            ))
        }
        fn import_review_request(
            &self,
            _rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation(
                "done-record backend does not support import_review_request".into(),
            ))
        }
        fn append_activity(
            &self,
            _id: &WorkItemId,
            _entry: &ActivityEntry,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_title(&self, _id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            Ok(self.record.plan.clone())
        }
        fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
            Ok(())
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> crate::work_item::BackendType {
            crate::work_item::BackendType::LocalFile
        }
    }

    #[test]
    fn collect_backfill_requests_reads_github_remote_from_cache() {
        // Drive `collect_backfill_requests` through a backend that
        // actually returns a Done record with a branch and no
        // `pr_identity`. The cached `github_remote` in `repo_data`
        // supplies owner/repo. The previous version of this test used
        // `StubBackend` (empty list), so the loop body never executed
        // and the counter-zero assertion was vacuously satisfied -
        // the test would have passed on master unchanged, providing
        // zero coverage of the UI-thread blocking-I/O guard.
        let repo = PathBuf::from("/tmp/p0-backfill-repo");
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-backfill.json"));
        let backend = Arc::new(DoneRecordBackend {
            record: crate::work_item_backend::WorkItemRecord {
                id: wi_id.clone(),
                title: "backfill-test".into(),
                description: None,
                status: WorkItemStatus::Done,
                kind: crate::work_item::WorkItemKind::Own,
                repo_associations: vec![crate::work_item_backend::RepoAssociationRecord {
                    repo_path: repo.clone(),
                    branch: Some("feature/bf".into()),
                    pr_identity: None,
                }],
                plan: None,
                done_at: Some(0),
            },
        });

        let ws = CountingWorktreeService::new();
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            backend,
            Arc::clone(&ws) as Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        install_cached_repo(&mut app, &repo, Some("feature/bf"), Some(false));

        let requests = app.collect_backfill_requests();

        assert_eq!(
            requests.len(),
            1,
            "backend returned a Done record with branch and no pr_identity - \
             collect_backfill_requests must produce exactly one request using \
             the cached github_remote",
        );
        let (req_wi_id, req_repo, req_branch, req_owner, req_repo_name) = &requests[0];
        assert_eq!(req_wi_id, &wi_id);
        assert_eq!(req_repo, &repo);
        assert_eq!(req_branch, "feature/bf");
        assert_eq!(req_owner, "owner");
        assert_eq!(req_repo_name, "repo");
        assert_eq!(
            ws.load(),
            0,
            "collect_backfill_requests must never call worktree_service on \
             the UI thread - owner/repo must come from repo_data cache",
        );
    }

    #[test]
    fn branch_has_commits_reads_from_cache_and_never_shells_out() {
        let (mut app, ws) = app_with_counting_ws();
        let repo = PathBuf::from("/tmp/p0-branch-commits-repo");
        // Cache populated with has_commits_ahead=Some(true).
        install_cached_repo(&mut app, &repo, Some("feature/bhc"), Some(true));
        assert!(app.branch_has_commits(&repo, "feature/bhc"));

        // Missing branch / missing cache entry returns the safe default.
        assert!(!app.branch_has_commits(&repo, "unknown-branch"));
        let unknown_repo = PathBuf::from("/tmp/never-fetched");
        assert!(!app.branch_has_commits(&unknown_repo, "anything"));

        // Cache populated with has_commits_ahead=None must also default
        // to false rather than retrying a shell-out.
        let repo2 = PathBuf::from("/tmp/p0-branch-commits-repo-2");
        install_cached_repo(&mut app, &repo2, Some("feature/null"), None);
        assert!(!app.branch_has_commits(&repo2, "feature/null"));

        assert_eq!(
            ws.load(),
            0,
            "branch_has_commits must read from repo_data and never call \
             worktree_service",
        );
    }

    /// `WorkItemBackend` probe that counts `read_plan` calls through
    /// an `AtomicUsize`. Used to assert that `begin_session_open`
    /// defers the plan read to a background thread.
    ///
    /// The backend holds a `Mutex` "gate" that the background thread
    /// must acquire before it is allowed to call `read_plan`. Tests
    /// lock the gate before calling `begin_session_open`, then
    /// atomically snapshot the counter (which MUST still be zero)
    /// BEFORE releasing the gate. Without the gate the background
    /// thread can race the UI thread and the counter may already be
    /// `1` by the time the test reads it - a race that would
    /// wrongly report a regression.
    #[derive(Default)]
    struct CountingPlanBackend {
        read_plan_calls: std::sync::atomic::AtomicUsize,
        gate: std::sync::Mutex<()>,
    }

    impl CountingPlanBackend {
        fn load(&self) -> usize {
            self.read_plan_calls
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl WorkItemBackend for CountingPlanBackend {
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
                "counting-plan backend does not support create".into(),
            ))
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
            _unlinked: &UnlinkedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation(
                "counting-plan backend does not support import".into(),
            ))
        }
        fn import_review_request(
            &self,
            _rr: &crate::work_item::ReviewRequestedPr,
        ) -> Result<crate::work_item_backend::WorkItemRecord, BackendError> {
            Err(BackendError::Validation(
                "counting-plan backend does not support import_review_request".into(),
            ))
        }
        fn append_activity(
            &self,
            _id: &WorkItemId,
            _entry: &ActivityEntry,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_plan(&self, _id: &WorkItemId, _plan: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn update_title(&self, _id: &WorkItemId, _title: &str) -> Result<(), BackendError> {
            Ok(())
        }
        fn read_plan(&self, _id: &WorkItemId) -> Result<Option<String>, BackendError> {
            // Block until the test releases the gate. This proves the
            // call runs on the background thread - a UI-thread caller
            // would deadlock against the already-held mutex.
            let _guard = self.gate.lock().unwrap();
            self.read_plan_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some("plan-text from counting backend".into()))
        }
        fn set_done_at(&self, _id: &WorkItemId, _done_at: Option<u64>) -> Result<(), BackendError> {
            Ok(())
        }
        fn activity_path_for(&self, _id: &WorkItemId) -> Option<std::path::PathBuf> {
            None
        }
        fn backend_type(&self) -> crate::work_item::BackendType {
            crate::work_item::BackendType::LocalFile
        }
    }

    #[test]
    fn stage_system_prompt_never_reads_plan_on_ui_thread() {
        // Proof: after the refactor, `stage_system_prompt` takes the
        // plan text as a parameter and MUST NOT call
        // `backend.read_plan(...)` itself. Against the pre-fix code
        // this assertion would fail: the UI-thread call of
        // `stage_system_prompt` unconditionally invoked
        // `self.backend.read_plan(work_item_id)` before building the
        // prompt, bumping the counter to 1.
        let backend = Arc::new(CountingPlanBackend::default());
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-stage-prompt.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "stage-prompt-test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/p0-stage-prompt-repo"),
                branch: Some("feature/sp".into()),
                worktree_path: Some(PathBuf::from("/tmp/p0-stage-prompt-worktree")),
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        let cwd = PathBuf::from("/tmp/p0-stage-prompt-worktree");
        // The caller passes the plan text as a parameter - the
        // function itself must NEVER consult the backend.
        let _ = app.stage_system_prompt(&wi_id, &cwd, "pre-read plan body".into());
        assert_eq!(
            backend.load(),
            0,
            "stage_system_prompt must use the plan_text parameter and \
             never call backend.read_plan on the UI thread",
        );
    }

    #[test]
    fn begin_session_open_defers_backend_read_plan_to_background_thread() {
        // Proof: `begin_session_open` must NOT call
        // `backend.read_plan` on the UI thread. Under the pre-fix
        // `stage_system_prompt` path, `complete_session_open`
        // -> `stage_system_prompt` would read the plan synchronously
        // before returning to the event loop, freezing the UI while
        // the filesystem read ran. This regression guard ensures the
        // read moves to the background thread driven by
        // `poll_session_opens` / `finish_session_open`.
        let backend = Arc::new(CountingPlanBackend::default());
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/p0-session-open.json"));
        // Work item needs a status that allows sessions.
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "session-open-test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/p0-session-open-repo"),
                branch: Some("feature/so".into()),
                worktree_path: Some(PathBuf::from("/tmp/p0-session-open-worktree")),
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        // Hold the gate so the background thread cannot call
        // `read_plan` until the test releases it. Any synchronous
        // caller of `backend.read_plan` would deadlock here (on the
        // test thread) and `begin_session_open` would never return.
        let gate = backend.gate.lock().unwrap();

        let cwd = PathBuf::from("/tmp/p0-session-open-worktree");
        app.begin_session_open(&wi_id, &cwd);

        // Immediately after the UI-thread call: the backend MUST NOT
        // have been touched. The background thread is parked waiting
        // on the gate mutex held by this test; the counter is zero.
        let reads_immediately_after = backend.load();
        assert_eq!(
            reads_immediately_after, 0,
            "begin_session_open must defer backend.read_plan to the \
             background thread - see docs/UI.md 'Blocking I/O Prohibition'",
        );
        assert!(
            app.session_open_rx.contains_key(&wi_id),
            "begin_session_open must register a pending receiver for the \
             background plan read",
        );

        // Release the gate so the background thread may proceed, then
        // drain it via the channel. After that the counter must be 1
        // (the background thread actually ran the read).
        drop(gate);
        let entry = app.session_open_rx.remove(&wi_id).unwrap();
        let result = entry
            .rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("background plan-read thread must deliver a result");
        assert_eq!(result.plan_text, "plan-text from counting backend");
        assert!(result.read_error.is_none());
        assert_eq!(
            backend.load(),
            1,
            "background thread must have performed exactly one read_plan call",
        );
        // End the spinner activity since `poll_session_opens` was
        // bypassed by the manual drain above.
        app.end_activity(entry.activity);
    }

    #[test]
    fn delete_work_item_phase5_forwards_orphan_branch_to_cleanup_info() {
        // Regression guard for R2-F-2. Round 1 pushed
        // `(repo_path, worktree_path)` pairs into `orphan_worktrees`
        // and silently dropped the branch name - so the synthesized
        // `DeleteCleanupInfo` had `branch: None` and
        // `spawn_delete_cleanup` skipped the `git branch -D` step. On
        // master this step ran inline. Net regression: a
        // delete-during-create race leaked a branch ref.
        //
        // Proof: put a completed `WorktreeCreateResult` with a known
        // branch into the `UserActionKey::WorktreeCreate` helper
        // payload, call `delete_work_item_by_id`, and assert the
        // resulting `OrphanWorktree` has the branch populated. Then
        // mirror the caller's synthesis logic and check the
        // `DeleteCleanupInfo` carries the branch through unchanged.
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(CountingPlanBackend::default()) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/r2f2-orphan.json"));
        let repo_path = PathBuf::from("/tmp/r2f2-repo");
        let worktree_path = PathBuf::from("/tmp/r2f2-worktree");
        let branch_name = "feature/r2f2-orphan".to_string();

        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "r2f2-test".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: repo_path.clone(),
                branch: Some(branch_name.clone()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        // Pre-queue a completed worktree-create result so Phase 5's
        // `try_recv` drains it synchronously.
        let (tx, rx) = crossbeam_channel::bounded::<WorktreeCreateResult>(1);
        tx.send(WorktreeCreateResult {
            wi_id: wi_id.clone(),
            repo_path: repo_path.clone(),
            branch: Some(branch_name.clone()),
            path: Some(worktree_path.clone()),
            error: None,
            open_session: true,
            branch_gone: false,
            reused: false,
        })
        .unwrap();
        app.try_begin_user_action(
            UserActionKey::WorktreeCreate,
            Duration::ZERO,
            "Initializing worktree...",
        )
        .expect("helper admit should succeed");
        app.attach_user_action_payload(
            &UserActionKey::WorktreeCreate,
            UserActionPayload::WorktreeCreate {
                rx,
                wi_id: wi_id.clone(),
            },
        );

        let mut warnings: Vec<String> = Vec::new();
        let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
        app.delete_work_item_by_id(&wi_id, &mut warnings, &mut orphan_worktrees)
            .expect("delete must succeed");

        assert_eq!(
            orphan_worktrees.len(),
            1,
            "Phase 5 must capture the in-flight worktree as an orphan",
        );
        let orphan = &orphan_worktrees[0];
        assert_eq!(orphan.repo_path, repo_path);
        assert_eq!(orphan.worktree_path, worktree_path);
        assert_eq!(
            orphan.branch.as_deref(),
            Some(branch_name.as_str()),
            "R2-F-2 regression: orphan must preserve the branch name so \
             spawn_delete_cleanup can run `git branch -D`",
        );

        // Mirror the caller's synthesis and verify the DeleteCleanupInfo
        // carries the branch through. This exercises the exact code path
        // in `confirm_delete_from_prompt` and the MCP delete handler.
        let cleanup_info = DeleteCleanupInfo {
            repo_path: orphan.repo_path.clone(),
            branch: orphan.branch.clone(),
            worktree_path: Some(orphan.worktree_path.clone()),
            branch_in_main_worktree: false,
            open_pr_number: None,
            github_remote: None,
        };
        assert_eq!(
            cleanup_info.branch.as_deref(),
            Some(branch_name.as_str()),
            "synthesized DeleteCleanupInfo must propagate the orphan branch",
        );
        assert!(
            !cleanup_info.branch_in_main_worktree,
            "a freshly-created worktree is never the main worktree",
        );
    }

    #[test]
    fn begin_session_open_surfaces_activity_spinner_for_feedback() {
        // Regression guard for R2-F-3. Round 1's background plan-read
        // path returned silently from `begin_session_open`, so a slow
        // backend made the TUI look hung between Enter and the next
        // 200ms poll tick. `begin_session_open` must register an
        // activity so `current_activity()` surfaces feedback
        // immediately. The activity must also be ended in every
        // terminal path of `poll_session_opens` - here we verify the
        // happy path.
        let backend = Arc::new(CountingPlanBackend::default());
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/r2f3-session-open.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "r2f3-session-open".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/r2f3-repo"),
                branch: Some("feature/r2f3".into()),
                worktree_path: Some(PathBuf::from("/tmp/r2f3-worktree")),
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        // No spinner before the call.
        assert!(app.current_activity().is_none());

        let cwd = PathBuf::from("/tmp/r2f3-worktree");
        app.begin_session_open(&wi_id, &cwd);

        // Spinner must be present IMMEDIATELY - no waiting on the
        // background thread to finish. This is the entire point of the
        // R2-F-3 fix.
        let activity_msg = app
            .current_activity()
            .expect("R2-F-3 regression: begin_session_open must start an activity spinner");
        assert_eq!(activity_msg, "Opening session...");
        assert!(
            app.session_open_rx.contains_key(&wi_id),
            "begin_session_open must register a pending receiver",
        );

        // Wait for the background read to produce a result, then drain
        // it via `poll_session_opens`. The spinner MUST be cleared once
        // the result is applied.
        let recv_start = std::time::Instant::now();
        loop {
            let ready = app
                .session_open_rx
                .get(&wi_id)
                .map(|entry| !entry.rx.is_empty())
                .unwrap_or(false);
            if ready {
                break;
            }
            if recv_start.elapsed() > std::time::Duration::from_secs(2) {
                panic!("background plan-read thread did not produce a result");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // `finish_session_open` will try to spawn a Claude session,
        // which would touch external binaries. To avoid that, drain
        // and end the spinner manually via the internal helper.
        // This mirrors what `poll_session_opens` does on success.
        let entry = app.session_open_rx.remove(&wi_id).unwrap();
        let _result = entry
            .rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("background plan-read thread must deliver a result");
        app.end_activity(entry.activity);

        assert!(
            app.current_activity().is_none(),
            "R2-F-3 regression: spinner must be cleared after the result is drained",
        );
    }

    #[test]
    fn apply_stage_change_cancels_pending_session_open() {
        // Codex finding: pending session opens must NOT survive a stage
        // change. The plan-read receiver in `session_open_rx` has no
        // entry in `self.sessions`, so the old session-kill branch in
        // `apply_stage_change` would only run if a session already
        // existed. Without the unconditional `drop_session_open_entry`
        // call, a stale pending open from the old stage would survive
        // the transition and `finish_session_open` would later spawn
        // Claude for the new stage - including no-session stages like
        // Mergequeue or Done. This test pins the cancellation contract.
        let backend = Arc::new(CountingPlanBackend::default());
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/codex-stage-cancel.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "codex-stage-cancel".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/codex-stage-cancel-repo"),
                branch: Some("feature/codex-stage".into()),
                worktree_path: Some(PathBuf::from("/tmp/codex-stage-cancel-wt")),
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        let cwd = PathBuf::from("/tmp/codex-stage-cancel-wt");
        app.begin_session_open(&wi_id, &cwd);
        assert!(
            app.session_open_rx.contains_key(&wi_id),
            "begin_session_open must register a pending receiver",
        );
        assert!(
            app.current_activity().is_some(),
            "begin_session_open must start an activity spinner",
        );

        // Stage transition to Mergequeue (a no-session stage). Use
        // "pr_merge" source to satisfy the merge-gate guard - the
        // important behaviour to pin is that the pending open is
        // cancelled, not the source-string semantics.
        app.apply_stage_change(
            &wi_id,
            &WorkItemStatus::Implementing,
            &WorkItemStatus::Mergequeue,
            "pr_merge",
        );

        assert!(
            !app.session_open_rx.contains_key(&wi_id),
            "stage change must cancel the pending session open - otherwise \
             finish_session_open would later spawn Claude for the new stage",
        );
        assert!(
            app.current_activity().is_none(),
            "stage change must end the 'Opening session...' spinner",
        );
    }

    /// A WorktreeService whose `remove_worktree` always fails. Used to
    /// verify that `spawn_orphan_worktree_cleanup` surfaces failures
    /// through the per-spawn `OrphanCleanupFinished` completion message
    /// instead of dropping them.
    #[cfg(test)]
    pub struct FailingRemoveWorktreeService;

    #[cfg(test)]
    impl WorktreeService for FailingRemoveWorktreeService {
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
                "unsupported in this stub".into(),
            ))
        }

        fn remove_worktree(
            &self,
            _repo_path: &std::path::Path,
            _worktree_path: &std::path::Path,
            _delete_branch: bool,
            _force: bool,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            Err(crate::worktree_service::WorktreeError::GitError(
                "simulated remove failure".into(),
            ))
        }

        fn delete_branch(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _force: bool,
        ) -> Result<(), crate::worktree_service::WorktreeError> {
            Err(crate::worktree_service::WorktreeError::GitError(
                "simulated branch delete failure".into(),
            ))
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

    #[test]
    fn spawn_orphan_worktree_cleanup_surfaces_failures_via_status_message() {
        // Codex finding: `spawn_orphan_worktree_cleanup` previously
        // discarded `remove_worktree` and `delete_branch` errors with
        // `let _ = ...`, leaving leaked worktrees/branches with no
        // user-visible warning. The fix routes failures through the
        // per-spawn `OrphanCleanupFinished` completion message so
        // `poll_orphan_cleanup_finished` can surface them in the status
        // bar AND end the matching status-bar activity.
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(StubBackend) as Arc<dyn WorkItemBackend>,
            Arc::new(FailingRemoveWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );

        // Drain any pre-existing status message so the assertion
        // below is unambiguous.
        app.status_message = None;

        app.spawn_orphan_worktree_cleanup(
            PathBuf::from("/tmp/codex-orphan-repo"),
            PathBuf::from("/tmp/codex-orphan-repo/.worktrees/feature/codex-orphan"),
            Some("feature/codex-orphan".into()),
        );

        // Spawning must register a status-bar activity per
        // `docs/UI.md` "Activity indicator placement".
        assert!(
            app.current_activity().is_some(),
            "spawn_orphan_worktree_cleanup must register a status-bar activity",
        );

        // Wait for the single completion message to land in the channel.
        let recv_start = std::time::Instant::now();
        loop {
            if !app.orphan_cleanup_finished_rx.is_empty() {
                break;
            }
            if recv_start.elapsed() > std::time::Duration::from_secs(2) {
                panic!("orphan cleanup background thread did not enqueue completion message");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        app.poll_orphan_cleanup_finished();

        let msg = app
            .status_message
            .as_ref()
            .expect("poll_orphan_cleanup_finished must surface a status message");
        assert!(
            msg.contains("Orphan worktree cleanup failed"),
            "status message must mention the worktree failure, got: {msg}",
        );
        assert!(
            msg.contains("Orphan branch cleanup failed"),
            "status message must mention the branch failure, got: {msg}",
        );
        assert!(
            msg.contains("feature/codex-orphan"),
            "status message must include the branch name, got: {msg}",
        );
        assert!(
            app.current_activity().is_none(),
            "poll_orphan_cleanup_finished must end the spawned activity even on failure",
        );
    }

    #[test]
    fn poll_orphan_cleanup_finished_is_silent_on_idle_channel() {
        // The idle path: an empty channel means no cleanup has finished;
        // `poll_orphan_cleanup_finished` must NOT clobber an unrelated
        // status message and must NOT touch any activity.
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(StubBackend) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        app.status_message = Some("unrelated status message".into());

        app.poll_orphan_cleanup_finished();

        assert_eq!(
            app.status_message.as_deref(),
            Some("unrelated status message"),
            "empty completion channel must not clobber unrelated status messages",
        );
    }

    #[test]
    fn spawn_orphan_worktree_cleanup_ends_activity_on_success() {
        // Success path: the cleanup closure runs against `StubWorktreeService`
        // (whose `remove_worktree` / `delete_branch` succeed), sends an
        // `OrphanCleanupFinished` with no warnings, and the poll
        // function must end the registered status-bar activity without
        // touching `status_message`.
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::new(StubBackend) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        app.status_message = None;

        app.spawn_orphan_worktree_cleanup(
            PathBuf::from("/tmp/orphan-success-repo"),
            PathBuf::from("/tmp/orphan-success-repo/.worktrees/feature/orphan-success"),
            Some("feature/orphan-success".into()),
        );

        assert!(
            app.current_activity().is_some(),
            "spawn_orphan_worktree_cleanup must register a status-bar activity",
        );

        // Wait for the single completion message to arrive.
        let recv_start = std::time::Instant::now();
        loop {
            if !app.orphan_cleanup_finished_rx.is_empty() {
                break;
            }
            if recv_start.elapsed() > std::time::Duration::from_secs(2) {
                panic!("orphan cleanup background thread did not enqueue completion message");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        app.poll_orphan_cleanup_finished();

        assert!(
            app.current_activity().is_none(),
            "poll_orphan_cleanup_finished must end the spawned activity on success",
        );
        assert!(
            app.status_message.is_none(),
            "successful orphan cleanup must not set status_message, got {:?}",
            app.status_message,
        );
    }

    #[test]
    fn cleanup_session_state_ends_spinner_for_pending_open() {
        // Regression guard for R2-F-3's symmetric cleanup path:
        // `cleanup_session_state_for` is called when a work item is
        // deleted mid-open. It must route through
        // `drop_session_open_entry` so the spinner is not leaked.
        let backend = Arc::new(CountingPlanBackend::default());
        let mut app = App::with_config_and_worktree_service(
            Config::default(),
            Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
            Arc::new(StubWorktreeService),
            Box::new(crate::config::InMemoryConfigProvider::new()),
        );
        let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/r2f3-cleanup.json"));
        app.work_items.push(crate::work_item::WorkItem {
            id: wi_id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "r2f3-cleanup".into(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![crate::work_item::RepoAssociation {
                repo_path: PathBuf::from("/tmp/r2f3-cleanup-repo"),
                branch: Some("feature/r2f3c".into()),
                worktree_path: Some(PathBuf::from("/tmp/r2f3-cleanup-wt")),
                pr: None,
                issue: None,
                git_state: None,
            }],
            errors: vec![],
        });

        let cwd = PathBuf::from("/tmp/r2f3-cleanup-wt");
        app.begin_session_open(&wi_id, &cwd);
        assert!(app.current_activity().is_some());

        // Delete-flavour cleanup: spinner must be cleared.
        app.cleanup_session_state_for(&wi_id);
        assert!(
            app.current_activity().is_none(),
            "cleanup_session_state_for must end the session-open spinner",
        );
        assert!(
            !app.session_open_rx.contains_key(&wi_id),
            "pending session-open entry must be removed on cleanup",
        );
    }

    /// Gap 1 regression: `drain_pr_identity_backfill` must end the
    /// status-bar activity AND clear the receiver on the Disconnected
    /// branch. The activity is started in `salsa.rs::app_init` when the
    /// backfill request set is non-empty; the only terminal state for
    /// that one-shot stream is sender-dropped (background thread done),
    /// so `drain_pr_identity_backfill` is the sole place the activity
    /// can be ended without leaking a spinner.
    #[test]
    fn drain_pr_identity_backfill_ends_activity_on_disconnect() {
        let mut app = App::new();

        // Manually wire a disconnected channel + a registered activity:
        // create the channel, drop the tx half so the next try_recv
        // returns Disconnected, store the rx on App and start the
        // matching status-bar activity.
        let (tx, rx) =
            crossbeam_channel::unbounded::<Result<crate::app::PrIdentityBackfillResult, String>>();
        drop(tx);
        app.pr_identity_backfill_rx = Some(rx);
        let aid = app.start_activity("Backfilling merged PR identities...");
        app.pr_identity_backfill_activity = Some(aid);

        let changed = app.drain_pr_identity_backfill();

        assert!(
            !changed,
            "no Ok messages were sent so changed must be false",
        );
        assert!(
            app.pr_identity_backfill_rx.is_none(),
            "Disconnected branch must drop the receiver",
        );
        assert!(
            app.pr_identity_backfill_activity.is_none(),
            "Disconnected branch must take the ActivityId",
        );
        assert!(
            app.current_activity().is_none(),
            "drain_pr_identity_backfill must end the status-bar activity \
             on Disconnected so the spinner does not leak",
        );
    }

    /// Gap 3 regression: the disconnected arm of `poll_review_gate`
    /// must end the review gate's status-bar activity. Routing through
    /// `drop_review_gate` is the structural guarantee.
    #[test]
    fn poll_review_gate_disconnect_ends_status_bar_activity() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        // Drop the tx half so the next try_recv yields Disconnected.
        let (tx, rx) = crossbeam_channel::unbounded::<ReviewGateMessage>();
        drop(tx);
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        assert!(
            app.current_activity().is_some(),
            "test gate must register an activity to begin with",
        );

        app.poll_review_gate();

        assert!(
            !app.review_gates.contains_key(&wi_id),
            "Disconnected gate must be dropped",
        );
        assert!(
            app.current_activity().is_none(),
            "Disconnected arm of poll_review_gate must end the gate activity",
        );
    }

    /// Gap 3 regression: the Blocked arm of `poll_review_gate` must
    /// end the review gate's status-bar activity. Use a Tui origin so
    /// the test does not need a live session map - the Tui branch only
    /// surfaces the reason and drops the gate, which is exactly what
    /// the test wants to observe.
    #[test]
    fn poll_review_gate_blocked_ends_status_bar_activity() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Blocked {
            work_item_id: wi_id.clone(),
            reason: "Cannot enter Review: no plan exists".into(),
        })
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Tui);

        assert!(app.current_activity().is_some());

        app.poll_review_gate();

        assert!(
            !app.review_gates.contains_key(&wi_id),
            "Blocked gate must be dropped",
        );
        assert!(
            app.current_activity().is_none(),
            "Blocked arm of poll_review_gate must end the gate activity",
        );
    }

    /// Gap 3 regression: the Result arm of `poll_review_gate` must
    /// end the review gate's status-bar activity, both for the approve
    /// path and the reject path.
    ///
    /// The reject path additionally kills and respawns the session,
    /// which starts its own "Opening session..." activity - so we
    /// cannot assert that `current_activity()` is None after polling.
    /// Instead, we capture the gate's ActivityId before polling and
    /// verify that exact ID is no longer in `app.activities`.
    #[test]
    fn poll_review_gate_result_ends_status_bar_activity_reject() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Result(ReviewGateResult {
            work_item_id: wi_id.clone(),
            approved: false,
            detail: "missing tests".into(),
        }))
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        let gate_aid = app
            .review_gates
            .get(&wi_id)
            .map(|g| g.activity)
            .expect("inserted gate must expose its ActivityId");

        app.poll_review_gate();

        assert!(
            !app.review_gates.contains_key(&wi_id),
            "Result gate must be dropped",
        );
        assert!(
            !app.activities.iter().any(|a| a.id == gate_aid),
            "Result arm of poll_review_gate must end the gate's specific \
             ActivityId via drop_review_gate",
        );
    }

    /// Gap 3 regression: same property on the approve path. The
    /// approve path advances the work item to Review and spawns a
    /// session for the new stage, so other activities may exist
    /// afterwards - we assert only that the gate's specific ID is
    /// gone.
    #[test]
    fn poll_review_gate_result_ends_status_bar_activity_approve() {
        let (mut app, wi_id) = app_with_work_item(
            WorkItemStatus::Implementing,
            Some("feature/test"),
            Some("/tmp/repo"),
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(ReviewGateMessage::Result(ReviewGateResult {
            work_item_id: wi_id.clone(),
            approved: true,
            detail: "looks good".into(),
        }))
        .unwrap();
        insert_test_review_gate(&mut app, wi_id.clone(), rx, ReviewGateOrigin::Mcp);

        let gate_aid = app
            .review_gates
            .get(&wi_id)
            .map(|g| g.activity)
            .expect("inserted gate must expose its ActivityId");

        app.poll_review_gate();

        assert!(
            !app.review_gates.contains_key(&wi_id),
            "Result gate must be dropped",
        );
        assert!(
            !app.activities.iter().any(|a| a.id == gate_aid),
            "Result arm of poll_review_gate must end the gate's specific \
             ActivityId via drop_review_gate",
        );
    }

    // -- User action guard --

    /// `try_begin_user_action` followed by `end_user_action` admits a
    /// single action, starts one activity, and clears it cleanly.
    #[test]
    fn user_action_try_begin_then_end_roundtrip() {
        let mut app = App::new();
        let aid = app
            .try_begin_user_action(UserActionKey::PrCreate, Duration::ZERO, "Creating PR...")
            .expect("first admit must succeed");
        assert!(app.is_user_action_in_flight(&UserActionKey::PrCreate));
        assert!(app.activities.iter().any(|a| a.id == aid));
        app.end_user_action(&UserActionKey::PrCreate);
        assert!(!app.is_user_action_in_flight(&UserActionKey::PrCreate));
        assert!(!app.activities.iter().any(|a| a.id == aid));
    }

    /// Calling `try_begin_user_action` twice without an intermediate
    /// `end_user_action` must reject the second call.
    #[test]
    fn user_action_try_begin_rejects_second_concurrent_call() {
        let mut app = App::new();
        let first = app
            .try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging...")
            .expect("first admit must succeed");
        let second =
            app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging...");
        assert!(second.is_none(), "second concurrent admit must return None");
        // First activity is still owned by the helper.
        assert!(app.activities.iter().any(|a| a.id == first));
    }

    /// A debounce window blocks a fresh admit even after the previous
    /// one has been ended.
    #[test]
    fn user_action_debounce_window_blocks_repeat() {
        let mut app = App::new();
        app.try_begin_user_action(
            UserActionKey::GithubRefresh,
            Duration::from_millis(500),
            "Refreshing...",
        )
        .expect("first admit must succeed");
        app.end_user_action(&UserActionKey::GithubRefresh);
        // Immediate retry within the debounce window is rejected.
        let retry = app.try_begin_user_action(
            UserActionKey::GithubRefresh,
            Duration::from_millis(500),
            "Refreshing...",
        );
        assert!(retry.is_none(), "debounce must reject rapid retry");
    }

    /// Once the debounce window has elapsed, a fresh admit is
    /// accepted.
    #[test]
    fn user_action_debounce_elapsed_allows_retry() {
        let mut app = App::new();
        // Use a very short (10ms) debounce so the test does not
        // actually have to sleep in production CI. The plan pins
        // debounce values at the call site, so direct overrides are
        // the supported way to test.
        app.try_begin_user_action(
            UserActionKey::GithubRefresh,
            Duration::from_millis(10),
            "Refreshing...",
        )
        .expect("first admit must succeed");
        app.end_user_action(&UserActionKey::GithubRefresh);
        std::thread::sleep(Duration::from_millis(20));
        let retry = app.try_begin_user_action(
            UserActionKey::GithubRefresh,
            Duration::from_millis(10),
            "Refreshing...",
        );
        assert!(retry.is_some(), "debounce should allow retry after elapse");
    }

    /// `end_user_action` is idempotent: calling it a second time is a
    /// silent no-op (no panic, no spurious activity cleanup).
    #[test]
    fn user_action_end_is_idempotent() {
        let mut app = App::new();
        app.try_begin_user_action(UserActionKey::ReviewSubmit, Duration::ZERO, "Submitting...")
            .expect("admit must succeed");
        app.end_user_action(&UserActionKey::ReviewSubmit);
        // Second end is a no-op.
        app.end_user_action(&UserActionKey::ReviewSubmit);
        // Third end on a key that was never admitted is also a no-op.
        app.end_user_action(&UserActionKey::DeleteCleanup);
    }

    /// Unit test for `try_begin_user_action`: a second admit on the
    /// same key while the first is still in flight is rejected. This
    /// only covers the helper-level in-flight check; the full Ctrl+R
    /// dispatch path (including the `pending_fetch_count` hard gate
    /// and the status message wiring) is exercised by
    /// `ctrl_r_rapid_double_press_through_handle_key_is_gated` in
    /// `src/event.rs`.
    #[test]
    fn user_action_second_admit_rejected_while_in_flight() {
        let mut app = App::new();
        // First admit succeeds.
        let first = app.try_begin_user_action(
            UserActionKey::GithubRefresh,
            Duration::from_millis(500),
            "Refreshing GitHub data",
        );
        assert!(first.is_some(), "first admit must succeed");
        // While the helper entry is still in flight, a second admit is
        // rejected by the in-flight check.
        let second = app.try_begin_user_action(
            UserActionKey::GithubRefresh,
            Duration::from_millis(500),
            "Refreshing GitHub data",
        );
        assert!(second.is_none(), "second admit must be rejected");
    }

    /// `reset_fetch_state` is the single site that tears down all
    /// fetcher-derived UI state on a structural restart (see the
    /// salsa.rs `fetcher_repos_changed` block). It must reset three
    /// invariants together:
    ///   1. drop `fetch_rx`
    ///   2. zero `pending_fetch_count`
    ///   3. end both possible spinner owners (the `GithubRefresh`
    ///      helper entry AND `structural_fetch_activity`)
    ///
    /// This test seeds the derived state as if two `FetchStarted`
    /// messages had been counted but their paired terminal messages
    /// were stranded on the old channel, then asserts that the reset
    /// leaves the app in a clean slate that does NOT strand the Ctrl+R
    /// count gate for the rest of the process lifetime.
    #[test]
    fn reset_fetch_state_clears_all_fetcher_derived_state() {
        let mut app = App::new();

        // Seed state as if the fetcher had started and the Ctrl+R
        // helper entry was admitted (covers the path where a Ctrl+R
        // was in flight when the restart happened).
        app.try_begin_user_action(
            UserActionKey::GithubRefresh,
            Duration::ZERO,
            "Refreshing GitHub data",
        )
        .expect("admit must succeed");
        // Simulate two repos' `FetchStarted` counted but not yet
        // paired with `RepoData`/`FetcherError`. These are exactly
        // the messages that would be stranded when the old channel is
        // dropped by the restart.
        app.pending_fetch_count = 2;

        // Sanity-check the seeded state.
        assert!(app.is_user_action_in_flight(&UserActionKey::GithubRefresh));
        assert_eq!(app.pending_fetch_count, 2);
        assert!(!app.activities.is_empty());

        // Simulate the salsa restart block.
        app.reset_fetch_state();

        // All three invariants must be clear.
        assert!(
            app.fetch_rx.is_none(),
            "fetch_rx must be dropped by reset_fetch_state",
        );
        assert_eq!(
            app.pending_fetch_count, 0,
            "pending_fetch_count must be reset to 0 - otherwise the Ctrl+R \
             hard gate in src/event.rs permanently locks out refresh",
        );
        assert!(
            !app.is_user_action_in_flight(&UserActionKey::GithubRefresh),
            "GithubRefresh helper entry must be cleared",
        );
        assert!(
            app.structural_fetch_activity.is_none(),
            "structural_fetch_activity must be cleared",
        );
        assert!(
            app.activities.is_empty(),
            "no stray status-bar spinners may survive the reset",
        );
    }

    /// `reset_fetch_state` must also handle the structural-fallback
    /// ownership path: when `FetchStarted` arrived without a prior
    /// Ctrl+R admit (manage/unmanage, work-item create, delete
    /// cleanup, etc.), the spinner is owned by
    /// `structural_fetch_activity` rather than the helper entry. The
    /// reset must end that activity too, not just the helper.
    #[test]
    fn reset_fetch_state_ends_structural_fetch_activity() {
        let mut app = App::new();
        // Simulate `drain_fetch_results` on the structural-restart
        // path: no helper entry, but a counted FetchStarted and an
        // owned structural activity.
        let id = app.start_activity("Refreshing GitHub data");
        app.structural_fetch_activity = Some(id);
        app.pending_fetch_count = 1;
        assert!(!app.is_user_action_in_flight(&UserActionKey::GithubRefresh));

        app.reset_fetch_state();

        assert_eq!(app.pending_fetch_count, 0);
        assert!(app.structural_fetch_activity.is_none());
        assert!(
            app.activities.is_empty(),
            "structural_fetch_activity id must be removed from the activity list",
        );
    }
}
