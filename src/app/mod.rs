use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::agent_backend::{
    self, AgentBackend, AgentBackendKind, ClaudeCodeBackend, ReviewGateSpawnConfig, SpawnConfig,
    WORK_ITEM_ALLOWED_TOOLS,
};
use crate::assembly;
use crate::click_targets::{ClickKind, ClickRegistry};
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

pub mod user_actions;
pub use user_actions::{UserActionGuard, UserActionKey, UserActionPayload, UserActionState};

/// A transient top-right notification shown after a click-to-copy
/// action. Auto-dismisses when `expires_at` is reached. Rendered by
/// `ui::draw_toasts` on top of everything else, including the global
/// drawer and settings overlay.
#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub expires_at: Instant,
}

/// State for the first-run Ctrl+G modal that asks the user to choose a
/// harness for the global assistant. Built lazily when Ctrl+G is pressed
/// with `config.defaults.global_assistant_harness == None`; dismissed
/// (and cleared from `App`) after a pick or Esc.
///
/// The list of available harnesses is frozen at modal-open time so the
/// keybindings shown to the user cannot reshuffle mid-selection.
#[derive(Clone, Debug)]
pub struct FirstRunGlobalHarnessModal {
    /// Harnesses currently on `PATH`, in canonical order
    /// (`ClaudeCode`, `Codex`). Only user-selectable kinds are
    /// considered: `AgentBackendKind::OpenCode` is not surfaced here
    /// because its adapter is a future-work stub. Empty is never
    /// stored: the opener shows a toast and returns without populating
    /// the modal when the list would be empty.
    pub available_harnesses: Vec<AgentBackendKind>,
}

/// Truncate a copy-target value for display in a toast so very long
/// URLs / file paths do not blow out the frame width. Returns a short
/// human-readable form; the untruncated value is what actually lands
/// on the clipboard.
///
/// Kind-specific policy:
/// - `PrUrl`: keep the trailing `<owner>/<repo>/pull/<n>` tail, or just
///   the last 40 chars if the URL has no recognizable tail.
/// - `RepoPath`: show the basename only.
/// - `Branch` / `Title`: truncate to 40 chars with an ellipsis marker.
pub fn short_display(value: &str, kind: ClickKind) -> String {
    const MAX: usize = 40;
    match kind {
        ClickKind::PrUrl => {
            // Find `/pull/` and keep everything from one segment
            // before it: e.g. `owner/repo/pull/123`.
            value.find("/pull/").map_or_else(
                || truncate_tail(value, MAX),
                |idx| {
                    let before = &value[..idx];
                    let owner_repo = before
                        .rsplit('/')
                        .take(2)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("/");
                    let tail = &value[idx..];
                    format!("{owner_repo}{tail}")
                },
            )
        }
        ClickKind::RepoPath => std::path::Path::new(value)
            .file_name()
            .and_then(|n| n.to_str())
            .map_or_else(
                || truncate_tail(value, MAX),
                std::string::ToString::to_string,
            ),
        ClickKind::Branch | ClickKind::Title => truncate_head(value, MAX),
    }
}

/// Head-truncate: keep the first `max` chars, append `...` if cut.
fn truncate_head(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{head}...")
    }
}

/// Tail-truncate: keep the last `max` chars, prepend `...` if cut.
fn truncate_tail(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        s.to_string()
    } else {
        let skip = total - max.saturating_sub(3);
        let tail: String = s.chars().skip(skip).collect();
        format!("...{tail}")
    }
}

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
    pub const fn days(self) -> i64 {
        match self {
            Self::Week => 7,
            Self::Month => 30,
            Self::Quarter => 90,
            Self::Year => 365,
        }
    }

    /// Short label shown in the header strip.
    pub const fn label(self) -> &'static str {
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
    /// Index into `BOARD_COLUMNS` (0=Backlog, 1=Planning, 2=Implementing, 3=Review).
    pub column: usize,
    /// Index of the selected item within the column, or None if column is empty.
    pub row: Option<usize>,
}

/// The four visible columns in the board view (Done is hidden).
/// Sentinel title used when a quick-start work item is created before the
/// user has specified what they want to work on. The `planning_quickstart`
/// system prompt instructs Claude to call `workbridge_set_title` once the
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
/// Derived from the currently selected `WorkItem`'s fields on each call
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// A review-requested PR (index into `App::review_requested_prs`).
    ReviewRequestItem(usize),
    /// An unlinked PR (index into `App::unlinked_prs`).
    UnlinkedItem(usize),
    /// A work item (index into `App::work_items`).
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
/// - `Mcp`: Claude requested Review via `workbridge_set_status` and the
///   background gate decided it cannot run. The rework flow applies -
///   kill the existing session and respawn with the rejection reason so
///   Claude has feedback to iterate on.
/// - `Tui`: The user pressed `l` (advance) on a no-diff or no-plan
///   Implementing item. The session is still the user's primary
///   workspace - killing and respawning would be destructive. Only
///   surface the reason in the status bar and let the user decide.
/// - `Auto`: An Implementing session died without calling
///   `workbridge_set_status("Review`"). Auto-triggering the gate is a
///   convenience; if it blocks we still want the rework flow so Claude
///   sees the reason on the next restart (mirrors Mcp semantics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewGateOrigin {
    Mcp,
    Tui,
    Auto,
}

/// Resolved selection target for the rebase-onto-main flow. Produced
/// by `App::selected_rebase_target` and consumed by
/// `App::start_rebase_on_main` -> `App::spawn_rebase_gate`. Carries
/// only the ids the spawn function needs, so the `m` key path stays
/// trivially testable without standing up a full work item.
///
/// `worktree_path` is intentionally the work item's worktree, not the
/// registered repo root. Each git worktree has its own HEAD, so any
/// `git -C` or `Command::current_dir` call that wants to operate on
/// `branch` MUST use the worktree path - otherwise it would shell out
/// against whatever the main checkout currently has checked out (which
/// is almost always `main` itself, and the rebase no-ops or, worse,
/// rewrites an unrelated branch).
pub struct RebaseTarget {
    pub wi_id: WorkItemId,
    pub worktree_path: PathBuf,
    pub branch: String,
}

/// Outcome of an attempted rebase-onto-main run, as reported by the
/// background thread that drove `git fetch` + the headless harness call.
///
/// Mirrors the shape of `ReviewGateResult` so the poll loop can pattern-
/// match on it without juggling tuples. `base_branch` is included on both
/// arms so the status-bar summary can name the branch we rebased onto
/// even on the failure path.
pub enum RebaseResult {
    /// The rebase finished cleanly. `conflicts_resolved` is `true` if
    /// the harness had to resolve conflicts during the run; the
    /// rebase-gate poll uses it only for the human-readable summary.
    /// `activity_log_error` is `Some` if the background thread tried
    /// to append a `rebase_completed` entry to the activity log and
    /// the append failed; the poll loop suffixes it onto the status
    /// message so the user can see that the audit trail did not
    /// land. The append runs in the background thread (NOT on the
    /// UI thread) per the absolute blocking-I/O invariant.
    Success {
        base_branch: String,
        conflicts_resolved: bool,
        activity_log_error: Option<String>,
    },
    /// The rebase failed - either `git fetch` did not return cleanly,
    /// the harness child exited non-zero, the JSON envelope was
    /// unparseable, or the harness gave up after attempting conflict
    /// resolution. `conflicts_attempted` is `true` if the harness made
    /// at least one resolution attempt before giving up; used only for
    /// the summary text. `activity_log_error` follows the same
    /// convention as on the Success arm: it is set if the background
    /// thread's `rebase_failed` activity-log append failed, and the
    /// poll loop appends it to the user-visible status message.
    Failure {
        base_branch: String,
        reason: String,
        conflicts_attempted: bool,
        activity_log_error: Option<String>,
    },
}

/// Messages sent from the rebase gate background thread to the main
/// thread. Streams zero or more `Progress` updates followed by exactly
/// one `Result`. Mirrors the streaming pattern documented in
/// `docs/UI.md` "Streaming progress variant" and used by
/// `ReviewGateMessage`.
pub enum RebaseGateMessage {
    Progress(String),
    Result(RebaseResult),
}

/// Per-work-item state for an in-flight rebase-onto-main run. Owned by
/// `App.rebase_gates: HashMap<WorkItemId, RebaseGateState>` so the
/// state's lifetime is tied structurally to the work item it belongs
/// to, per the structural-ownership rule in `CLAUDE.md`.
///
/// `activity` is the status-bar spinner started when the rebase
/// admission succeeded. The structural-ownership rule makes this the
/// single drop site for the spinner: every code path that removes an
/// entry from `rebase_gates` must go through `drop_rebase_gate`, which
/// ends the activity so the spinner can never leak.
///
/// The base branch is intentionally NOT cached here: it is unknown
/// until the background thread resolves it via
/// `WorktreeService::default_branch` (which shells out and would
/// violate the blocking-I/O invariant if called on the UI thread), and
/// the final `RebaseResult` carries it back through the channel for
/// the status-bar summary.
pub struct RebaseGateState {
    pub rx: crossbeam_channel::Receiver<RebaseGateMessage>,
    pub progress: Option<String>,
    pub activity: ActivityId,
    /// PID of the harness child while it is alive, written by the
    /// spawning sub-thread immediately after `Command::spawn` returns
    /// and cleared after `wait_with_output` returns. The main thread
    /// uses this in `drop_rebase_gate` to SIGKILL the harness when the
    /// owning work item is deleted, the user force-quits, or any other
    /// cleanup path tears the gate down. Without this handle the
    /// harness child would happily keep running `git rebase` /
    /// `git add` / `git rebase --continue` against a worktree that is
    /// being concurrently removed by `spawn_delete_cleanup`, which is
    /// the failure mode the cleanup-path drops protect against.
    ///
    /// Wrapped in `Arc<Mutex>` because the spawning sub-thread (which
    /// writes the PID) and the main thread (which reads + clears it
    /// during `drop_rebase_gate`) both need shared access. There is a
    /// microsecond race window between `wait_with_output` returning
    /// and the sub-thread clearing the PID; in practice the kernel
    /// will not reuse the PID inside that window, and SIGKILL on a
    /// recently-reaped PID is a no-op error rather than a wrong-process
    /// kill.
    pub child_pid: Arc<Mutex<Option<u32>>>,
    /// Cancellation flag set by `drop_rebase_gate` when the gate is
    /// torn down. Covers the window BEFORE the harness child is
    /// spawned: the background thread runs several blocking phases
    /// (default-branch resolution, `git fetch`, MCP server start,
    /// temp-config write, prompt build) before the harness PID is
    /// available, and during that window `child_pid` is still `None`
    /// so the SIGKILL path cannot stop the thread. The thread checks
    /// this flag at the start of every blocking phase and immediately
    /// after `Command::spawn` returns; on a `true` reading it kills
    /// the just-spawned child (if any), drops its MCP server / temp
    /// config, and exits without sending a result. Combined with
    /// `child_pid`, this closes the cancellation race for the entire
    /// gate lifecycle.
    pub cancelled: Arc<AtomicBool>,
}

/// Defense-in-depth `Drop` impl for `RebaseGateState`. Removing a
/// gate from `App.rebase_gates` (via `HashMap::remove`, via clearing
/// the map, or via `App` itself being dropped on a panic) is now
/// sufficient to signal cancellation and SIGKILL the harness process
/// group: the `cancelled` flag is set so the background thread bails
/// out of its next phase check, and `libc::killpg` takes down the
/// harness AND any `git rebase` / `git add` subprocesses it has
/// started. This is a structural insurance against forgetting to
/// call `App::drop_rebase_gate` from a new cleanup site - the helper
/// is still the preferred entrypoint because it ALSO ends the
/// status-bar activity and releases the user-action slot (both of
/// which need `App` access and so cannot live inside `Drop`), but
/// the worst case if a future caller forgets the helper is a leaked
/// spinner / debounce slot, NOT a runaway harness against a deleted
/// worktree.
///
/// The Drop runs synchronously when the state goes out of scope, so
/// `HashMap::remove` -> let-binding fall off scope -> Drop happens
/// in tens of microseconds. This is the same window as the explicit
/// `killpg` in `drop_rebase_gate`, just guaranteed for every removal
/// path including ones we have not written yet.
impl Drop for RebaseGateState {
    #[expect(
        unsafe_code,
        reason = "libc::killpg FFI on a process-group id we spawned; SAFETY comment below"
    )]
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let pid_to_kill = self.child_pid.lock().ok().and_then(|mut slot| slot.take());
        if let Some(pid) = pid_to_kill {
            // SAFETY: `libc::killpg` is an FFI call into a stable
            // POSIX syscall; arguments are a process-group id and a
            // signal number, both plain integers. The harness was
            // spawned with `Command::process_group(0)` (see
            // `spawn_rebase_gate`), so its PID equals its
            // process-group id. `ESRCH` after a freshly-reaped group
            // is harmless.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

/// Outcome of a subprocess run through `run_cancellable`.
/// Distinguishes "completed normally" from "gate was torn down
/// while the subprocess was running." Callers match on this to
/// decide whether to process the output or bail out of the gate.
pub enum SubprocessOutcome {
    /// The subprocess exited (successfully or not). Inspect
    /// `Output.status` to determine success/failure.
    Completed(std::process::Output),
    /// The `cancelled` flag was set while the subprocess was alive.
    /// The helper already `SIGKILLed` the process group and reaped
    /// the child; the caller should exit the gate cleanly.
    Cancelled,
}

/// Run a subprocess in a cancellable way. Encapsulates the full
/// "spawn in own process group, stash PID, check cancelled,
/// killpg if cancelled" dance so each call site in the rebase
/// gate's background thread does not have to reimplement the
/// ordering contract manually.
///
/// The ordering contract (the reason this helper exists):
///
///   **Stash the PID FIRST, then check `cancelled` SECOND.**
///
/// The `cancelled` flag is sticky (once set, never cleared). By
/// stashing the PID before reading the flag, we guarantee that
/// for every interleaving with `drop_rebase_gate` on the main
/// thread, either the drop path sees the PID and `killpg`s it,
/// or we see the flag and `killpg` the group ourselves. The
/// inverse ordering (check then stash) has a race window where
/// the drop path fires between check and stash, finds None in
/// the slot, and silently fails to kill the subprocess.
///
/// This bug was introduced twice (once for the harness child,
/// once for the fetch) before the pattern was extracted into this
/// helper. The helper makes the class of bug impossible to
/// reintroduce at new call sites because the ordering is baked
/// into one place rather than replicated.
#[expect(
    unsafe_code,
    reason = "libc::killpg FFI on a process-group id we spawned; SAFETY comment below"
)]
pub fn run_cancellable(
    cmd: &mut std::process::Command,
    pid_slot: &Arc<Mutex<Option<u32>>>,
    cancelled: &AtomicBool,
) -> Result<SubprocessOutcome, std::io::Error> {
    let mut child = cmd.process_group(0).spawn()?;
    let pid = child.id();

    // Stash FIRST.
    if let Ok(mut slot) = pid_slot.lock() {
        *slot = Some(pid);
    }

    // Check SECOND. Sticky flag: if true, it was true before we
    // stashed and will stay true forever, so the ordering is safe.
    if cancelled.load(Ordering::SeqCst) {
        if let Ok(mut slot) = pid_slot.lock() {
            *slot = None;
        }
        // SAFETY: `libc::killpg` is an FFI call into a stable
        // POSIX syscall. The child was spawned with
        // `process_group(0)` so its PID equals its process-group
        // id. `ESRCH` after a freshly-reaped group is harmless.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
        let _ = child.wait();
        return Ok(SubprocessOutcome::Cancelled);
    }

    let output = child.wait_with_output()?;
    if let Ok(mut slot) = pid_slot.lock() {
        *slot = None;
    }
    Ok(SubprocessOutcome::Completed(output))
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

/// Result of the live working-tree precheck that runs between the user
/// pressing "merge" and the actual `gh pr merge` thread spawning. The
/// shape mirrors `ReviewGateMessage::Result` / `Blocked`: either Ready
/// (the precheck cleared and the caller should hand off to the merge
/// thread with the recorded strategy) or Blocked (the live worktree
/// state is dirty / untracked / unpushed / unreachable and the user
/// must address it before retrying).
///
/// The Ready variant carries every piece of state the merge thread
/// needs (`branch`, `repo_path`, `owner_repo`) so
/// `perform_merge_after_precheck` does not have to re-resolve them
/// against `self.work_items` / `self.repo_data`. This is the
/// "structural ownership over manual correlation" pattern from
/// `CLAUDE.md`: the message itself is the source of truth for the
/// in-flight precheck's payload, and the helper map's
/// `UserActionKey::PrMerge` slot owns the receiver via the
/// `UserActionPayload::PrMergePrecheck` variant. There is no sibling
/// `Option<Receiver>` field on `App` - dropping the helper entry
/// (e.g. `end_user_action(&UserActionKey::PrMerge)` from any cancel
/// path) drops the receiver in the same step, so a future cancel
/// site cannot leak a stale channel.
pub enum MergePreCheckMessage {
    Ready {
        wi_id: WorkItemId,
        strategy: String,
        branch: String,
        repo_path: PathBuf,
        owner_repo: String,
    },
    /// The live precheck blocked the merge. The `wi_id` is implicit
    /// in the helper map's `UserActionKey::PrMerge` slot - which is
    /// the source of truth for the in-flight precheck - so it does
    /// not ride in this variant. `reason` is the user-facing alert
    /// text (it comes from `MergeReadiness::merge_block_message` for
    /// dirty / untracked / unpushed / PR-conflict / CI-failing, and
    /// a custom string for the "`list_worktrees` failed" and
    /// "`fetch_live_merge_state` failed" cases).
    Blocked { reason: String },
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

/// Classification of the combined local + remote state of a PR-bearing
/// work item, returned by `MergeReadiness::classify`.
///
/// The variants are ordered by merge-guard priority: the classifier
/// returns the first matching one, so the effective order is
/// `Dirty` > `Untracked` > `Unpushed` > `PrConflict` > `CiFailing` >
/// `BehindOnly` > `Clean`.
///
/// Local worktree states (`Dirty`, `Untracked`, `Unpushed`) rank above
/// remote-PR states (`PrConflict`, `CiFailing`) because they represent
/// the user's own in-flight work that would be lost or misrepresented
/// if the merge proceeded. Fixing a local blocker may also resolve the
/// PR blocker (e.g. pushing unpushed commits may update CI), so the
/// local wording is the most actionable first step.
///
/// `BehindOnly` is a soft-warning state: the worktree is behind its
/// upstream but has nothing of its own to lose, so pushing would be
/// the wrong fix and the branch is about to be deleted on merge
/// anyway. Every other non-Clean variant (except `BehindOnly`) blocks
/// the Review -> Done merge transition because advancing without
/// addressing it would either merge a PR that does not include the
/// user's local work, destroy that work when `gh pr merge
/// --delete-branch` deletes the local branch, or merge against a base
/// branch that will reject the merge at the server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeReadiness {
    /// No uncommitted changes, no untracked files, no unpushed
    /// commits, no PR conflicts, no failing CI. Either up-to-date
    /// with upstream or no upstream configured at all.
    Clean,
    /// Tracked files are modified / staged / renamed / deleted but
    /// not committed.
    Dirty,
    /// No tracked-file changes, but one or more untracked files are
    /// present in the worktree.
    Untracked,
    /// N commits exist locally that are not on the upstream. The
    /// user must push before merging or their work will not be in
    /// the merged PR.
    Unpushed(u32),
    /// GitHub reports the PR as `CONFLICTING` against its base branch.
    /// Merging would either fail or create a merge commit with
    /// unresolved conflict markers, depending on the strategy.
    PrConflict,
    /// The PR's CI rollup is `FAILURE`. Branch protection would
    /// typically reject the merge; even if it doesn't, merging
    /// known-broken code into the base branch is a footgun.
    CiFailing,
    /// N commits exist on the upstream that are not local. Shown
    /// as a soft warning because merging is still safe.
    BehindOnly(u32),
}

impl MergeReadiness {
    /// Classify the combined live worktree + remote PR state using
    /// the canonical priority order documented on the enum.
    ///
    /// Called exclusively from `App::spawn_merge_precheck` on a
    /// background thread after fresh fetches from both
    /// `WorktreeService::list_worktrees` and
    /// `GithubClient::fetch_live_merge_state`. The cached
    /// `RepoData.worktrees` / `PrInfo.mergeable` / `PrInfo.checks`
    /// are not consulted by this path - see `execute_merge` and the
    /// doc on `MergePreCheckMessage` for the rationale.
    ///
    /// `wt` is `Option<&WorktreeInfo>` so "no matching worktree"
    /// (PR-only items and items whose worktree was removed after the
    /// branch was pushed) is expressible at the type level without a
    /// sentinel value: the local checks short-circuit to "nothing
    /// to protect" and the PR checks still run.
    ///
    /// Field semantics mirror the docs on `WorktreeInfo`: `None` for
    /// any of `dirty`, `untracked`, `unpushed`, `behind_remote` means
    /// "the underlying check was not attempted or failed" and is
    /// treated as the safe default (`false` / `0`) so the worktree is
    /// not falsely flagged when a freshly-listed `WorktreeInfo` is
    /// missing a field.
    pub fn classify(
        wt: Option<&crate::worktree_service::WorktreeInfo>,
        live_pr: &crate::github_client::LivePrState,
    ) -> Self {
        // Local checks first: the user's own in-flight work takes
        // priority over server-side constraints because fixing the
        // local problem is a precondition for fixing the remote
        // problem (a CONFLICTING PR cannot be resolved without
        // committing locally, for instance).
        if let Some(wt) = wt {
            if wt.dirty.unwrap_or(false) {
                return Self::Dirty;
            }
            if wt.untracked.unwrap_or(false) {
                return Self::Untracked;
            }
            if let Some(ahead) = wt.unpushed
                && ahead > 0
            {
                return Self::Unpushed(ahead);
            }
        }

        // Remote checks: only meaningful when an open PR exists.
        if live_pr.has_open_pr {
            if matches!(
                live_pr.mergeable,
                crate::work_item::MergeableState::Conflicting
            ) {
                return Self::PrConflict;
            }
            if matches!(live_pr.check_rollup, crate::work_item::CheckStatus::Failing) {
                return Self::CiFailing;
            }
        }

        // Soft warning - not a blocker.
        if let Some(wt) = wt
            && let Some(behind) = wt.behind_remote
            && behind > 0
        {
            return Self::BehindOnly(behind);
        }

        Self::Clean
    }

    /// User-facing error text for a blocking state, or `None` when
    /// the state does not block. The wording is intentionally
    /// prescriptive ("Commit & push before merging.") so the alert
    /// doubles as the remediation step. Callers use the `Some` /
    /// `None` discriminant as the merge-block signal: there is no
    /// separate `is_merge_blocking` predicate because that would
    /// make it possible (via copy-paste drift) for the predicate
    /// and the message to disagree on which variants block.
    pub const fn merge_block_message(&self) -> Option<&'static str> {
        match self {
            Self::Dirty => Some("Uncommitted changes. Commit & push before merging."),
            Self::Untracked => Some("Untracked files. Commit/ignore & push before merging."),
            Self::Unpushed(_) => Some("Unpushed commits. Push before merging."),
            Self::PrConflict => Some("PR has conflicts. Resolve before merging."),
            Self::CiFailing => Some("CI failing. Fix checks before merging."),
            Self::Clean | Self::BehindOnly(_) => None,
        }
    }
}

/// Information needed to poll the PR state of a work item waiting on a
/// merge. Used by two parallel pollers:
///
/// - `poll_mergequeue` watches `Mergequeue`-stage items for the PR the
///   user explicitly opted into via `[p] Poll`.
/// - `poll_review_request_merges` watches `ReviewRequest`-kind items in
///   `Review` whose PR was merged externally - the only code path that
///   can observe such a merge, since the `--author @me` fetch filters
///   the PR out and `review-requested:@me` is `--state open`.
///
/// `pr_number` is the unambiguous identity of the PR. When it is `Some`,
/// the poll thread targets `gh pr view <number>`, which always returns
/// the exact PR even if the branch has since had another PR opened on
/// it. Mergequeue entry pins it from `assoc.pr.number` immediately, so
/// the live-entry path is never vulnerable to branch-resolution drift.
///
/// `pr_number` is `None` on a watch that was rebuilt from a backend
/// record after an app restart (since the in-memory `assoc.pr` may have
/// been gone by then) and on `ReviewRequest` watches where the
/// author-filtered fetch never populated `assoc.pr` in the first place.
/// In those cases the poll thread falls back to `gh pr view <branch>`;
/// the result drain writes the resolved number back into the watch so
/// subsequent polls are unambiguous.
///
/// `last_polled` enforces a per-item cooldown so each watch is checked on
/// its own 30s schedule. Polls run concurrently across watches.
pub struct PrMergeWatch {
    pub wi_id: WorkItemId,
    pub pr_number: Option<u64>,
    pub owner_repo: String,
    pub branch: String,
    pub repo_path: PathBuf,
    pub last_polled: Option<std::time::Instant>,
}

/// In-flight poll for a single PR merge watch. The map key is the
/// `WorkItemId`, so retreat / delete can drop exactly the entry that
/// belongs to the affected item without touching anything else
/// (structural ownership, per CLAUDE.md).
pub struct PrMergePollState {
    pub rx: crossbeam_channel::Receiver<PrMergePollResult>,
    pub activity: ActivityId,
}

/// Result from a background `gh pr view` merge-state poll. Shared by
/// both the Mergequeue and the `ReviewRequest` merge pollers - the JSON
/// shape and all fields are identical.
pub struct PrMergePollResult {
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
    /// (`repo_path`, branch) pairs for PRs that were successfully closed.
    /// Used to populate `cleanup_evicted_branches` so stale fetch data
    /// does not resurrect closed PRs as phantom unlinked items.
    pub closed_pr_branches: Vec<(PathBuf, String)>,
}

/// Info gathered on the main thread for one repo association, passed to
/// the background delete-cleanup thread for resource removal.
pub struct DeleteCleanupInfo {
    repo_path: PathBuf,
    branch: Option<String>,
    worktree_path: Option<PathBuf>,
    branch_in_main_worktree: bool,
    open_pr_number: Option<u64>,
    github_remote: Option<(String, String)>,
}

/// Result from the Phase 1 asynchronous session-open preparation
/// thread.
///
/// The UI thread must never touch the filesystem or spawn subprocesses
/// directly on the session-open path; this struct is the handoff point
/// where a Phase 1 background worker returns ALL the preparation work
/// (plan read, MCP socket bind, side-car file writes, temp
/// `--mcp-config` file write) so `finish_session_open` only has to do
/// pure-CPU work (system prompt + command building) before handing the
/// `Session::spawn` fork+exec off to a Phase 2 background thread (see
/// `SessionSpawnResult` / `poll_session_spawns`). See `docs/UI.md`
/// "Blocking I/O Prohibition" and `docs/harness-contract.md` C4 / C10.
/// Bundle of MCP-injection inputs threaded through
/// `App::build_agent_cmd_with`. Bundling keeps that helper's argument
/// count below clippy's `too_many_arguments` cap and lets every caller
/// see the three MCP fields together (instead of as three sibling
/// positional arguments that are easy to swap by mistake).
///
/// All three fields are borrows because the helper is a pure function
/// of its inputs and writes nothing into `App`. The `'a` lifetime is
/// the shorter of every borrowed view; in practice all three borrows
/// come from the same `SessionOpenPlanResult`-derived local state in
/// `finish_session_open`.
pub struct McpInjection<'a> {
    /// Path to the temp `--mcp-config` JSON the worker thread wrote.
    /// `None` when the worker could not stage the file (server start
    /// failed, exe-path resolution failed, write failed); the backend
    /// degrades cleanly and skips the flag.
    pub config_path: Option<&'a std::path::Path>,
    /// Structured spec for the workbridge MCP bridge (used by Codex
    /// per-key `-c` overrides). Mirrors `config_path` for harnesses
    /// that consume it via JSON; `None` when degraded.
    pub primary_bridge: Option<&'a crate::agent_backend::McpBridgeSpec>,
    /// Per-repo user-configured MCP servers from
    /// `Config::mcp_servers_for_repo`, already converted into
    /// `McpBridgeSpec` shape. Codex emits one `-c
    /// mcp_servers.<name>.*` pair per entry; Claude consumes them via
    /// the JSON file at `config_path`. Empty when the work item is not
    /// associated with a repo, or the repo has no extra servers.
    pub extra_bridges: &'a [crate::agent_backend::McpBridgeSpec],
}

pub struct SessionOpenPlanResult {
    /// The work item the session is being opened for.
    pub wi_id: WorkItemId,
    /// The worktree path where the agent CLI will run.
    pub cwd: PathBuf,
    /// Plan text read from the backend, if any. Empty string when the
    /// backend returned `Ok(None)` or an error (the caller treats an
    /// empty plan the same as a missing plan).
    pub plan_text: String,
    /// Human-readable error surfaced in the status bar when the backend
    /// read failed. `None` on success or when the backend reported no
    /// plan exists.
    pub read_error: Option<String>,
    /// MCP socket server handle produced by the worker. `None` if the
    /// worker could not start the server (`server_error` carries the
    /// reason). When `Some`, the UI thread moves this into
    /// `App::mcp_servers` on successful spawn.
    pub server: Option<McpSocketServer>,
    /// Human-readable error from the background MCP server start. Not
    /// fatal: a failed server still lets the session spawn in degraded
    /// mode without MCP tools, matching the pre-refactor behaviour.
    pub server_error: Option<String>,
    /// Backend-specific side-car files written on the background thread
    /// via `AgentBackend::write_session_files` (and the temp MCP config
    /// tempfile). Threaded into `SessionEntry::agent_written_files` so
    /// `delete_work_item_by_id` can hand the list back to
    /// `spawn_agent_file_cleanup` on teardown.
    pub written_files: Vec<PathBuf>,
    /// Path to the temp `--mcp-config` file (populated on the
    /// background thread). `None` when the server failed to start or
    /// the write itself failed; the backend sees `None` and falls back
    /// to the degraded argv path.
    pub mcp_config_path: Option<PathBuf>,
    /// Structured MCP bridge spec (workbridge binary + bridge args)
    /// for harnesses that register MCP servers via per-field CLI
    /// overrides (Codex). Computed on the background thread at the
    /// same time as `mcp_config_path`; `None` when the server failed
    /// to start or `std::env::current_exe` failed. See
    /// `agent_backend::McpBridgeSpec` for the shape and rationale.
    pub mcp_bridge: Option<crate::agent_backend::McpBridgeSpec>,
    /// Per-repo user-configured MCP servers (from
    /// `Config::mcp_servers_for_repo`), already converted into
    /// `McpBridgeSpec` shape so Codex can emit one `-c
    /// mcp_servers.<name>.command` / `mcp_servers.<name>.args` pair
    /// per entry. Claude consumes the same list via the JSON written
    /// to `mcp_config_path` (workbridge already passes
    /// `repo_mcp_servers` into `crate::mcp::build_mcp_config`); the
    /// structured copy here is what makes per-repo MCP servers
    /// visible to Codex sessions, which have no `--mcp-config` flag.
    /// HTTP-transport entries (Codex has no `mcp_servers.<name>.url`
    /// schema) are filtered out at construction time.
    pub extra_mcp_bridges: Vec<crate::agent_backend::McpBridgeSpec>,
    /// Non-fatal MCP config / side-car file write error. Surfaced to
    /// the user via the status bar but does not abort the spawn.
    pub mcp_config_error: Option<String>,
}

/// Per-entry state tracked alongside the `session_open_rx` map so
/// `poll_session_opens` can end the "Opening session..." spinner
/// started by `begin_session_open`. Stored in a named struct (rather
/// than a bare tuple) so the activity ID cannot be accidentally
/// dropped if the map grows new fields - a missed `end_activity`
/// would leak a permanent spinner in the status bar.
///
/// `cancelled` is a shared cancellation signal: the worker thread
/// loads it via `Ordering::Acquire` before each `std::fs::write`
/// (and before `McpSocketServer::start`) and skips the write when
/// set, so a cancelled open cannot leak side-car files to disk.
/// `drop_session_open_entry` (the canonical cancellation site) and
/// `cleanup_all_mcp` (the shutdown path) both set the flag via
/// `Ordering::Release` before scheduling the file cleanup. There is
/// still a sub-microsecond race window (worker reads the flag,
/// main thread sets the flag, worker proceeds with the stale
/// false) that this flag cannot fully close without a mutex
/// across the write itself, but the file is still committed to
/// `mcp_config_path` (below), which is known to the main thread
/// regardless of whether the worker reached the write.
///
/// `mcp_config_path` is the temp `--mcp-config` file path that the
/// UI thread commits to BEFORE spawning the worker (the worker
/// uses this exact path). It is routed through
/// `spawn_agent_file_cleanup` on cancellation so the tempfile
/// cannot be orphaned even if the worker writes it after the
/// cancellation flag was set.
///
/// `committed_files` is the shared running list of side-car files
/// the worker has actually written to disk (returned from
/// `AgentBackend::write_session_files`). The worker pushes each
/// successfully-written path into this `Mutex<Vec<PathBuf>>`
/// immediately after the write returns; the main thread drains it
/// on cancellation and feeds the entries into
/// `spawn_agent_file_cleanup` alongside `mcp_config_path`. This
/// closes the leak window where the worker has written a file but
/// `tx.send(...)` never reaches the main thread because the
/// receiver was already dropped (cancellation race), so the
/// `written_files` Vec inside `SessionOpenPlanResult` is silently
/// discarded along with the result.
pub struct SessionOpenPending {
    pub rx: crossbeam_channel::Receiver<SessionOpenPlanResult>,
    pub activity: ActivityId,
    pub cancelled: Arc<AtomicBool>,
    pub mcp_config_path: PathBuf,
    pub committed_files: Arc<Mutex<Vec<PathBuf>>>,
}

/// Result from the Phase 2 PTY spawn thread. `finish_session_open`
/// builds the command on the UI thread (pure CPU), then hands the
/// fork+exec off to a background thread so `Session::spawn` never
/// runs on the event loop. See `docs/UI.md` "Blocking I/O
/// Prohibition".
pub struct SessionSpawnResult {
    pub wi_id: WorkItemId,
    pub session_key: (WorkItemId, WorkItemStatus),
    pub session: Option<Session>,
    pub error: Option<String>,
    pub mcp_server: Option<McpSocketServer>,
    pub written_files: Vec<PathBuf>,
}

/// In-flight Phase 2 PTY spawn for a work-item session. Tracked so
/// `poll_session_spawns` can drain the result and the activity
/// spinner can be ended. Keyed by `WorkItemId` in
/// `App::session_spawn_rx`.
pub struct SessionSpawnPending {
    pub rx: crossbeam_channel::Receiver<SessionSpawnResult>,
    pub activity: ActivityId,
}

/// Fully-prepared global assistant session, produced entirely on a
/// background worker thread so no filesystem I/O or PTY fork/exec
/// runs on the event loop. See `docs/UI.md` "Blocking I/O Prohibition"
/// and `docs/harness-contract.md` C4 / C10.
///
/// On success the UI thread moves `session` and `mcp_server` into
/// `App::global_session` and `App::global_mcp_server`; the temp
/// `--mcp-config` path lives on `GlobalSessionOpenPending::config_path`
/// (committed BEFORE the worker starts, so the main thread owns the
/// cleanup on every path) and poll moves it into
/// `App::global_mcp_config_path` on success. On any error the
/// worker populates `error` and the UI thread resets the drawer to
/// closed, letting `teardown_global_session` handle the tempfile
/// cleanup via `spawn_agent_file_cleanup`.
pub struct GlobalSessionPrepResult {
    pub mcp_server: Option<McpSocketServer>,
    pub session: Option<Session>,
    pub error: Option<String>,
}

/// Per-entry state tracked while the global assistant preparation
/// worker is in flight. `activity` owns the "Opening global
/// assistant..." spinner. `pre_drawer_focus` is captured at spawn
/// time so we can restore it on a worker-reported error without
/// touching UI-thread state during the drop path.
///
/// `config_path` is computed on the UI thread BEFORE spawning the
/// worker (not inside the worker) so the main thread always knows
/// the tempfile path the worker will create - this is what lets
/// `teardown_global_session` clean up a cancelled worker's file
/// ownership-correctly instead of racing the worker's own cleanup
/// on the send-error arm. The path is per-call unique (UUID) so
/// two concurrent workers under rapid drawer toggling can never
/// collide on a shared filename.
///
/// `cancelled` is a shared cancellation signal: the worker thread
/// loads it via `Ordering::Acquire` before each `std::fs::write`,
/// `McpSocketServer::start_global`, and `std::fs::create_dir_all`
/// and skips the op when set. `teardown_global_session` and
/// `cleanup_all_mcp` both set it via `Ordering::Release` before
/// scheduling the file cleanup. See the matching doc on
/// `SessionOpenPending::cancelled` for the residual race-window
/// caveat.
pub struct GlobalSessionOpenPending {
    pub rx: crossbeam_channel::Receiver<GlobalSessionPrepResult>,
    pub activity: ActivityId,
    pub pre_drawer_focus: FocusPanel,
    pub config_path: PathBuf,
    pub cancelled: Arc<AtomicBool>,
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
    /// When the failure was because the branch is locked to a stale worktree
    /// (e.g. after an interrupted rebase). Holds the path where git says the
    /// branch is locked, so the recovery dialog can show it and offer to
    /// force-remove it.
    pub stale_worktree_path: Option<PathBuf>,
}

/// An orphaned worktree captured from an in-flight worktree-create
/// result at the moment a delete is confirmed. Threaded back to the
/// caller of `delete_work_item_by_id` so the caller can synthesize a
/// `DeleteCleanupInfo` for `spawn_delete_cleanup` - keeping the
/// `git worktree remove` and `git branch -D` off the UI thread. The
/// branch name is preserved so the cleanup thread deletes the stale
/// branch ref too (dropping it here would leak the branch on master's
/// pre-P0-fix behaviour).
pub struct OrphanWorktree {
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: Option<String>,
}

/// State for the stale-worktree recovery dialog. Shown when worktree
/// creation fails because the branch is already locked to a stale/corrupt
/// worktree (e.g. after an interrupted rebase). The user can force-remove
/// the stale worktree and retry, or dismiss.
pub struct StaleWorktreePrompt {
    pub wi_id: WorkItemId,
    pub error: String,
    pub stale_path: PathBuf,
    pub repo_path: PathBuf,
    pub branch: String,
    /// Whether a Claude session should be opened after successful recovery.
    /// `true` for the normal Enter-to-open path, `false` for import paths
    /// that only need a worktree without spawning a session.
    pub open_session: bool,
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
    /// GitHub client used by the merge precheck to re-fetch the live
    /// PR mergeable flag and CI rollup before admitting a merge.
    /// Injected via the trait so tests can drive the conflict /
    /// CI-failing / no-PR / error branches without shelling out to
    /// `gh`. Production threads a `GhCliClient` in via
    /// `App::with_config_worktree_and_github`; the test-only default
    /// constructor swaps in `StubGithubClient` which always reports
    /// "no open PR" so the precheck classifier falls through to the
    /// worktree-only classification.
    pub github_client: Arc<dyn crate::github_client::GithubClient + Send + Sync>,
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
    /// Pluggable LLM harness adapter that knows how to build argv for the
    /// three spawn profiles (work-item, review-gate, global) and write any
    /// backend-specific side-car files (`config.toml`, etc.).
    /// Every place that previously hard-coded `claude` flags now goes
    /// through this trait object. See `crate::agent_backend` and
    /// `docs/harness-contract.md`.
    pub agent_backend: Arc<dyn AgentBackend>,
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
    pub last_k_press: Option<(WorkItemId, Instant)>,
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
    /// Monotonic counter for generating unique `ActivityId` values.
    pub activity_counter: u64,
    /// Currently running activities. The last entry is displayed in the
    /// status bar. When empty, the normal `status_message` shows through.
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
    /// Phase 2 PTY spawn results. `finish_session_open` hands the
    /// `Session::spawn` call off to a background thread; the result
    /// flows back here and is drained by `poll_session_spawns` on the
    /// next timer tick. Keyed by work item so concurrent spawns for
    /// different items do not collide.
    pub session_spawn_rx: HashMap<WorkItemId, SessionSpawnPending>,

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
    /// In-flight preparation for the global assistant session.
    /// Populated by `spawn_global_session` while a background worker
    /// runs `McpSocketServer::start_global`, `std::fs::write` on the
    /// `--mcp-config` tempfile, `std::fs::create_dir_all` on the
    /// scratch cwd, and `Session::spawn` itself. Drained by
    /// `poll_global_session_open` on each background tick, which
    /// moves the worker's result into the three durable fields
    /// (`global_session`, `global_mcp_server`, `global_mcp_config_path`)
    /// or restores the drawer state on failure. Kept as a named
    /// struct (rather than a tuple) so the activity ID cannot be
    /// accidentally dropped and leak a permanent spinner.
    pub global_session_open_pending: Option<GlobalSessionOpenPending>,
    /// True when repo/work-item data has changed since the last
    /// `refresh_global_mcp_context` call. Set by `drain_fetch_results`
    /// returning true; cleared after the refresh runs.
    pub global_mcp_context_dirty: bool,
    /// Buffered bytes destined for the active PTY session. Key events
    /// that forward to the PTY push here instead of writing immediately.
    /// Flushed as a single write on the next timer tick so the child
    /// process receives all characters in one `read()` - matching how a
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

    /// Per-frame click-to-copy target registry. Populated during draw
    /// (via `&App`, which is why this is a `RefCell`), consumed by
    /// `handle_mouse`. Cleared at the top of every frame.
    pub click_registry: RefCell<ClickRegistry>,

    /// Tracks a pending click-to-copy gesture between `Down(Left)` and
    /// `Up(Left)`. A drag or an `Up` outside the original target
    /// cancels the gesture. Stored as `(col, row, kind, value)` in
    /// absolute frame coordinates.
    pub pending_chrome_click: Option<(u16, u16, ClickKind, String)>,

    /// Transient top-right toast notifications. Newest is at the end of
    /// the vector. Pruned each tick by `prune_toasts`.
    pub toasts: Vec<Toast>,
}

/// Spawn a background thread that runs
/// `gh pr view <target> --repo <owner/repo> --json state,number,title,url`
/// and sends exactly one `PrMergePollResult` through the returned
/// receiver. Shared by `poll_mergequeue` and `poll_review_request_merges`
/// so the `gh` invocation and JSON parsing live in a single place.
///
/// `target` is the pinned PR number when known (unambiguous), otherwise
/// the branch name. The branch fallback is used on watches reconstructed
/// from a backend record after an app restart, and on all `ReviewRequest`
/// watches where the `--author @me` fetch never populated `assoc.pr`.
/// The poll's caller backfills the resolved number into the watch on
/// the first successful result so subsequent polls target the exact PR.
///
/// Every outcome (success, non-zero exit, spawn failure, JSON parse
/// failure) is delivered as a single send on `tx`. Errors are encoded
/// as `pr_state: "ERROR: ..."` so the caller can handle them uniformly.
fn spawn_gh_pr_view_poll(
    wi_id: WorkItemId,
    pr_number: Option<u64>,
    owner_repo: String,
    branch: String,
    repo_path: PathBuf,
) -> crossbeam_channel::Receiver<PrMergePollResult> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let target = pr_number.map_or_else(|| branch.clone(), |n| n.to_string());
        let outcome = match std::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &target,
                "--repo",
                &owner_repo,
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
                        let _ = tx.send(PrMergePollResult {
                            wi_id,
                            pr_state: format!("ERROR: JSON parse failed: {e}"),
                            branch,
                            repo_path,
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
                let pr_identity = parsed
                    .get("number")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|number| {
                        let title = parsed.get("title")?.as_str()?.to_string();
                        let url = parsed.get("url")?.as_str()?.to_string();
                        Some(PrIdentityRecord { number, title, url })
                    });
                PrMergePollResult {
                    wi_id,
                    pr_state: state,
                    branch,
                    repo_path,
                    pr_identity,
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                PrMergePollResult {
                    wi_id,
                    pr_state: format!("ERROR: {}", stderr.trim()),
                    branch,
                    repo_path,
                    pr_identity: None,
                }
            }
            Err(e) => PrMergePollResult {
                wi_id,
                pr_state: format!("ERROR: {e}"),
                branch,
                repo_path,
                pr_identity: None,
            },
        };
        let _ = tx.send(outcome);
    });
    rx
}

/// Generate a `poll_*` method that drives one PR-merge poller instance.
///
/// `poll_mergequeue` and `poll_review_request_merges` differ only in
/// which `App` fields they touch (watches / in-flight polls / errors),
/// which stage they treat as "eligible", and a few static data bits
/// (strategy tag, status messages, whether the merged branch runs
/// `cleanup_worktree_for_item`). Everything else - the Phase 1 drain
/// loop, the still-eligible guard, the `pr_number` backfill, the merge-
/// gate dispatch, the Phase 2 cooldown, the subprocess spawn - is
/// identical. Expressing both via a macro keeps the two methods on one
/// source of truth so they cannot drift as the `gh` path, the merge-
/// gate, or the JSON schema evolve.
///
/// See `spawn_gh_pr_view_poll` for the subprocess body itself.
macro_rules! impl_pr_merge_poll_method {
    (
        $(#[$meta:meta])*
        fn $method_name:ident,
        watches = $watches_field:ident,
        polls = $polls_field:ident,
        errors = $errors_field:ident,
        source_stage = $source_stage:expr,
        kind_filter = $kind_filter:expr,
        strategy_tag = $strategy_tag:expr,
        merged_message = $merged_message:expr,
        closed_message = $closed_message:expr,
        poll_error_prefix = $poll_error_prefix:expr,
        poll_label_prefix = $poll_label_prefix:expr,
        cleanup_worktree_on_merge = $cleanup_worktree:expr,
    ) => {
        $(#[$meta])*
        pub fn $method_name(&mut self) {
            // -- Phase 1: drain any in-flight results --
            // Collect into locals before acting so we don't borrow `self`
            // twice when calling into `apply_stage_change`, `end_activity`,
            // etc.
            let mut ready: Vec<PrMergePollResult> = Vec::new();
            let mut to_remove: Vec<WorkItemId> = Vec::new();
            for (wi_id, state) in &self.$polls_field {
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
                if let Some(state) = self.$polls_field.remove(wi_id) {
                    self.end_activity(state.activity);
                }
            }

            for result in ready {
                // Actual-status guard: re-check the item is still
                // eligible. The user may have retreated / deleted it
                // between the spawn and the drain.
                let kind_filter: Option<WorkItemKind> = $kind_filter;
                let still_eligible = self.work_items.iter().any(|w| {
                    w.id == result.wi_id
                        && w.status == $source_stage
                        && kind_filter
                            .as_ref()
                            .is_none_or(|k| w.kind == *k)
                });
                if !still_eligible {
                    // Item moved away - drop the watch / error entries
                    // so nothing lingers. Structural ownership via the
                    // maps.
                    self.$watches_field.retain(|w| w.wi_id != result.wi_id);
                    self.$errors_field.remove(&result.wi_id);
                    continue;
                }

                // Backfill pr_number on the watch the first time a
                // branch-based poll resolves to a concrete PR. This
                // pins subsequent polls to the exact PR so a
                // closed-then-reopened-on-same-branch race cannot
                // redirect the watch to a different PR.
                if let Some(identity) = &result.pr_identity
                    && let Some(watch) = self
                        .$watches_field
                        .iter_mut()
                        .find(|w| w.wi_id == result.wi_id)
                    && watch.pr_number.is_none()
                {
                    watch.pr_number = Some(identity.number);
                }

                match result.pr_state.as_str() {
                    "MERGED" => {
                        // Persist PR identity BEFORE the stage change
                        // so the subsequent `reassemble_work_items`
                        // (fired from inside `apply_stage_change`)
                        // finds the snapshot and can synthesize a
                        // `PrInfo { state: Merged }` via the assembly
                        // fallback. That in turn makes
                        // `status_derived = true` and gives the item a
                        // stable merged-PR link in the UI even after
                        // the next fetch cycle clears any transient
                        // data.
                        if let Some(identity) = &result.pr_identity
                            && let Err(e) = self.backend.save_pr_identity(
                                &result.wi_id,
                                &result.repo_path,
                                identity,
                            )
                        {
                            self.status_message =
                                Some(format!("PR identity save error: {e}"));
                        }

                        let log_entry = ActivityEntry {
                            timestamp: now_iso8601(),
                            event_type: "pr_merged".to_string(),
                            payload: serde_json::json!({
                                "strategy": $strategy_tag,
                                "branch": result.branch,
                            }),
                        };
                        if let Err(e) =
                            self.backend.append_activity(&result.wi_id, &log_entry)
                        {
                            self.status_message =
                                Some(format!("Activity log error: {e}"));
                        }

                        if $cleanup_worktree {
                            self.cleanup_worktree_for_item(&result.wi_id);
                        }

                        self.$watches_field.retain(|w| w.wi_id != result.wi_id);
                        self.$errors_field.remove(&result.wi_id);

                        self.apply_stage_change(
                            &result.wi_id,
                            $source_stage,
                            WorkItemStatus::Done,
                            "pr_merge",
                        );
                        self.status_message = Some($merged_message.into());
                    }
                    "CLOSED" => {
                        // A closed PR is NOT a merge - it must not
                        // bypass the merge-gate invariant. Leave the
                        // watch in place so we keep observing (in case
                        // somebody reopens the same PR) and surface a
                        // distinct warning.
                        self.$errors_field.remove(&result.wi_id);
                        self.status_message = Some($closed_message.into());
                    }
                    s if s.starts_with("ERROR:") => {
                        let msg = format!(
                            "{} for {}: {}",
                            $poll_error_prefix, result.branch, result.pr_state
                        );
                        self.$errors_field
                            .insert(result.wi_id.clone(), msg.clone());
                        self.status_message = Some(msg);
                        // Item stays in its source stage - will retry
                        // on next poll cycle.
                    }
                    _ => {
                        // Still open - no action, will poll again next
                        // cycle.
                        self.$errors_field.remove(&result.wi_id);
                    }
                }
            }

            // -- Phase 2: spawn polls for any watch whose per-item
            // cooldown has elapsed and which has no in-flight poll. --
            let cooldown = std::time::Duration::from_secs(30);
            let now = crate::side_effects::clock::instant_now();
            let mut to_spawn: Vec<(WorkItemId, Option<u64>, String, String, PathBuf)> =
                Vec::new();
            for watch in &self.$watches_field {
                if self.$polls_field.contains_key(&watch.wi_id) {
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
                let rx = spawn_gh_pr_view_poll(
                    wi_id.clone(),
                    pr_number,
                    owner_repo,
                    branch.clone(),
                    repo_path,
                );
                let activity =
                    self.start_activity(format!("{} ({branch})", $poll_label_prefix));
                self.$polls_field
                    .insert(wi_id.clone(), PrMergePollState { rx, activity });
                if let Some(w) = self
                    .$watches_field
                    .iter_mut()
                    .find(|w| w.wi_id == wi_id)
                {
                    w.last_polled = Some(now);
                }
            }
        }
    };
}

/// Generate a `reconstruct_*_watches` method that rebuilds one PR-merge
/// poller's watch list from the current `work_items` snapshot.
///
/// Both pollers share the same reconstruction shape: idempotently add a
/// watch for each eligible item that doesn't already have one. This is
/// strictly additive - stale watches / in-flight polls / errors for
/// items that are no longer eligible are cleaned up lazily by the
/// corresponding poll method's Phase 1 drain (via its still-eligible
/// guard), not here. That matches the historical `poll_mergequeue` flow
/// and avoids a subtle interaction with `reassemble_work_items`: after
/// a transient backend-read failure `work_items` can briefly be empty
/// even though the real state hasn't changed, and a proactive prune
/// here would then evict live watches that should have survived the
/// read.
macro_rules! impl_pr_merge_reconstruct_method {
    (
        $(#[$meta:meta])*
        fn $method_name:ident,
        watches = $watches_field:ident,
        source_stage = $source_stage:expr,
        kind_filter = $kind_filter:expr,
    ) => {
        $(#[$meta])*
        pub fn $method_name(&mut self) {
            let kind_filter: Option<WorkItemKind> = $kind_filter;

            for wi in &self.work_items {
                if wi.status != $source_stage {
                    continue;
                }
                if let Some(k) = kind_filter.as_ref()
                    && wi.kind != *k
                {
                    continue;
                }
                if self.$watches_field.iter().any(|w| w.wi_id == wi.id) {
                    continue;
                }
                let Some(assoc) = wi.repo_associations.first() else {
                    continue;
                };
                let Some(ref branch) = assoc.branch else {
                    continue;
                };
                // Read the GitHub remote from the cached fetcher
                // result so we never shell out to `git remote get-url`
                // on the UI thread. When the fetcher has not yet
                // populated this repo, skip - the next reassembly
                // (triggered on fetch completion) will retry.
                let Some((owner, repo_name)) = self
                    .repo_data
                    .get(&assoc.repo_path)
                    .and_then(|rd| rd.github_remote.clone())
                else {
                    continue;
                };
                // If the fetcher has already populated assoc.pr, pin
                // the number immediately. Otherwise the watch starts
                // with pr_number = None and the first poll falls back
                // to `gh pr view <branch>`. For ReviewRequest items
                // the fetcher almost never populates this (the
                // `--author @me` filter drops their author-side PRs),
                // so the branch fallback is the normal path there.
                let pr_number = assoc.pr.as_ref().map(|pr| pr.number);
                self.$watches_field.push(PrMergeWatch {
                    wi_id: wi.id.clone(),
                    pr_number,
                    owner_repo: format!("{owner}/{repo_name}"),
                    branch: branch.clone(),
                    repo_path: assoc.repo_path.clone(),
                    last_polled: None,
                });
            }
        }
    };
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
            stale_worktree_prompt: None,
            stale_recovery_in_progress: false,
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
            github_client,
            work_items: Vec::new(),
            unlinked_prs: Vec::new(),
            review_requested_prs: Vec::new(),
            current_user_login: None,
            sessions: HashMap::new(),
            repo_data: HashMap::new(),
            fetch_rx: None,
            gh_available: Self::check_gh_available(),
            gh_cli_not_found_shown: false,
            gh_auth_required_shown: false,
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
            agent_backend: Arc::new(ClaudeCodeBackend),
            harness_choice: HashMap::new(),
            last_k_press: None,
            first_run_global_harness_modal: None,
            mcp_servers: HashMap::new(),
            agent_working: std::collections::HashSet::new(),
            mcp_rx: Some(mcp_rx),
            mcp_tx,
            review_gates: HashMap::new(),
            rebase_gates: HashMap::new(),
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
            review_request_merge_watches: Vec::new(),
            review_request_merge_polls: HashMap::new(),
            review_request_merge_poll_errors: HashMap::new(),
            pr_identity_backfill_rx: None,
            pr_identity_backfill_activity: None,
            session_open_rx: HashMap::new(),
            session_spawn_rx: HashMap::new(),
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
            global_session_open_pending: None,
            global_mcp_context_dirty: false,
            pending_active_pty_bytes: Vec::new(),
            pending_global_pty_bytes: Vec::new(),
            right_panel_tab: RightPanelTab::ClaudeCode,
            terminal_sessions: HashMap::new(),
            pending_terminal_pty_bytes: Vec::new(),
            click_registry: RefCell::new(ClickRegistry::default()),
            pending_chrome_click: None,
            toasts: Vec::new(),
        };
        app.reassemble_work_items();
        app.build_display_list();
        app
    }

    // -- Toast API --

    /// Show a transient top-right toast for ~2 seconds. Newest toasts
    /// stack at the top of the column. Multiple calls in quick
    /// succession produce a visible stack; each auto-dismisses
    /// independently. Called from `fire_chrome_copy` and any other
    /// handler that wants to surface a short confirmation without
    /// hijacking the status bar.
    pub fn push_toast(&mut self, text: String) {
        self.toasts.push(Toast {
            text,
            expires_at: crate::side_effects::clock::instant_now() + Duration::from_secs(2),
        });
    }

    /// Drop any toasts whose deadline has passed. Cheap - called from
    /// the per-tick hook in `salsa::app_event`. Keeps the vector from
    /// growing unbounded and is the only thing that removes toasts.
    pub fn prune_toasts(&mut self) {
        let now = crate::side_effects::clock::instant_now();
        self.toasts.retain(|t| t.expires_at > now);
    }

    /// Fire a click-to-copy action: write `value` to the clipboard via
    /// the OSC 52 + arboard backend and push a confirmation toast. The
    /// toast shows a short-form of `value` based on `kind` so long
    /// URLs and file paths do not overflow the frame.
    ///
    /// Branches on the clipboard backend's return value: on success
    /// the toast reads `Copied: <short>`; on failure it reads
    /// `Copy failed: <short>`. Lying about the clipboard state is
    /// the worst UX failure mode for this feature - a user who
    /// believes the copy succeeded will paste stale content and
    /// only notice long after the fact. `clipboard::copy` returns
    /// `true` iff at least one of OSC 52 (stdout write + flush) or
    /// `arboard` (native clipboard) succeeded; a `false` result
    /// means neither path even delivered bytes, so the clipboard
    /// definitely does not hold `value`.
    ///
    /// Does not touch the PTY selection state - this path is
    /// independent of the existing drag-select copy flow.
    pub fn fire_chrome_copy(&mut self, value: String, kind: ClickKind) {
        let ok = crate::side_effects::clipboard::copy(&value);
        let short = short_display(&value, kind);
        let text = if ok {
            format!("Copied: {short}")
        } else {
            format!("Copy failed: {short}")
        };
        self.push_toast(text);
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
    pub const fn has_visible_status_bar(&self) -> bool {
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
    pub const fn total_repos(&self) -> usize {
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
        if self.active_repo_cache.is_empty() {
            self.settings_repo_selected = 0;
        } else {
            self.settings_repo_selected = self
                .settings_repo_selected
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

    /// Check liveness (`try_wait`) on all sessions. Called on periodic ticks.
    ///
    /// The reader threads handle PTY output continuously - no reading
    /// happens here. This only checks if child processes have exited.
    /// Also cleans up MCP servers and side-car files for dead sessions.
    pub fn check_liveness(&mut self) {
        let mut dead_ids: Vec<WorkItemId> = Vec::new();
        let mut dead_implementing: Vec<WorkItemId> = Vec::new();
        for ((wi_id, stage), entry) in &mut self.sessions {
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
            let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
                continue;
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
            if let Some(mut entry) = self.sessions.remove(&key) {
                // Drain side-car files before dropping the entry so
                // the `--mcp-config` tempfile is
                // cleaned up even when the session is removed as a
                // stage-mismatch orphan.
                let files = std::mem::take(&mut entry.agent_written_files);
                if !files.is_empty() {
                    self.spawn_agent_file_cleanup(files);
                }
                if let Some(mut session) = entry.session.take() {
                    session.kill();
                }
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
                // Symmetric with `teardown_global_session`: when the
                // assistant child dies on its own (crash, OOM,
                // `/exit`), the `--mcp-config` tempfile it was using
                // is no longer referenced and would otherwise leak
                // to `/tmp` until the next workbridge run. Route
                // the removal through `spawn_agent_file_cleanup` so
                // the `std::fs::remove_file` runs off the UI thread.
                if let Some(path) = self.global_mcp_config_path.take() {
                    self.spawn_agent_file_cleanup(vec![path]);
                }
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
        self.agent_working.remove(wi_id);
        // Drain agent-written side-car files from the live session
        // entry (if any) so that natural session death (detected by
        // `check_liveness`) removes the `--mcp-config` tempfile
        // instead of leaking it. The
        // delete path (`delete_work_item_by_id`) does its own
        // `std::mem::take` after `sessions.remove`, so this is a
        // no-op there - but here the entry stays in
        // `self.sessions` (the session is dead, not deleted) and
        // would otherwise silently drop its file list when the
        // entry is later replaced by a reopened session.
        if let Some(key) = self.session_key_for(wi_id)
            && let Some(entry) = self.sessions.get_mut(&key)
        {
            let files = std::mem::take(&mut entry.agent_written_files);
            if !files.is_empty() {
                self.spawn_agent_file_cleanup(files);
            }
        }
        // Cancel any pending background session-open: signal the
        // worker to skip remaining file writes, route the committed
        // `mcp_config_path` through `spawn_agent_file_cleanup`, and
        // end the "Opening session..." spinner. The worker will
        // then finish and try to send; the send fails because the
        // receiver is gone, and the thread exits.
        // `finish_session_open` also has its own deleted-work-item
        // guard as a second line of defence.
        self.cancel_session_open_entry(wi_id);
        // Cancel any pending Phase 2 PTY spawn. The worker's
        // Session::spawn may already be in flight; when it
        // completes, `poll_session_spawns` will see that the item
        // no longer exists (or the stage mismatches) and drop the
        // session. Removing the pending entry here ends the
        // "Spawning agent session..." spinner immediately.
        if let Some(pending) = self.session_spawn_rx.remove(wi_id) {
            // If the Phase 2 worker already sent a result, drain it
            // so Session::Drop and McpSocketServer::Drop do not run
            // on the UI thread when the receiver is dropped.
            if let Ok(result) = pending.rx.try_recv() {
                if let Some(server) = result.mcp_server {
                    self.drop_mcp_server_off_thread(server);
                }
                self.spawn_agent_file_cleanup(result.written_files);
                // Session::Drop must also run off the UI thread -
                // it kills/joins the child process.
                if let Some(session) = result.session {
                    std::thread::spawn(move || drop(session));
                }
            }
            self.end_activity(pending.activity);
        }
    }

    /// Stop all MCP servers, clear activity state, and remove temp config files.
    /// Called on app exit.
    pub fn cleanup_all_mcp(&mut self) {
        self.mcp_servers.clear();
        self.agent_working.clear();
        self.global_mcp_server = None;
        // Route every tempfile removal off the UI thread.
        // `cleanup_all_mcp` runs during graceful shutdown but the
        // event loop is still alive for up to 10 seconds (waiting
        // for child processes to exit); a wedged filesystem would
        // freeze the shutdown-wait UI otherwise. See `docs/UI.md`
        // "Blocking I/O Prohibition".
        //
        // We collect paths from FIVE sources so every in-flight
        // or live tempfile is caught:
        //   1. Live global assistant session (`global_mcp_config_path`)
        //   2. In-flight global preparation worker
        //      (`global_session_open_pending.config_path`)
        //   3. In-flight work-item preparation workers
        //      (`session_open_rx` entries' `mcp_config_path`)
        //   4. In-flight Phase 2 PTY spawn workers
        //      (`session_spawn_rx` entries)
        //   5. Live work-item sessions
        //      (`SessionEntry::agent_written_files`)
        //
        // For (2) and (3) we also flip each worker's `cancelled`
        // flag via `Ordering::Release` so workers that have not yet
        // reached their Phase C `std::fs::write` skip the write and
        // exit. Workers that already wrote before we flip the flag
        // leave files on disk; the scheduled
        // `spawn_agent_file_cleanup` removes them asynchronously
        // on the same background thread.
        let mut files_to_clean: Vec<PathBuf> = Vec::new();
        if let Some(path) = self.global_mcp_config_path.take() {
            files_to_clean.push(path);
        }
        if let Some(pending) = self.global_session_open_pending.take() {
            pending.cancelled.store(true, Ordering::Release);
            // Drain any queued result so its handles are disposed
            // off the UI thread.
            if let Ok(result) = pending.rx.try_recv() {
                if let Some(server) = result.mcp_server {
                    self.drop_mcp_server_off_thread(server);
                }
                if let Some(session) = result.session {
                    std::thread::spawn(move || drop(session));
                }
            }
            self.end_activity(pending.activity);
            files_to_clean.push(pending.config_path);
        }
        let pending_wi_ids: Vec<WorkItemId> = self.session_open_rx.keys().cloned().collect();
        for wi_id in pending_wi_ids {
            if let Some(entry) = self.session_open_rx.remove(&wi_id) {
                entry.cancelled.store(true, Ordering::Release);
                // Drain any queued result so its MCP server is
                // disposed off the UI thread.
                if let Ok(result) = entry.rx.try_recv()
                    && let Some(server) = result.server
                {
                    self.drop_mcp_server_off_thread(server);
                }
                self.end_activity(entry.activity);
                // Drain side-car files the worker already wrote.
                // Symmetric with `cancel_session_open_entry`'s
                // cleanup so shutdown does not leak them.
                if let Ok(mut guard) = entry.committed_files.lock() {
                    files_to_clean.extend(guard.drain(..));
                }
                files_to_clean.push(entry.mcp_config_path);
            }
        }
        // 4. In-flight Phase 2 PTY spawn workers
        //    (`session_spawn_rx` entries). The worker's
        //    `Session::spawn` may still be in flight; when it
        //    completes the `tx.send` will fail (receiver dropped)
        //    and the Session + MCP server Drops will run. We just
        //    end the activity spinner here.
        let spawn_wi_ids: Vec<WorkItemId> = self.session_spawn_rx.keys().cloned().collect();
        for wi_id in spawn_wi_ids {
            if let Some(pending) = self.session_spawn_rx.remove(&wi_id) {
                // Drain any queued result so its handles are
                // disposed off the UI thread.
                if let Ok(result) = pending.rx.try_recv() {
                    if let Some(server) = result.mcp_server {
                        self.drop_mcp_server_off_thread(server);
                    }
                    files_to_clean.extend(result.written_files);
                    if let Some(session) = result.session {
                        std::thread::spawn(move || drop(session));
                    }
                }
                self.end_activity(pending.activity);
            }
        }
        // 5. Live work-item sessions: drain agent_written_files
        //    so the --mcp-config tempfile is cleaned up even if
        //    the user force-quits during the shutdown wait before
        //    check_liveness observes the child exit.
        for entry in self.sessions.values_mut() {
            files_to_clean.extend(std::mem::take(&mut entry.agent_written_files));
        }
        self.spawn_agent_file_cleanup(files_to_clean);
    }

    /// Resize PTY sessions and vt100 parsers to match the current pane
    /// dimensions. Resize is an instant ioctl call, so we resize all
    /// sessions immediately. The first resize failure per call is surfaced
    /// via `status_message`.
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
    ///
    /// Also tears down all in-flight rebase gates immediately. Rebase
    /// gates do NOT have a graceful-exit path: the headless harness
    /// process does not handle SIGTERM (it is `claude --print`, not
    /// an interactive PTY session), so there is nothing to "wait
    /// for". Dropping the gate here SIGKILLs the harness process
    /// group via `Drop for RebaseGateState`, which is safe because
    /// the rebase gate's own state is structural - the next
    /// `all_dead`/`all_background_done` check will see the empty
    /// map and let the shutdown loop proceed. Without this call,
    /// pressing Q while a rebase was in flight (with no other PTY
    /// session alive) would let the shutdown loop exit immediately
    /// (because `all_dead` only checks PTY sessions) and leave the
    /// harness child running against the worktree, which is the
    /// failure mode docs/harness-contract.md C10 calls out.
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
        // Cancel all in-flight rebase gates. SIGKILL via Drop is
        // immediate; no second pass needed in `force_kill_all`
        // because by the time the loop reaches that path the
        // rebase_gates map is already empty. The `force_kill_all`
        // version of this loop is left in place so that an explicit
        // force-quit (signal-during-shutdown / 10s deadline) is
        // still safe even if a future caller bypasses
        // `send_sigterm_all`.
        let rebase_keys: Vec<WorkItemId> = self.rebase_gates.keys().cloned().collect();
        for key in rebase_keys {
            self.drop_rebase_gate(&key);
        }
    }

    /// Check if all sessions are dead (or there are no sessions).
    /// Also returns false if any rebase gate is still tracked: the
    /// rebase gate is a long-running background op with its own
    /// process tree, and the shutdown loop must not let `Control::Quit`
    /// fire while one is in flight or workbridge will exit before
    /// the harness has been signalled. `send_sigterm_all` empties
    /// the `rebase_gates` map on the first shutdown tick, so this
    /// check is satisfied as soon as the SIGKILL has propagated;
    /// the explicit dependency keeps any future caller that adds a
    /// new shutdown entrypoint from accidentally letting the loop
    /// drop through with rebase gates still alive.
    pub fn all_dead(&self) -> bool {
        self.sessions.values().all(|entry| !entry.alive)
            && self.global_session.as_ref().is_none_or(|s| !s.alive)
            && self.terminal_sessions.values().all(|entry| !entry.alive)
            && self.rebase_gates.is_empty()
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
        // Cancel all in-flight rebase gates. `drop_rebase_gate`
        // SIGKILLs the harness child if it is still running, so
        // force-quit cannot leave a `claude` / `git rebase` process
        // mutating a worktree after the TUI exits. Mirrors the
        // review-gate loop above; the helper is the single place that
        // knows how to tear a rebase gate down.
        let rebase_keys: Vec<WorkItemId> = self.rebase_gates.keys().cloned().collect();
        for key in rebase_keys {
            self.drop_rebase_gate(&key);
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
    ///
    /// Each per-session flush is gated on the corresponding session
    /// actually existing AND being alive. Without that gate,
    /// `send_bytes_to_*` is a no-op and the keystrokes get silently
    /// dropped, because `std::mem::take` already cleared the buffer
    /// before the helper noticed there was nowhere to write to. This
    /// matters for the global assistant in particular: after the
    /// async `spawn_global_session` refactor (see
    /// `docs/harness-contract.md` C10), `App::global_session` is
    /// `None` for ~one timer tick between drawer-open and the
    /// background worker installing the session via
    /// `poll_global_session_open`. Keystrokes the user types in
    /// that window stay parked in `pending_global_pty_bytes` until
    /// the session is installed, then flush in one batch on the
    /// next tick. Same gate applies to the work-item active pane
    /// (worker session-open) and the terminal pane.
    pub fn flush_pty_buffers(&mut self) {
        if !self.pending_active_pty_bytes.is_empty() && self.has_alive_active_session() {
            let data = std::mem::take(&mut self.pending_active_pty_bytes);
            self.send_bytes_to_active(&data);
        }
        if !self.pending_global_pty_bytes.is_empty()
            && self
                .global_session
                .as_ref()
                .is_some_and(|e| e.alive && e.session.is_some())
        {
            let data = std::mem::take(&mut self.pending_global_pty_bytes);
            self.send_bytes_to_global(&data);
        }
        if !self.pending_terminal_pty_bytes.is_empty() && self.has_alive_terminal_session() {
            let data = std::mem::take(&mut self.pending_terminal_pty_bytes);
            self.send_bytes_to_terminal(&data);
        }
    }

    /// True when the active (work-item) session for the currently
    /// selected work item exists and is alive. Used by
    /// `flush_pty_buffers` to gate the work-item PTY flush so
    /// keystrokes typed during a session-open worker's in-flight
    /// window are not silently dropped on the floor.
    fn has_alive_active_session(&self) -> bool {
        let Some(work_item_id) = self.selected_work_item_id() else {
            return false;
        };
        let Some(key) = self.session_key_for(&work_item_id) else {
            return false;
        };
        self.sessions
            .get(&key)
            .is_some_and(|e| e.alive && e.session.is_some())
    }

    /// True when the terminal session for the currently selected
    /// work item exists and is alive. Symmetric with
    /// `has_alive_active_session` so the terminal pane behaves the
    /// same on the keystroke-buffering path.
    fn has_alive_terminal_session(&self) -> bool {
        let Some(work_item_id) = self.selected_work_item_id() else {
            return false;
        };
        self.terminal_sessions
            .get(&work_item_id)
            .is_some_and(|e| e.alive && e.session.is_some())
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
                        agent_written_files: Vec::new(),
                    },
                );
            }
            Err(e) => {
                self.status_message = Some(format!("Terminal spawn error: {e}"));
            }
        }
    }

    /// Get the terminal `SessionEntry` for the currently selected work item.
    pub fn active_terminal_entry(&self) -> Option<&SessionEntry> {
        let wi_id = self.selected_work_item_id()?;
        self.terminal_sessions.get(&wi_id)
    }

    /// Get a mutable terminal `SessionEntry` for the currently selected work item.
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
    /// Calls `try_recv()` in a loop until the channel is empty, storing each
    /// `RepoData` result in `self.repo_data`. `FetcherError` messages are surfaced
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
                    // Capture the authenticated user's login so review-
                    // request row rendering can classify direct-to-you vs.
                    // team. Never clobber a known login with None - a
                    // transient `gh api user` failure should not erase a
                    // value that was successfully resolved earlier in the
                    // session.
                    if let Some(login) = result.current_user_login.clone() {
                        self.current_user_login = Some(login);
                    }
                    self.repo_data.insert(result.repo_path.clone(), *result);
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
    /// Calls `backend.list()` for fresh records, then runs the assembly
    /// layer to produce `work_items` and `unlinked_prs`. Surfaces any
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
        let (items, unlinked, review_requested, mut reopen_ids) = assembly::reassemble(
            &list_result.records,
            &self.repo_data,
            issue_pattern,
            &self.config.defaults.worktree_dir,
        );
        self.work_items = items;
        self.unlinked_prs = unlinked;
        self.review_requested_prs = review_requested;

        // Start the archival clock for items that became Done through PR merge
        // (derived status) but don't yet have a done_at timestamp.
        if self.config.defaults.archive_after_days > 0 {
            match crate::side_effects::clock::system_now().duration_since(std::time::UNIX_EPOCH) {
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
            let Ok(list_result) = self.backend.list() else {
                return;
            };
            let (items, unlinked, review_requested, _) = assembly::reassemble(
                &list_result.records,
                &self.repo_data,
                issue_pattern,
                &self.config.defaults.worktree_dir,
            );
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
                        let (items, unlinked, review_requested, _) = assembly::reassemble(
                            &kept,
                            &self.repo_data,
                            pattern,
                            &self.config.defaults.worktree_dir,
                        );
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

        // Reconstruct ReviewRequest merge watches for any ReviewRequest
        // item in Review (also prunes watches whose owning item no
        // longer qualifies, e.g. after an auto-transition to Done or a
        // delete). The `--author @me` and `review-requested:@me` fetch
        // paths cannot observe a merged review-request PR, so this
        // background poll is the ONLY code path that can detect the
        // merge and advance the item to Done.
        self.reconstruct_review_request_merge_watches();
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
        // -- Phase 1: Cancel long-running background ops BEFORE
        //    destroying any backend state. The architectural rule
        //    here is "cancellation must precede destruction" - the
        //    rebase gate's background thread writes its own activity
        //    log entry, and if `backend.delete` archives the active
        //    log first there is a window where the bg thread can
        //    call `append_activity` and recreate an orphan active
        //    log for a deleted item (the failure mode described in
        //    docs/harness-contract.md C10). Routing every delete
        //    site through `abort_background_ops_for_work_item`
        //    closes that window structurally: by the time we reach
        //    `backend.delete` below, the gate has been removed from
        //    the map (so its `Drop` impl set the cancelled flag and
        //    SIGKILLed the harness group) and the bg thread will
        //    exit on its next phase check without writing.
        self.abort_background_ops_for_work_item(wi_id);

        // -- Phase 2: Backend cleanup (fatal on delete failure) --
        if let Err(e) = self.backend.pre_delete_cleanup(wi_id) {
            warnings.push(format!("pre-delete cleanup: {e}"));
        }
        self.backend.delete(wi_id)?;

        // -- Phase 3: Kill session and clean up MCP --
        self.cleanup_session_state_for(wi_id);
        if let Some(key) = self.session_key_for(wi_id)
            && let Some(mut entry) = self.sessions.remove(&key)
        {
            // Hand the written-files list back to the backend so it can
            // reverse any side-car files it wrote on spawn (the
            // `--mcp-config` tempfile, or future backend equivalents).
            // See `docs/harness-contract.md` C4 and
            // `AgentBackend::write_session_files`. The actual
            // `std::fs::remove_file` calls run on a dedicated
            // background thread via `spawn_agent_file_cleanup` -
            // doing them inline would block the UI thread on slow
            // or wedged filesystems, forbidden by `docs/UI.md`
            // "Blocking I/O Prohibition".
            self.spawn_agent_file_cleanup(std::mem::take(&mut entry.agent_written_files));
            if let Some(ref mut session) = entry.session {
                session.kill();
            }
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
            let (thread_done, captured_orphan) = self
                .user_actions
                .in_flight
                .get(&UserActionKey::WorktreeCreate)
                .map_or((true, None), |state| match &state.payload {
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
                });
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
            // `end_user_action` drops the slot and any payload it
            // owns - both `PrMergePrecheck` and `PrMerge` variants
            // store their receivers structurally inside the helper
            // entry, so no sibling clears are required.
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
        // The rebase gate was already torn down in Phase 1 via
        // `abort_background_ops_for_work_item`, BEFORE
        // `backend.delete` ran, so no second call is needed here.
        // Calling `drop_rebase_gate` again would be a no-op (the
        // map entry is gone) but the redundancy would invite future
        // confusion about the canonical cancellation point.
        if self
            .branch_gone_prompt
            .as_ref()
            .is_some_and(|(id, _)| id == wi_id)
        {
            self.branch_gone_prompt = None;
        }
        if self
            .stale_worktree_prompt
            .as_ref()
            .is_some_and(|p| p.wi_id == *wi_id)
        {
            self.clear_stale_recovery();
        }

        Ok(())
    }

    /// Keep the dialog open in progress mode and spawn a background thread to
    /// close the PR and delete the branch. The dialog shows a spinner until
    /// `poll_unlinked_cleanup()` receives the result.
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
        let Some(activity_id) = self.try_begin_user_action(
            UserActionKey::UnlinkedCleanup,
            Duration::ZERO,
            "Cleaning up unlinked PR...",
        ) else {
            self.status_message = Some("Unlinked PR cleanup already in progress".into());
            return;
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

        let reason_owned: Option<String> = reason.map(std::string::ToString::to_string);
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
        let Ok(result) = recv_result else {
            self.end_user_action(&UserActionKey::UnlinkedCleanup);
            self.cleanup_prompt_visible = false;
            self.cleanup_progress_pr_number = None;
            self.cleanup_progress_repo_path = None;
            self.cleanup_progress_branch = None;
            self.alert_message = Some("Cleanup: background thread exited unexpectedly".into());
            return;
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

                let branch_in_main_worktree = wt_for_branch.is_some_and(|wt| wt.is_main);

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
    /// cleaned up on the main thread. `poll_delete_cleanup()` receives the
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
        let Some(activity_id) = self.try_begin_user_action(
            UserActionKey::DeleteCleanup,
            Duration::ZERO,
            "Deleting work item resources...",
        ) else {
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

    /// Fire-and-forget background disposer for the side-car files the
    /// `AgentBackend` wrote on spawn (the `--mcp-config` tempfile, or
    /// any future backend's equivalent). See
    /// `docs/harness-contract.md` C4 and
    /// `AgentBackend::write_session_files`.
    ///
    /// The removal must not run on the UI thread: `std::fs::remove_file`
    /// blocks on the filesystem and a slow or wedged FS would freeze the
    /// event loop, violating `docs/UI.md` "Blocking I/O Prohibition".
    /// Called from `delete_work_item_by_id` (every delete path - modal
    /// confirm, MCP `workbridge_delete`, auto-archive), so every caller
    /// inherits the off-UI-thread guarantee without having to plumb the
    /// list through `spawn_delete_cleanup` (which is itself gated by the
    /// `DeleteCleanup` user-action single-flight and so cannot be shared
    /// by the auto-archive path). Each delete spawns at most one
    /// detached thread and file removals are idempotent, so there is no
    /// result channel - errors are swallowed by the default trait impl.
    ///
    /// `Arc<dyn AgentBackend>` is `Send + Sync` by the trait bound, so
    /// cloning it into the thread is safe.
    /// Drop an `McpSocketServer` on a background thread so its
    /// `Drop` impl (which calls `std::fs::remove_file` on the
    /// socket path) never blocks the UI thread. See `docs/UI.md`
    /// "Blocking I/O Prohibition".
    fn drop_mcp_server_off_thread(&self, server: McpSocketServer) {
        std::thread::spawn(move || {
            drop(server);
        });
    }

    fn spawn_agent_file_cleanup(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let backend = Arc::clone(&self.agent_backend);
        std::thread::spawn(move || {
            backend.cleanup_session_files(&paths);
        });
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
        let Ok(result) = recv_result else {
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

    /// Remove recently-closed PRs from cached `repo_data`. Called after
    /// `poll_unlinked_cleanup` and after `drain_fetch_results` to ensure stale
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

        let now =
            match crate::side_effects::clock::system_now().duration_since(std::time::UNIX_EPOCH) {
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

    /// Build the display list from current `work_items` and `unlinked_prs`.
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
                // Sort direct-to-you rows to the top of the block so the
                // most actionable reviews are always surfaced first. The
                // sort key is the `is_direct_request` boolean (false < true
                // but we negate below so direct comes first) and Rust's
                // `sort_by_key` is stable, so the original `gh` order is
                // preserved within each bucket. When the login is unknown
                // (fetch has not yet reported one), every row classifies
                // as team and the original order is preserved unchanged.
                let login = self.current_user_login.as_deref();
                let mut indices: Vec<usize> = (0..self.review_requested_prs.len()).collect();
                indices.sort_by_key(|&i| {
                    u8::from(!self.review_requested_prs[i].is_direct_request(login))
                });
                for i in indices {
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

        // Collect unique repos in order of first appearance. Uses the
        // shared `repo_slug_from_path` helper so the slug displayed in
        // the group header can never drift from the slug baked into a
        // work item's `display_id` (e.g. `#workbridge-42`).
        let mut repo_order: Vec<String> = Vec::new();
        let mut by_repo: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        for &i in indices {
            let repo = work_items[i].repo_associations.first().map_or_else(
                || "unknown".to_string(),
                |a| crate::work_item::repo_slug_from_path(&a.repo_path),
            );
            by_repo.entry(repo.clone()).or_default().push(i);
            if !repo_order.contains(&repo) {
                repo_order.push(repo);
            }
        }

        // Sort each repo's items in reverse-workflow order so MQ items
        // precede RV, RV precedes IM, IM precedes PL - items closest to
        // shipping appear at the top of the ACTIVE bucket. Stable sort
        // preserves the existing backend path order within a stage as the
        // tiebreaker. This is a no-op for the BLOCKED / BACKLOGGED / DONE
        // callers because every item in those buckets shares a single
        // status.
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

    /// Sync the identity trackers (`selected_work_item`, `selected_unlinked_branch`)
    /// from the current `selected_item` index. Called after any navigation that
    /// changes `selected_item` so that reassembly can restore the correct entry.
    pub(crate) fn sync_selection_identity(&mut self) {
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
    ///
    /// Sets `recenter_viewport_on_selection` on a successful move so the
    /// next render re-centers the viewport on the new selection. The
    /// flag is deliberately NOT set on the "no further selectable item"
    /// branch, since the selection and viewport both stay put.
    pub fn select_next_item(&mut self) {
        let start = self.selected_item.map_or(0, |idx| idx + 1);
        for i in start..self.display_list.len() {
            if is_selectable(&self.display_list[i]) {
                self.selected_item = Some(i);
                self.sync_selection_identity();
                self.recenter_viewport_on_selection.set(true);
                return;
            }
        }
        // If nothing found after current position, keep current selection.
    }

    /// Move selection to the previous selectable item in the display list.
    ///
    /// Sets `recenter_viewport_on_selection` on a successful move so the
    /// next render re-centers the viewport on the new selection.
    pub fn select_prev_item(&mut self) {
        let start = match self.selected_item {
            Some(idx) if idx > 0 => idx - 1,
            Some(_) => return, // at position 0, nowhere to go
            None => {
                // Nothing selected, select the last selectable item.
                if let Some(pos) = self.display_list.iter().rposition(is_selectable) {
                    self.selected_item = Some(pos);
                    self.sync_selection_identity();
                    self.recenter_viewport_on_selection.set(true);
                }
                return;
            }
        };
        for i in (0..=start).rev() {
            if is_selectable(&self.display_list[i]) {
                self.selected_item = Some(i);
                self.sync_selection_identity();
                self.recenter_viewport_on_selection.set(true);
                return;
            }
        }
        // If nothing found before current position, keep current selection.
    }

    /// Get the `WorkItemId` for the currently selected work item, if any.
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
    pub fn items_for_column(&self, status: WorkItemStatus) -> Vec<usize> {
        self.work_items
            .iter()
            .enumerate()
            .filter(|(_, wi)| {
                if status == WorkItemStatus::Implementing {
                    wi.status == WorkItemStatus::Implementing
                        || wi.status == WorkItemStatus::Blocked
                } else if status == WorkItemStatus::Review {
                    wi.status == WorkItemStatus::Review || wi.status == WorkItemStatus::Mergequeue
                } else {
                    wi.status == status
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Resolve the board cursor to a `WorkItemId`.
    pub fn board_selected_work_item_id(&self) -> Option<WorkItemId> {
        let col_status = *BOARD_COLUMNS.get(self.board_cursor.column)?;
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
                let items = self.items_for_column(*status);
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
                .copied()
                .unwrap_or(WorkItemStatus::Backlog),
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

    /// Cycle view mode: `FlatList` -> Board -> Dashboard -> `FlatList`. Also
    /// syncs cursor state when leaving Board mode so the `FlatList` cursor
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
    ///    it and drop workbridge state there, violating
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
            crate::config::canonicalize_path(&w.path)
                .is_ok_and(|existing_canonical| existing_canonical == target_canonical)
        })
    }

    /// Open or focus a session for the currently selected work item.
    ///
    /// If a session already exists for this work item, focuses the
    /// right panel. If no session exists, the caller must have already
    /// recorded a harness choice via `c` / `x` / `o`; otherwise this
    /// method pushes a hint toast and does nothing. This is the
    /// breaking behaviour-change from the v1 plan (Milestone 3): Enter
    /// no longer spawns a session without an explicit harness pick.
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

        // Breaking change from the v1 plan (Milestone 3): Enter on a
        // work-item row with no live session is now a no-op unless a
        // harness has been picked (via `c` / `x`). The hint teaches
        // the new keybinding without taking a silent-default action
        // the user did not request.
        if !self.harness_choice.contains_key(&work_item_id) {
            self.push_toast("press c / x to open this work item with a specific harness".into());
            return;
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
        if let Some(path) = wi
            .repo_associations
            .iter()
            .find_map(|a| a.worktree_path.clone())
        {
            // Worktree already exists - enqueue the background plan
            // read that feeds `finish_session_open`. The read MUST
            // live on a background thread because
            // `WorkItemBackend::read_plan` hits the filesystem
            // (see `docs/UI.md` "Blocking I/O Prohibition").
            self.begin_session_open(&work_item_id, &path);
        } else {
            // Try to find an association with a branch name and auto-create
            // a worktree for it in the background.
            // Keep only associations with a branch - and bind the
            // branch string directly, so the subsequent match arm
            // can destructure `Some((assoc, branch))` without a
            // restriction-lint `unwrap()`.
            let branch_assoc = wi
                .repo_associations
                .iter()
                .find_map(|a| a.branch.as_ref().map(|b| (a, b.clone())));
            match branch_assoc {
                Some((assoc, branch)) => {
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
                                    "Could not fetch or create branch '{branch}': {create_err}",
                                )),
                                open_session: true,
                                branch_gone: true,
                                reused: false,
                                stale_worktree_path: None,
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
                        let (wt_result, reused) = reused_wt.map_or_else(
                            || (ws.create_worktree(&repo_path, &branch, &wt_target), false),
                            |existing_wt| (Ok(existing_wt), true),
                        );
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
                                    stale_worktree_path: None,
                                });
                            }
                            Err(
                                crate::worktree_service::WorktreeError::BranchLockedToWorktree {
                                    ref locked_at,
                                    ..
                                },
                            ) => {
                                let _ = tx.send(WorktreeCreateResult {
                                    wi_id: wi_id_clone,
                                    repo_path,
                                    branch: Some(branch.clone()),
                                    path: None,
                                    error: Some(format!(
                                        "Branch '{}' is locked to a stale worktree at '{}'\n\
                                         (likely from an interrupted rebase).",
                                        branch,
                                        locked_at.display(),
                                    )),
                                    open_session: true,
                                    branch_gone: false,
                                    reused: false,
                                    stale_worktree_path: Some(locked_at.clone()),
                                });
                            }
                            Err(e) => {
                                let _ = tx.send(WorktreeCreateResult {
                                    wi_id: wi_id_clone,
                                    repo_path,
                                    branch: Some(branch.clone()),
                                    path: None,
                                    error: Some(format!(
                                        "Failed to create worktree for '{branch}': {e}",
                                    )),
                                    open_session: true,
                                    branch_gone: false,
                                    reused: false,
                                    stale_worktree_path: None,
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

    /// Begin the async preparation stage of opening an agent session.
    ///
    /// Spawns a Phase 1 background thread that performs ALL of the
    /// blocking I/O the session-open path needs (plan read, MCP socket
    /// bind, backend side-car file writes, temp `--mcp-config` file
    /// write) and then hands the result back to `poll_session_opens`,
    /// which finishes the session on the UI thread by doing pure-CPU
    /// work (system prompt + command building) and then handing the
    /// `Session::spawn` fork+exec to a Phase 2 background thread (see
    /// `poll_session_spawns`). Running any of these I/O operations
    /// on the caller (a UI-thread entry point such as `spawn_session`
    /// / `poll_worktree_creation` / `poll_review_gate`) would freeze
    /// the event loop - see `docs/UI.md` "Blocking I/O Prohibition"
    /// and `docs/harness-contract.md` C4.
    ///
    /// If another preparation is already in flight for this work item,
    /// the new request is dropped (the previous one will finish and
    /// spawn a session). This cannot deadlock: `poll_session_opens`
    /// removes the entry as soon as the result arrives.
    fn begin_session_open(&mut self, work_item_id: &WorkItemId, cwd: &std::path::Path) {
        if self.session_open_rx.contains_key(work_item_id) {
            // Phase 1 already in flight - the pending worker will
            // finish the open. Re-surface the spinner message so a
            // repeat Enter press is not silent.
            self.status_message = Some("Opening session...".into());
            return;
        }
        if self.session_spawn_rx.contains_key(work_item_id) {
            // Phase 2 PTY spawn already in flight - the pending
            // `poll_session_spawns` tick will install the session.
            self.status_message = Some("Spawning agent session...".into());
            return;
        }
        // Resolve the per-work-item harness backend for the Phase 1
        // worker BEFORE allocating channels or spawning any thread.
        // CLAUDE.md has an [ABSOLUTE] rule forbidding silent fallbacks
        // to a default harness - if the user never picked one, we
        // abort with a toast rather than letting `apply_stage_change`
        // or any other internal caller silently run Claude against
        // their code. Mirrors the `spawn_review_gate` /
        // `spawn_rebase_gate` handling.
        let Some(agent_backend) = self.backend_for_work_item(work_item_id) else {
            self.push_toast(
                "Cannot open session: no harness chosen for this work item. Press c / x to pick one first."
                    .into(),
            );
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let backend = Arc::clone(&self.backend);
        let wi_id_clone = work_item_id.clone();
        let cwd_clone = cwd.to_path_buf();

        // Commit the temp `--mcp-config` path UP FRONT on the UI
        // thread (not inside the worker) so the main thread knows
        // exactly which file the worker will create, and can route
        // it through `spawn_agent_file_cleanup` on cancellation
        // without needing to see the worker's `SessionOpenPlanResult`.
        // Per-call UUID so concurrent workers for different work
        // items cannot collide on a shared filename.
        let mcp_config_path = crate::side_effects::paths::temp_dir().join(format!(
            "workbridge-mcp-config-{}.json",
            uuid::Uuid::new_v4()
        ));

        // Shared cancellation flag. `drop_session_open_entry` sets it
        // (via `Ordering::Release`) when the user deletes the work
        // item while the worker is still in flight; the worker
        // checks it (via `Ordering::Acquire`) before each blocking
        // operation and returns early on `true`. Combined with the
        // UI-thread-committed `mcp_config_path`, this keeps the
        // tempfile-leak window bounded.
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_mcp_config_path = mcp_config_path.clone();

        // Shared running list of side-car files the worker has
        // successfully written. Populated by the worker immediately
        // after each `write_session_files` / `std::fs::write` call;
        // drained by `cancel_session_open_entry` on cancellation
        // alongside `mcp_config_path`. This closes the leak window
        // where the worker writes a side-car file then loses the
        // receiver to a cancellation race - the path would
        // otherwise vanish along with the dropped result and
        // orphan the file.
        let committed_files: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_committed_files = Arc::clone(&committed_files);

        // Precompute every MCP-setup input that requires `&self` here
        // on the UI thread. All of these are pure in-memory lookups;
        // no filesystem or subprocess calls happen in this block (the
        // docs tag is intentional - see `docs/UI.md` "Blocking I/O
        // Prohibition" for why an audit of this exact block matters).
        let socket_path = crate::mcp::socket_path_for_session();
        let wi_id_str = serde_json::to_string(work_item_id).unwrap_or_default();
        let (wi_kind, context_json, repo_mcp_servers) = {
            let wi = self.work_items.iter().find(|w| w.id == *work_item_id);
            let wi_kind = wi.map(|w| format!("{:?}", w.kind)).unwrap_or_default();
            let context_json = wi.map_or_else(
                || "{}".to_string(),
                |wi| {
                    let pr_url = wi
                        .repo_associations
                        .first()
                        .and_then(|a| a.pr.as_ref())
                        .map_or("", |pr| pr.url.as_str());
                    serde_json::json!({
                        "work_item_id": wi_id_str,
                        "stage": format!("{:?}", wi.status),
                        "title": wi.title,
                        "description": wi.description,
                        "repo": cwd_clone.display().to_string(),
                        "pr_url": pr_url,
                    })
                    .to_string()
                },
            );
            let repo_mcp_servers: Vec<crate::config::McpServerEntry> = wi
                .and_then(|w| w.repo_associations.first())
                .map(|assoc| {
                    let repo_display = crate::config::collapse_home(&assoc.repo_path);
                    self.config
                        .mcp_servers_for_repo(&repo_display)
                        .into_iter()
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            (wi_kind, context_json, repo_mcp_servers)
        };
        // R3-F-3: surface to the user that HTTP-transport MCP servers
        // are silently dropped from the Codex argv builder (Codex's
        // `mcp_servers.<name>` schema requires command + args; there
        // is no `url` sub-field). Without this toast, a user with
        // HTTP MCP entries who switched a work item to Codex would
        // silently lose those servers vs. their Claude session and
        // have no clue why a tool they expected to be available is
        // missing. Only emit for Codex sessions; the Claude argv
        // builder consumes HTTP entries via the `--mcp-config` JSON.
        // Emitted once per session-open keypress (this function is
        // gated by the `session_open_rx.contains_key` early-return
        // above, so rapid Enter presses do not fire repeated toasts).
        if agent_backend.kind() == AgentBackendKind::Codex {
            let http_skipped = repo_mcp_servers
                .iter()
                .filter(|e| e.server_type == "http")
                .count();
            if http_skipped > 0 {
                self.push_toast(format!(
                    "Codex: {http_skipped} HTTP MCP server(s) skipped (Codex requires stdio)"
                ));
            }
        }
        // `activity_path_for` is a pure in-memory path computation in
        // `LocalFileBackend` (no filesystem I/O); kept here on the UI
        // thread to avoid cloning the whole `Arc<dyn WorkItemBackend>`
        // into the worker purely for a path join.
        let activity_log_path = self.backend.activity_path_for(work_item_id);
        let mcp_tx = self.mcp_tx.clone();
        let socket_path_for_worker = socket_path;

        std::thread::spawn(move || {
            // Phase A: plan read. Must stay first so the existing
            // `begin_session_open_defers_backend_read_plan_to_background_thread`
            // regression guard continues to pass (it holds a gate
            // that parks the worker until the test releases it).
            let (plan_text, read_error) = match backend.read_plan(&wi_id_clone) {
                Ok(Some(plan)) => (plan, None),
                Ok(None) => (String::new(), None),
                Err(e) => (String::new(), Some(format!("Could not read plan: {e}"))),
            };

            // Cancellation check before any filesystem side effect.
            // If the main thread cancelled this open (work item
            // deleted, drawer closed, shutdown), bail out early
            // without starting the MCP server or writing any
            // side-car files. The `mcp_config_path` the main
            // thread committed to is cleaned up by whichever site
            // dropped the pending entry.
            if worker_cancelled.load(Ordering::Acquire) {
                return;
            }

            // Phase B: start MCP socket server. The socket bind, the
            // stale-file remove, and the accept-loop thread spawn all
            // live inside `McpSocketServer::start`; running it here
            // keeps every one of those operations off the UI thread.
            let (server, server_error) = match McpSocketServer::start(
                socket_path_for_worker.clone(),
                wi_id_str,
                wi_kind,
                context_json,
                activity_log_path,
                mcp_tx,
                false, // read_only: interactive sessions need full tool access
            ) {
                Ok(s) => (Some(s), None),
                Err(e) => (
                    None,
                    Some(format!(
                        "MCP unavailable: failed to start socket server: {e}"
                    )),
                ),
            };

            // Phase C: write the backend-specific side-car files and
            // the temp `--mcp-config` file. Both are `std::fs::write`
            // calls that block on the worktree / tmpfs filesystem and
            // so must NEVER run on the UI thread. Only executed when
            // the server came up AND the open has not been
            // cancelled; otherwise there is no socket to wire the
            // agent CLI up to and the spawn proceeds in degraded
            // mode with `mcp_config_path: None`. The cancellation
            // check here is a best-effort race window reduction:
            // the main thread's cleanup still owns `mcp_config_path`
            // even if the flag flip happens after this load.
            let mut written_files: Vec<PathBuf> = Vec::new();
            let mut mcp_config_path_out: Option<PathBuf> = None;
            let mut mcp_bridge_out: Option<crate::agent_backend::McpBridgeSpec> = None;
            // Convert each per-repo `McpServerEntry` into an
            // `McpBridgeSpec` so Codex can emit one `-c
            // mcp_servers.<name>.*` pair per entry. Skip HTTP-transport
            // entries: Codex's `mcp_servers.<name>` schema requires
            // command + args (no `url` sub-field), so an HTTP entry
            // would produce a malformed override. Claude still sees
            // HTTP entries via the JSON written into `mcp_config_path`.
            // Skip stdio entries with no `command` (defensive against
            // hand-edited config); they cannot spawn anything.
            let extra_mcp_bridges: Vec<crate::agent_backend::McpBridgeSpec> = repo_mcp_servers
                .iter()
                .filter(|entry| entry.server_type != "http")
                .filter_map(|entry| {
                    entry
                        .command
                        .as_ref()
                        .map(|cmd| crate::agent_backend::McpBridgeSpec {
                            name: entry.name.clone(),
                            command: PathBuf::from(cmd),
                            args: entry.args.clone(),
                        })
                })
                .collect();
            let mut mcp_config_error: Option<String> = None;
            if let Some(ref server) = server
                && !worker_cancelled.load(Ordering::Acquire)
            {
                match std::env::current_exe() {
                    Ok(exe) => {
                        let mcp_config = crate::mcp::build_mcp_config(
                            &exe,
                            &server.socket_path,
                            &repo_mcp_servers,
                        );
                        // Capture the structured bridge spec so Codex
                        // (and any future harness that uses per-field
                        // `-c` MCP overrides) can register the server
                        // without having to parse `mcp_config` back out
                        // of the JSON on disk. Mirrors what
                        // `crate::mcp::build_mcp_config` writes into
                        // the `workbridge` key of the JSON.
                        mcp_bridge_out = Some(crate::agent_backend::McpBridgeSpec {
                            name: "workbridge".to_string(),
                            command: exe,
                            args: vec![
                                "--mcp-bridge".to_string(),
                                "--socket".to_string(),
                                server.socket_path.to_string_lossy().into_owned(),
                            ],
                        });

                        // Backend side-car files (future backends
                        // may write temp config files here). Push
                        // each successfully-written path into the
                        // shared `worker_committed_files` list under
                        // the mutex BEFORE continuing, so a
                        // cancellation that arrives between the
                        // write and the eventual `tx.send(...)`
                        // can still find the path and clean it up
                        // via `cancel_session_open_entry`. Without
                        // this push, a cancelled work item would
                        // orphan the side-car file.
                        match agent_backend.write_session_files(&cwd_clone, &mcp_config) {
                            Ok(paths) => {
                                if !paths.is_empty()
                                    && let Ok(mut guard) = worker_committed_files.lock()
                                {
                                    guard.extend(paths.iter().cloned());
                                }
                                written_files.extend(paths);
                            }
                            Err(e) => {
                                mcp_config_error = Some(format!("MCP config write error: {e}"));
                            }
                        }

                        // Primary MCP wire-up: write to the
                        // `mcp_config_path` the UI thread committed
                        // to. The path flows back into the backend
                        // via `SpawnConfig::mcp_config_path`. Re-check
                        // the cancellation flag right before the
                        // write so a rapid user cancel can still
                        // skip the write in the common case.
                        if !worker_cancelled.load(Ordering::Acquire) {
                            match std::fs::write(&worker_mcp_config_path, &mcp_config) {
                                Ok(()) => {
                                    written_files.push(worker_mcp_config_path.clone());
                                    mcp_config_path_out = Some(worker_mcp_config_path.clone());
                                }
                                Err(e) => {
                                    if mcp_config_error.is_none() {
                                        mcp_config_error =
                                            Some(format!("MCP config write error: {e}"));
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        mcp_config_error = Some(format!("Cannot resolve executable path: {e}"));
                    }
                }
            }

            let result = SessionOpenPlanResult {
                wi_id: wi_id_clone,
                cwd: cwd_clone,
                plan_text,
                read_error,
                server,
                server_error,
                written_files,
                mcp_config_path: mcp_config_path_out,
                mcp_bridge: mcp_bridge_out,
                extra_mcp_bridges,
                mcp_config_error,
            };
            if let Err(crossbeam_channel::SendError(result)) = tx.send(result) {
                // Receiver was dropped (work item deleted or app
                // shutting down). The main thread's cancellation
                // cleanup may have run before we wrote the config,
                // so the file might still be on disk. Clean up
                // directly since we're already on a background
                // thread.
                for path in &result.written_files {
                    let _ = std::fs::remove_file(path);
                }
                if let Some(path) = &result.mcp_config_path {
                    let _ = std::fs::remove_file(path);
                }
                // MCP server Drop runs here (background thread).
            }
        });
        // Surface immediate feedback so a slow background phase does
        // not make the TUI look hung between the Enter keypress and
        // the next `poll_session_opens` tick (200ms). The spinner is
        // ended in `poll_session_opens` for every terminal arm
        // (success, read_error, disconnect) via `drop_session_open_entry`.
        let activity = self.start_activity("Opening session...");
        self.session_open_rx.insert(
            work_item_id.clone(),
            SessionOpenPending {
                rx,
                activity,
                cancelled,
                mcp_config_path,
                committed_files,
            },
        );
    }

    /// Remove a pending `session_open_rx` entry and end its spinner
    /// activity after the worker has successfully delivered its
    /// result. Does NOT set the cancellation flag and does NOT
    /// schedule any file cleanup - the worker already wrote the
    /// tempfile and the main thread is about to hand it to
    /// `finish_session_open` which moves it into
    /// `SessionEntry::agent_written_files`. Use
    /// `cancel_session_open_entry` for the abort paths.
    fn drop_session_open_entry(&mut self, wi_id: &WorkItemId) {
        if let Some(entry) = self.session_open_rx.remove(wi_id) {
            self.end_activity(entry.activity);
        }
    }

    /// Cancel a pending `session_open_rx` entry: signal the worker to
    /// skip any remaining file writes (via the shared
    /// `cancelled: Arc<AtomicBool>`), route the UI-thread-committed
    /// `mcp_config_path` AND any side-car files the worker has
    /// already written (via the shared `committed_files` mutex)
    /// through `spawn_agent_file_cleanup` so the tempfile and any
    /// side-car files are not leaked, and end the
    /// spinner activity. Called from every abort path
    /// (`cleanup_session_state_for`, a dead-worker arm in
    /// `poll_session_opens`, the stage-transition respawn path, and
    /// `cleanup_all_mcp` at shutdown).
    ///
    /// There is still a sub-microsecond race window where the worker
    /// loads `cancelled == false`, the main thread sets
    /// `cancelled = true`, and the worker then writes the file
    /// anyway. For the temp `--mcp-config` file (path known to the
    /// main thread up front) and for any side-car file the worker
    /// has already pushed into `committed_files` under the mutex,
    /// the scheduled `spawn_agent_file_cleanup` removes them. The
    /// only residual leak is a side-car write that races: worker
    /// returns `Ok(paths)` from `write_session_files` AFTER the
    /// main thread has already drained `committed_files` here.
    /// That window is bounded by the time between
    /// `agent_backend.write_session_files(...)` returning and the
    /// `worker_committed_files.lock().unwrap().extend(...)` push -
    /// nanoseconds in normal conditions. The OS tmp cleaner reaps
    /// orphaned entries; in the work-item case the worktree itself
    /// is usually about to be removed by `spawn_delete_cleanup`
    /// which sweeps the entire directory.
    fn cancel_session_open_entry(&mut self, wi_id: &WorkItemId) {
        if let Some(entry) = self.session_open_rx.remove(wi_id) {
            entry.cancelled.store(true, Ordering::Release);
            // If the worker already sent a result before we set
            // cancelled, drain it so the MCP server's Drop (which
            // calls std::fs::remove_file) does not run on the UI
            // thread when the receiver is dropped.
            if let Ok(result) = entry.rx.try_recv()
                && let Some(server) = result.server
            {
                self.drop_mcp_server_off_thread(server);
            }
            // Drain any side-car files the worker has already
            // committed to disk. The lock is held briefly inside
            // `Mutex::lock().unwrap()` - effectively wait-free
            // unless the worker is mid-push.
            let mut files_to_clean: Vec<PathBuf> = Vec::new();
            if let Ok(mut guard) = entry.committed_files.lock() {
                files_to_clean.extend(guard.drain(..));
            }
            files_to_clean.push(entry.mcp_config_path);
            self.spawn_agent_file_cleanup(files_to_clean);
            self.end_activity(entry.activity);
        }
    }

    /// Poll Phase 1 session-open preparation workers. Called from the
    /// background-work tick in `salsa.rs`. Each completed receiver
    /// hands a fully-prepared `SessionOpenPlanResult` (plan text, MCP
    /// server handle, written side-car files, temp config path) to
    /// `finish_session_open`, which does pure-CPU work (system prompt
    /// and command building) then hands the `Session::spawn` fork+exec
    /// to a Phase 2 background thread. No filesystem I/O or subprocess
    /// spawns happen here.
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
                        // Background thread died without sending - the
                        // worker may have written its `--mcp-config`
                        // and side-car files before panicking, so
                        // route the committed tempfile path through
                        // `spawn_agent_file_cleanup` via
                        // `cancel_session_open_entry` and end the
                        // spinner so a retry is possible.
                        self.cancel_session_open_entry(&wi_id);
                        self.status_message =
                            Some("Session open: background thread exited unexpectedly".into());
                        continue;
                    }
                },
                None => continue,
            };
            self.drop_session_open_entry(&wi_id);
            // Surface every non-fatal error the worker reported. None
            // of these abort the spawn - they flow into the status bar
            // alongside the session. Order matters: the worker
            // populates at most one of the three slots per failure
            // class, and the last non-empty message wins in the bar.
            if let Some(msg) = result.read_error.clone() {
                self.status_message = Some(msg);
            }
            if let Some(msg) = result.server_error.clone() {
                self.status_message = Some(msg);
            }
            if let Some(msg) = result.mcp_config_error.clone() {
                self.status_message = Some(msg);
            }
            self.finish_session_open(result);
        }
    }

    /// Finish the session-open flow after the background worker has
    /// completed every blocking step (plan read, MCP socket bind,
    /// side-car writes, temp config write).
    ///
    /// Called only from `poll_session_opens`. MUST NOT be called from
    /// any UI-thread entry point that has not first gone through the
    /// background worker: this function calls `stage_system_prompt`
    /// which consumes `rework_reasons` / `review_gate_findings` state,
    /// so calling it twice for the same work item would discard user
    /// state. It is also explicitly free of filesystem I/O and
    /// subprocess spawns - every `std::fs::*` call lives in the
    /// Phase 1 worker in `begin_session_open`, and the `Session::spawn`
    /// fork+exec is handed off to a Phase 2 background thread whose
    /// result is drained by `poll_session_spawns`.
    fn finish_session_open(&mut self, result: SessionOpenPlanResult) {
        let SessionOpenPlanResult {
            wi_id,
            cwd,
            plan_text,
            server: mcp_server,
            written_files,
            mcp_config_path,
            mcp_bridge,
            extra_mcp_bridges,
            // The callers of `finish_session_open` surface these
            // three to the status bar before this function runs, so
            // we deliberately do not re-read them here.
            read_error: _,
            server_error: _,
            mcp_config_error: _,
        } = result;
        let work_item_id = &wi_id;
        let cwd = cwd.as_path();

        // Guard: the work item may have been deleted while the
        // background worker was in flight. In that case, do not spawn
        // a session. The server (if any) is dropped on a background
        // thread so its `std::fs::remove_file` does not block the UI;
        // the side-car files are handed to `spawn_agent_file_cleanup`
        // for the same reason.
        let Some(work_item_status) = self
            .work_items
            .iter()
            .find(|w| w.id == *work_item_id)
            .map(|w| w.status)
        else {
            if let Some(server) = mcp_server {
                self.drop_mcp_server_off_thread(server);
            }
            self.spawn_agent_file_cleanup(written_files);
            return;
        };

        let session_key = (work_item_id.clone(), work_item_status);
        let has_gate_findings = self.review_gate_findings.contains_key(work_item_id);
        let system_prompt = self.stage_system_prompt(work_item_id, cwd, plan_text);

        // Resolve the per-work-item harness choice. CLAUDE.md has an
        // [ABSOLUTE] rule: silent fallbacks to a default harness are
        // P0. If `harness_choice` has no entry for this work item, we
        // MUST abort the spawn with a user-visible toast rather than
        // silently running Claude (or any other hidden default). This
        // is symmetrical with how `spawn_review_gate` and
        // `spawn_rebase_gate` handle the same case. The callers
        // (`open_session_for_selected`, `apply_stage_change`) already
        // guard against the common path, but the guard here is
        // defence-in-depth for any future entry point that calls
        // `spawn_session` -> `begin_session_open` without a recorded
        // harness choice.
        let Some(wi_backend) = self.backend_for_work_item(work_item_id) else {
            // Clean up the MCP server and side-car files the worker
            // prepared; the session will not be spawned.
            if let Some(server) = mcp_server {
                self.drop_mcp_server_off_thread(server);
            }
            self.spawn_agent_file_cleanup(written_files);
            self.push_toast(
                "Cannot open session: no harness chosen for this work item. Press c / x to pick one first."
                    .into(),
            );
            return;
        };
        let cmd = self.build_agent_cmd_with(
            wi_backend.as_ref(),
            work_item_status,
            system_prompt.as_deref(),
            McpInjection {
                config_path: mcp_config_path.as_deref(),
                primary_bridge: mcp_bridge.as_ref(),
                extra_bridges: &extra_mcp_bridges,
            },
            has_gate_findings,
        );

        // Phase 2: hand the fork+exec off to a background thread so
        // `Session::spawn` never runs on the event loop. The result
        // flows back through `session_spawn_rx` and is drained by
        // `poll_session_spawns` on the next timer tick.
        let (tx, rx) = crossbeam_channel::bounded::<SessionSpawnResult>(1);
        let pane_cols = self.pane_cols;
        let pane_rows = self.pane_rows;
        let cwd_owned = cwd.to_path_buf();
        let wi_id_clone = work_item_id.clone();
        let session_key_clone = session_key;
        std::thread::spawn(move || {
            let cmd_refs: Vec<&str> = cmd.iter().map(std::string::String::as_str).collect();
            let result = match Session::spawn(pane_cols, pane_rows, Some(&cwd_owned), &cmd_refs) {
                Ok(session) => SessionSpawnResult {
                    wi_id: wi_id_clone,
                    session_key: session_key_clone,
                    session: Some(session),
                    error: None,
                    mcp_server,
                    written_files,
                },
                Err(e) => SessionSpawnResult {
                    wi_id: wi_id_clone,
                    session_key: session_key_clone,
                    session: None,
                    error: Some(format!("Error spawning session: {e}")),
                    mcp_server,
                    written_files,
                },
            };
            if let Err(crossbeam_channel::SendError(result)) = tx.send(result) {
                // Receiver was dropped (work item deleted or app
                // shutting down while spawn was in flight). Session
                // and MCP server Drops run here (background thread,
                // so no UI-thread I/O). Clean up side-car files
                // directly since we cannot reach
                // `spawn_agent_file_cleanup` from here.
                for path in &result.written_files {
                    let _ = std::fs::remove_file(path);
                }
                // `result.session` and `result.mcp_server` drop
                // here, killing the child and unlinking the socket.
            }
        });

        let activity = self.start_activity("Spawning agent session...");
        self.session_spawn_rx
            .insert(work_item_id.clone(), SessionSpawnPending { rx, activity });
    }

    /// Drain Phase 2 PTY spawn results. Called on each timer tick.
    /// Installs the `Session` into `self.sessions` on success, or
    /// cleans up MCP resources on failure. Symmetric with
    /// `poll_session_opens` (Phase 1) and `poll_global_session_open`.
    pub fn poll_session_spawns(&mut self) {
        if self.session_spawn_rx.is_empty() {
            return;
        }
        let keys: Vec<WorkItemId> = self.session_spawn_rx.keys().cloned().collect();
        for wi_id in keys {
            let Some(pending) = self.session_spawn_rx.get(&wi_id) else {
                continue;
            };
            let result = match pending.rx.try_recv() {
                Ok(r) => r,
                Err(crossbeam_channel::TryRecvError::Empty) => continue,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // Worker thread died without sending.
                    if let Some(pending) = self.session_spawn_rx.remove(&wi_id) {
                        self.end_activity(pending.activity);
                    }
                    self.status_message =
                        Some("Session spawn: background thread exited unexpectedly".into());
                    continue;
                }
            };
            if let Some(pending) = self.session_spawn_rx.remove(&wi_id) {
                self.end_activity(pending.activity);
            }

            // Guard: the work item may have been deleted or
            // transitioned to another stage while the Phase 2
            // worker was in flight. If the owning work item no
            // longer exists or its status no longer matches the
            // session key, drop the session and clean up.
            let item_valid = self
                .work_items
                .iter()
                .find(|w| w.id == result.wi_id)
                .is_some_and(|w| w.status == result.session_key.1);

            match (result.session, result.error) {
                (Some(session), _) if item_valid => {
                    let parser = Arc::clone(&session.parser);
                    let entry = SessionEntry {
                        parser,
                        alive: true,
                        session: Some(session),
                        scrollback_offset: 0,
                        selection: None,
                        // Hand the written-files list to the session so
                        // its death path can call
                        // `cleanup_session_files` on the backend, per
                        // `docs/harness-contract.md` C4.
                        agent_written_files: result.written_files,
                    };
                    self.sessions.insert(result.session_key, entry);
                    if let Some(server) = result.mcp_server {
                        self.mcp_servers.insert(result.wi_id.clone(), server);
                    }
                    self.focus = FocusPanel::Right;
                    self.status_message =
                        Some("Right panel focused - press Ctrl+] to return".into());
                }
                (Some(session), _) => {
                    // Work item was deleted or stage changed while
                    // the spawn was in flight. Drop both the session
                    // and MCP server off the UI thread: Session::Drop
                    // kills/joins the child, McpSocketServer::Drop
                    // unlinks the socket.
                    std::thread::spawn(move || drop(session));
                    if let Some(server) = result.mcp_server {
                        self.drop_mcp_server_off_thread(server);
                    }
                    self.spawn_agent_file_cleanup(result.written_files);
                }
                (None, Some(e)) => {
                    // Session spawn failed. Drop the MCP server off
                    // the UI thread and clean up side-car files.
                    if let Some(server) = result.mcp_server {
                        self.drop_mcp_server_off_thread(server);
                    }
                    self.spawn_agent_file_cleanup(result.written_files);
                    self.status_message = Some(e);
                }
                (None, None) => {
                    // Should not happen, but handle gracefully.
                    if let Some(server) = result.mcp_server {
                        self.drop_mcp_server_off_thread(server);
                    }
                    self.spawn_agent_file_cleanup(result.written_files);
                    self.status_message =
                        Some("Session spawn returned no session and no error".into());
                }
            }
        }
    }

    /// Neutral placeholder shown in the right-panel tab title when no
    /// harness has been committed to the current context (no selected
    /// work item, or a selected item with no `harness_choice` and no
    /// live session). Rendering a vendor name ("Claude Code", "Codex")
    /// in this state would lie: the pane contains no session, so no
    /// specific harness is running. The placeholder is exported so
    /// snapshot tests and docs can reference the single canonical
    /// string instead of duplicating it.
    pub const SESSION_TITLE_NONE: &'static str = "Session";

    /// Human-readable name of the agent backend actually driving the
    /// current context's session. Used for the right-panel tab title,
    /// the dead-session placeholder, and any other UI text that names
    /// which LLM CLI is running. Centralised here so a new backend is
    /// a one-line addition. See `docs/harness-contract.md` glossary
    /// and `docs/UI.md` "Session tab title".
    ///
    /// **Architectural principle** (CLAUDE.md `[ABSOLUTE]` "session
    /// title is downstream of live harness state"): this function is
    /// forbidden from falling back to a hardcoded vendor default. If
    /// no harness is committed for the current context, it returns
    /// the neutral `SESSION_TITLE_NONE` placeholder. Returning
    /// `self.agent_backend.kind().display_name()` as a fallback would
    /// mean the tab title reads "Claude Code" for a user who has
    /// picked Codex but not yet spawned the session - a user-facing
    /// lie because no harness is running in the pane at all.
    ///
    /// Resolution order:
    /// 1. Per-work-item `harness_choice` for the currently selected
    ///    work item: this is the harness actually driving (or about
    ///    to drive) that item's session, and is set only after the
    ///    user explicitly pressed `c` / `x`.
    /// 2. Global-assistant harness if the Ctrl+G drawer is open and
    ///    the user has configured one.
    /// 3. `SESSION_TITLE_NONE` placeholder - never a vendor default.
    pub fn agent_backend_display_name(&self) -> &'static str {
        self.resolved_harness_kind()
            .map_or(Self::SESSION_TITLE_NONE, |kind| kind.display_name())
    }

    /// Single source of truth for the Session tab title's harness
    /// resolution. Both `agent_backend_display_name` and
    /// `agent_backend_display_name_with_permission_marker` delegate
    /// here so the name-vs-marker branches can never diverge (a
    /// previous divergence-class bug silently dropped the Codex
    /// `" [!]"` marker when a work item was selected with no
    /// `harness_choice` entry and the Ctrl+G drawer was open with
    /// global=Codex).
    ///
    /// Resolution is fall-through, matching the name path:
    /// 1. Per-work-item `harness_choice` for the selected item, if
    ///    such an entry exists. A selected item with no entry does
    ///    NOT short-circuit to `None` - it falls through.
    /// 2. Global-assistant harness when the Ctrl+G drawer is open.
    /// 3. `None` (caller renders the neutral placeholder / unmarked).
    fn resolved_harness_kind(&self) -> Option<AgentBackendKind> {
        if let Some(id) = self.selected_work_item_id()
            && let Some(kind) = self.harness_choice.get(&id)
        {
            return Some(*kind);
        }
        if self.global_drawer_open {
            return self.global_assistant_harness_kind();
        }
        None
    }

    /// Suffix appended to a Codex session's display name in the
    /// right-panel tab title (and anywhere else the per-harness
    /// permission marker is rendered). Single typable characters only
    /// (global rule: no fancy unicode). The marker is a visible
    /// reminder that Codex runs without its built-in sandbox - see
    /// README "Per-harness permission model".
    pub const PERMISSION_MARKER_CODEX: &'static str = " [!]";

    /// Like `agent_backend_display_name`, but appends a visible
    /// permission marker (` [!]`) when the resolved harness is Codex.
    /// Call sites that render the harness name in UI chrome (right-
    /// panel tab title, dead-session placeholder, Ctrl+\\ switch-back
    /// hint) use this function; the marker signals to the user that
    /// Codex runs without its built-in sandbox on every spawn path.
    ///
    /// The neutral `SESSION_TITLE_NONE` placeholder renders unmarked
    /// (no harness is committed, so no permission model applies yet);
    /// Claude Code also renders unmarked. This matches the
    /// `[ABSOLUTE]` "session title is downstream of live harness
    /// state" rule: the marker appears only when a harness is
    /// actually resolved AND that harness is Codex.
    ///
    /// The underlying `agent_backend_display_name` stays for snapshot
    /// / contract tests that pin the canonical vendor name.
    pub fn agent_backend_display_name_with_permission_marker(
        &self,
    ) -> std::borrow::Cow<'static, str> {
        // Delegate resolution to the shared helper so the name and
        // the marker can never diverge. The previous separate
        // `if/else if/else` chain here silently dropped the marker
        // when a work item was selected with no `harness_choice`
        // entry and the drawer was open with global=Codex: the name
        // correctly fell through to "Codex" but the marker
        // resolution bailed at the `if let Some(id)` arm and
        // returned None.
        let name = self.agent_backend_display_name();
        if matches!(self.resolved_harness_kind(), Some(AgentBackendKind::Codex)) {
            std::borrow::Cow::Owned(format!("{name}{}", Self::PERMISSION_MARKER_CODEX))
        } else {
            std::borrow::Cow::Borrowed(name)
        }
    }

    /// Resolve the harness-specific backend for a work-item spawn.
    /// Returns `Some` only if the user has already pressed `c` / `x` /
    /// `o` for this item (i.e. there is a `harness_choice` entry). The
    /// spawn sites surface the `None` case as a toast and bail rather
    /// than silently defaulting to `self.agent_backend` - that was the
    /// "abort rather than default to claude" rule pinned by the plan
    /// (Milestone 3, review/rebase-gate bullet). See also
    /// `docs/harness-contract.md` Change Log 2026-04-16.
    pub fn backend_for_work_item(
        &self,
        work_item_id: &WorkItemId,
    ) -> Option<Arc<dyn AgentBackend>> {
        let kind = self.harness_choice.get(work_item_id).copied()?;
        Some(agent_backend::backend_for_kind(kind))
    }

    /// Record the user's per-work-item harness choice and open the
    /// session using it. Called from the `c` / `x` keybindings (the
    /// `o` key is reserved for "open PR in browser" and does not
    /// route here).
    /// Performs a lazy availability check first (via
    /// `agent_backend::is_available`); missing-binary shows a toast
    /// and does not overwrite an existing choice. If a live session
    /// already exists for this item, shows a "press kk to end first"
    /// toast and returns - the user must terminate before respawning.
    pub fn open_session_with_harness(&mut self, kind: AgentBackendKind) {
        // PATH availability check before recording the choice. A failed
        // press must NOT silently clobber a valid previous selection.
        if !agent_backend::is_available(kind) {
            self.push_toast(format!("{}: command not found", kind.binary_name()));
            return;
        }

        let Some(work_item_id) = self.selected_work_item_id() else {
            return;
        };

        // If a live session already exists, we must refuse to spawn.
        // The user loses scrollback and activity state otherwise.
        if let Some(existing_key) = self.session_key_for(&work_item_id) {
            let is_alive = self
                .sessions
                .get(&existing_key)
                .is_some_and(|entry| entry.alive);
            if is_alive {
                self.push_toast("session already running - press kk to end first".into());
                return;
            }
        }

        // Record the choice BEFORE any stage transition so the downstream
        // spawn in `apply_stage_change` -> `spawn_session` has the harness
        // available when it calls `backend_for_work_item`.
        self.harness_choice.insert(work_item_id.clone(), kind);

        // Auto-advance Backlog -> Planning so `c`/`x` is a single-keypress
        // "begin work on this item" action. Without this, pressing c/x
        // on a Backlog row silently records the harness but spawns no
        // session (spawn_session early-returns for Backlog), leaving the
        // user staring at an unchanged row. The UI hint on a Backlog row
        // already advertises c/x as the begin-planning action.
        let current_status = self
            .work_items
            .iter()
            .find(|w| w.id == work_item_id)
            .map(|w| w.status);
        if current_status == Some(WorkItemStatus::Backlog) {
            self.apply_stage_change(
                &work_item_id,
                WorkItemStatus::Backlog,
                WorkItemStatus::Planning,
                "user_harness_pick",
            );
            // apply_stage_change already calls spawn_session for stages
            // with prompts (Planning qualifies), so no further action is
            // needed - the session is now spawning.
            return;
        }

        // Non-Backlog path: delegate to the existing session-open flow.
        // `finish_session_open` reads back the choice via
        // `backend_for_work_item`.
        self.open_session_for_selected();
    }

    /// Handle a `k` keypress on a work-item row. First press within the
    /// window arms a toast hint; a second press within ~1.5s SIGTERMs
    /// the session (by dropping the `SessionEntry`, which triggers the
    /// `Drop for Session` path - SIGTERM, then SIGKILL after 50ms -
    /// per C10). Press outside the window on a different item resets.
    pub fn handle_k_press(&mut self) {
        const WINDOW: Duration = Duration::from_millis(1500);
        let Some(work_item_id) = self.selected_work_item_id() else {
            return;
        };
        // Only react if a live session exists. `k` is otherwise unused
        // in this context and an arming toast would be confusing.
        let has_live_session = self
            .session_key_for(&work_item_id)
            .and_then(|k| self.sessions.get(&k))
            .is_some_and(|entry| entry.alive);
        if !has_live_session {
            return;
        }

        let now = crate::side_effects::clock::instant_now();
        let armed = matches!(
            self.last_k_press.as_ref(),
            Some((id, t)) if id == &work_item_id
                && now.saturating_duration_since(*t) < WINDOW
        );

        if armed {
            // Second press within the window - kill.
            if let Some(key) = self.session_key_for(&work_item_id) {
                self.sessions.remove(&key);
            }
            // Note: harness_choice is NOT cleared here. A subsequent
            // c/x overwrites it, and keeping the last choice around
            // is harmless. See the Milestone 3 acceptance-criteria
            // notes.
            self.last_k_press = None;
            self.push_toast("session ended".into());
        } else {
            self.last_k_press = Some((work_item_id, now));
            self.push_toast("press k again within 1.5s to end session".into());
        }
    }

    /// Clear an expired `last_k_press` entry. Called from the per-tick
    /// hook so the hint clears after ~1.5s even if the user walks
    /// away without pressing any other key.
    pub fn prune_k_press(&mut self) {
        const WINDOW: Duration = Duration::from_millis(1500);
        if let Some((_, t)) = &self.last_k_press
            && crate::side_effects::clock::instant_now().saturating_duration_since(*t) >= WINDOW
        {
            self.last_k_press = None;
        }
    }

    /// Clear the `last_k_press` flag. Called from `handle_key` on any
    /// key that isn't `k` so the double-press window dies on unrelated
    /// keystrokes rather than arming two sessions apart in time.
    pub fn clear_k_press(&mut self) {
        self.last_k_press = None;
    }

    /// Resolve the harness kind for the Ctrl+G global assistant.
    /// Returns the configured kind if one is set, otherwise `None`
    /// to signal "show the first-run modal".
    pub fn global_assistant_harness_kind(&self) -> Option<AgentBackendKind> {
        let name = self.config.defaults.global_assistant_harness.as_deref()?;
        AgentBackendKind::from_str(name).ok()
    }

    /// Handle a Ctrl+G keypress. If the config already has a chosen
    /// harness, toggle the drawer as before. Otherwise open the
    /// first-run modal that lists harnesses on PATH. If no harness is
    /// on PATH, show a toast and do nothing.
    pub fn handle_ctrl_g(&mut self) {
        // Fast path: harness already configured.
        if self.global_assistant_harness_kind().is_some() {
            self.toggle_global_drawer();
            return;
        }

        let available: Vec<AgentBackendKind> = AgentBackendKind::all()
            .into_iter()
            .filter(|k| agent_backend::is_available(*k))
            .collect();

        if available.is_empty() {
            self.push_toast(
                "no supported harnesses on PATH - install claude or codex to use Ctrl+G".into(),
            );
            return;
        }

        self.first_run_global_harness_modal = Some(FirstRunGlobalHarnessModal {
            available_harnesses: available,
        });
    }

    /// Finish the first-run modal: persist the pick to config and
    /// open the drawer immediately. Called from the modal's key
    /// handler in `event.rs` when the user presses one of the
    /// harness keybindings inside the modal.
    pub fn finish_first_run_global_pick(&mut self, kind: AgentBackendKind) {
        self.first_run_global_harness_modal = None;
        self.config.defaults.global_assistant_harness = Some(kind.canonical_name().into());
        // Persist via the configured provider. The helper swallows
        // errors as toasts so a read-only config dir does not take
        // down the UI; the in-memory value still reflects the pick
        // for this TUI session.
        if let Err(e) = self.config_provider.save(&self.config) {
            self.push_toast(format!("could not save config: {e}"));
        }
        self.toggle_global_drawer();
    }

    /// Dismiss the first-run modal without a pick. Config stays at its
    /// previous (None) state; the drawer does not open.
    pub fn cancel_first_run_global_pick(&mut self) {
        self.first_run_global_harness_modal = None;
    }

    /// Test-only thin wrapper over `build_agent_cmd_with(self.agent_backend, ...)`.
    /// Exists so legacy tests can assert argv-shape without stitching
    /// a per-work-item backend; new production call sites use
    /// `build_agent_cmd_with` directly so the per-work-item harness
    /// choice is honored.
    #[cfg(test)]
    fn build_agent_cmd(
        &self,
        status: WorkItemStatus,
        system_prompt: Option<&str>,
        mcp_config_path: Option<&std::path::Path>,
        force_auto_start: bool,
    ) -> Vec<String> {
        self.build_agent_cmd_with(
            self.agent_backend.as_ref(),
            status,
            system_prompt,
            McpInjection {
                config_path: mcp_config_path,
                primary_bridge: None,
                extra_bridges: &[],
            },
            force_auto_start,
        )
    }

    /// Build the argv using a specific backend. Thin wrapper around
    /// `backend.build_command` that also computes the C7 auto-start
    /// message from the stage and the gate-findings flag. Called from
    /// `finish_session_open` so the per-work-item harness choice
    /// (recorded in `App::harness_choice`) is honored.
    fn build_agent_cmd_with(
        &self,
        backend: &dyn AgentBackend,
        status: WorkItemStatus,
        system_prompt: Option<&str>,
        mcp: McpInjection<'_>,
        force_auto_start: bool,
    ) -> Vec<String> {
        let auto_start_message = self.auto_start_message_for_stage(status, force_auto_start);
        let cfg = SpawnConfig {
            stage: status,
            system_prompt,
            mcp_config_path: mcp.config_path,
            mcp_bridge: mcp.primary_bridge,
            extra_bridges: mcp.extra_bridges,
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: auto_start_message.as_deref(),
            read_only: false,
        };
        backend.build_command(&cfg)
    }

    /// Resolve the C7 auto-start user message for a given stage.
    ///
    /// Returns `None` for stages that do not auto-start (Blocked, and
    /// Review without pending gate findings). The actual phrasing lives
    /// in `prompts/stage_prompts.json` under the `auto_start_default`
    /// and `auto_start_review` keys so it can be edited without
    /// recompiling.
    fn auto_start_message_for_stage(
        &self,
        status: WorkItemStatus,
        force_auto_start: bool,
    ) -> Option<String> {
        let auto_start = force_auto_start
            || matches!(
                status,
                WorkItemStatus::Planning | WorkItemStatus::Implementing
            );
        if !auto_start {
            return None;
        }
        let vars = std::collections::HashMap::new();
        let key = if status == WorkItemStatus::Review {
            "auto_start_review"
        } else {
            "auto_start_default"
        };
        crate::prompts::render(key, &vars)
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
        let Ok(result) = recv_result else {
            self.end_user_action(&UserActionKey::WorktreeCreate);
            self.status_message =
                Some("Worktree creation: background thread exited unexpectedly".into());
            return;
        };

        self.end_user_action(&UserActionKey::WorktreeCreate);

        // If this result came from a stale-worktree recovery, clear the
        // recovery modal. On success the prompt is dismissed; on failure
        // the error arm below will re-display the appropriate alert.
        if self.stale_recovery_in_progress {
            self.clear_stale_recovery();
        }

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
                } else if let Some(stale_path) = result.stale_worktree_path {
                    // Branch is locked to a stale worktree. Show
                    // recovery dialog instead of a generic alert.
                    self.stale_worktree_prompt = Some(StaleWorktreePrompt {
                        wi_id: result.wi_id.clone(),
                        error,
                        stale_path,
                        repo_path: result.repo_path.clone(),
                        branch: result.branch.clone().unwrap_or_default(),
                        open_session: result.open_session,
                    });
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

    /// Clear both stale-worktree recovery fields atomically. These two
    /// fields must always be cleared together; using this helper instead
    /// of setting them individually prevents a future cleanup site from
    /// clearing one but not the other (which would leave a stuck spinner).
    fn clear_stale_recovery(&mut self) {
        self.stale_worktree_prompt = None;
        self.stale_recovery_in_progress = false;
    }

    /// Spawn a background thread that force-removes a stale worktree,
    /// prunes git's worktree bookkeeping, and retries worktree creation.
    /// Called from the stale-worktree recovery dialog when the user
    /// presses [r]. The dialog switches to a spinner modal
    /// (`stale_recovery_in_progress`) that blocks all input until the
    /// result arrives via `poll_worktree_creation`.
    pub fn spawn_stale_worktree_recovery(&mut self, prompt: StaleWorktreePrompt) {
        // Extract the fields the background thread needs before
        // storing the prompt back for the spinner modal.
        let wi_id = prompt.wi_id.clone();
        let wi_id_for_payload = wi_id.clone();
        let repo_path = prompt.repo_path.clone();
        let stale_path = prompt.stale_path.clone();
        let branch = prompt.branch.clone();
        let open_session = prompt.open_session;

        // Re-populate the prompt so the UI can render the spinner modal.
        self.stale_worktree_prompt = Some(prompt);
        self.stale_recovery_in_progress = true;

        if self
            .try_begin_user_action(
                UserActionKey::WorktreeCreate,
                Duration::ZERO,
                "Recovering stale worktree...",
            )
            .is_none()
        {
            self.clear_stale_recovery();
            self.status_message = Some("Worktree operation already in progress...".into());
            return;
        }

        let ws = Arc::clone(&self.worktree_service);
        let wt_dir = self.config.defaults.worktree_dir.clone();
        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            let mut cleanup_errors: Vec<String> = Vec::new();

            // Step 1: Force-remove the stale worktree. If the path
            // doesn't exist on disk, `git worktree remove --force` still
            // cleans up the bookkeeping in .git/worktrees/.
            if let Err(e) = ws.remove_worktree(
                &repo_path,
                &stale_path,
                false, // don't delete the branch - it has the user's work
                true,  // force
            ) {
                cleanup_errors.push(format!("force-remove: {e}"));
            }

            // Step 2: Prune any remaining stale worktree entries.
            if let Err(e) = ws.prune_worktrees(&repo_path) {
                cleanup_errors.push(format!("prune: {e}"));
            }

            // Step 3: Retry worktree creation.
            let wt_target = Self::worktree_target_path(&repo_path, &branch, &wt_dir);

            let reused_wt =
                Self::find_reusable_worktree(ws.as_ref(), &repo_path, &branch, &wt_target);
            let (wt_result, reused) = reused_wt.map_or_else(
                || (ws.create_worktree(&repo_path, &branch, &wt_target), false),
                |existing| (Ok(existing), true),
            );

            match wt_result {
                Ok(wt_info) => {
                    let _ = tx.send(WorktreeCreateResult {
                        wi_id,
                        repo_path,
                        branch: Some(branch),
                        path: Some(wt_info.path),
                        error: None,
                        open_session,
                        branch_gone: false,
                        reused,
                        stale_worktree_path: None,
                    });
                }
                Err(e) => {
                    let mut msg = format!("Recovery failed: {e}");
                    if !cleanup_errors.is_empty() {
                        use std::fmt::Write as _;
                        let _ =
                            write!(msg, " (cleanup also failed: {})", cleanup_errors.join("; "));
                    }
                    let _ = tx.send(WorktreeCreateResult {
                        wi_id,
                        repo_path,
                        branch: Some(branch),
                        path: None,
                        error: Some(msg),
                        open_session,
                        branch_gone: false,
                        reused: false,
                        stale_worktree_path: None,
                    });
                }
            }
        });

        self.attach_user_action_payload(
            &UserActionKey::WorktreeCreate,
            UserActionPayload::WorktreeCreate {
                rx,
                wi_id: wi_id_for_payload,
            },
        );
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
                let pr_line = pr_url.as_ref().map_or_else(
                    || {
                        format!(
                            " Note: no pull request URL is available yet (it may still be creating). \
                             You can find it by running: gh pr list --head {branch_name}"
                        )
                    },
                    |url| format!(" Pull request: {url}."),
                );
                if review_gate_findings.is_empty() {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         Implementation is complete and ready for review.{pr_line}"
                    )
                } else {
                    format!(
                        "Worktree: {worktree_display}. Branch: {branch_name}. \
                         Implementation passed the review gate and is ready for review.{pr_line}"
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
                if review_gate_findings.is_empty() {
                    "review"
                } else {
                    "review_with_findings"
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

    /// Drain MCP events from the crossbeam channel.
    /// Called on the 200ms timer tick. Processes status updates, log events,
    /// and plan updates from all active MCP socket servers.
    pub fn poll_mcp_status_updates(&mut self) {
        let Some(ref rx) = self.mcp_rx else {
            return;
        };

        let mut events: Vec<McpEvent> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
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
                    if wi_ref.is_some_and(|w| w.status_derived) {
                        self.status_message = Some("MCP: status is derived from merged PR".into());
                        continue;
                    }

                    // Block all MCP transitions for review request items.
                    // Claude sessions should not drive workflow for someone else's PR.
                    if wi_ref.is_some_and(|w| w.kind == WorkItemKind::ReviewRequest) {
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
                        (
                            Some(WorkItemStatus::Implementing | WorkItemStatus::Blocked),
                            WorkItemStatus::Review
                        ) | (Some(WorkItemStatus::Implementing), WorkItemStatus::Blocked)
                            | (
                                Some(WorkItemStatus::Blocked | WorkItemStatus::Planning),
                                WorkItemStatus::Implementing
                            )
                    );
                    if !allowed {
                        self.status_message = Some(format!(
                            "MCP: transition from {} to {} is not allowed",
                            current_status.map_or("unknown", |s| s.badge_text()),
                            new_status.badge_text()
                        ));
                        continue;
                    }

                    // No-plan prompt: when Claude blocks because there is no
                    // implementation plan, offer the user a choice to retreat
                    // to Planning instead of staying blocked.
                    if let Some(current) = current_status
                        && current == WorkItemStatus::Implementing
                        && new_status == WorkItemStatus::Blocked
                        && reason.contains("No implementation plan")
                    {
                        // Apply the block first so the item is in Blocked state.
                        self.apply_stage_change(&wi_id, current, new_status, "mcp");

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
                    // `current_status` is populated above from `wi_ref.map(...)`;
                    // if the work item disappeared between the map and here
                    // (it cannot, wi_ref is still live up the stack), skip
                    // the stage change rather than panic.
                    let Some(current) = current_status else {
                        continue;
                    };
                    self.apply_stage_change(&wi_id, current, new_status, "mcp");

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
                                    WorkItemStatus::Planning,
                                    WorkItemStatus::Implementing,
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
                        self.agent_working.insert(wi_id);
                    } else {
                        self.agent_working.remove(&wi_id);
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
                            "MCP: repo '{repo_path}' not found or has no git dir"
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
    /// Calls `backend.import()` then spawns a background thread to fetch the
    /// branch and create a worktree. The UI remains responsive while the
    /// git operations run. Results are picked up by `poll_worktree_creation()`.
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
                let wi_id = record.id;
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
    /// Calls `backend.import_review_request()` then spawns a background thread
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
                let wi_id = record.id;
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

    /// Resolve the currently selected left-panel entry to the PR URL (and a
    /// short human-readable label) that should open when the user presses
    /// `o`. Returns `None` if there is no selection, the entry is not
    /// PR-bearing (e.g. a group header), or the selected work item has no
    /// repo association with a PR attached.
    ///
    /// For work items with multiple repo associations, the first association
    /// whose `pr` field is `Some(_)` wins. This is deterministic across
    /// repeat presses because `repo_associations` preserves insertion order
    /// through reassembly.
    ///
    /// Pure: does not spawn, does not shell out, does not mutate `self`.
    /// Split out so the dispatch logic can be unit-tested without shelling
    /// out to `open`.
    pub(crate) fn selected_pr_target(&self) -> Option<(String, String)> {
        let idx = self.selected_item?;
        let entry = self.display_list.get(idx)?;
        match entry {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                let wi = self.work_items.get(*wi_idx)?;
                let pr = wi.repo_associations.iter().find_map(|a| a.pr.as_ref())?;
                Some((pr.url.clone(), format!("PR #{}", pr.number)))
            }
            DisplayEntry::UnlinkedItem(u_idx) => {
                let ul = self.unlinked_prs.get(*u_idx)?;
                Some((ul.pr.url.clone(), format!("PR #{}", ul.pr.number)))
            }
            DisplayEntry::ReviewRequestItem(r_idx) => {
                let rr = self.review_requested_prs.get(*r_idx)?;
                Some((rr.pr.url.clone(), format!("PR #{}", rr.pr.number)))
            }
            DisplayEntry::GroupHeader { .. } => None,
        }
    }

    /// Open the currently selected entry's PR in the default browser via
    /// the macOS `open` command. Sets a status message with the PR label on
    /// success, or "No PR to open" when the selection has no PR.
    ///
    /// The subprocess is spawned on a background thread: even though
    /// `open` is a local shell-out (not remote I/O), running it on the UI
    /// thread would still violate the `[ABSOLUTE]` blocking-I/O invariant
    /// in `docs/UI.md` as soon as `open` stalls for any reason (e.g. a
    /// slow `LaunchServices` dispatch). The child's status and stderr are
    /// intentionally discarded - this is a best-effort UX affordance and
    /// surfacing `open` errors would add more noise than value.
    ///
    /// Not routed through `try_begin_user_action` because the user-action
    /// guard is scoped to remote I/O (`gh`, network, `git fetch`/`push`/
    /// `pull`) per `docs/UI.md` "User action guard", and `open` is none of
    /// those.
    pub fn open_selected_pr_in_browser(&mut self) {
        match self.selected_pr_target() {
            Some((url, label)) => {
                std::thread::spawn(move || {
                    let _ = std::process::Command::new("open").arg(&url).status();
                });
                self.status_message = Some(format!("Opening {label}"));
            }
            None => {
                self.status_message = Some("No PR to open".into());
            }
        }
    }

    /// Resolve the currently selected left-panel entry to a
    /// `(WorkItemId, repo path, branch)` triple suitable for the
    /// rebase-onto-main flow. Returns `None` if the selection is not a
    /// work item, has no repo association with both a worktree and a
    /// branch set, or is the only thing the caller could rebase
    /// (group headers, unlinked items, review-request items).
    ///
    /// The default-branch comparison is intentionally NOT done here:
    /// `default_branch` shells out to git, and this helper runs on the
    /// UI thread. A "branch == default branch" rebase is a no-op that
    /// the harness will detect on its own; gating it on the UI thread
    /// would require an unconditional blocking call that we cannot
    /// afford. The single-flight admission and the harness's idempotent
    /// `git rebase` are sufficient defence-in-depth.
    ///
    /// Pure: does not spawn, does not shell out, does not mutate
    /// `self`. Mirrors the shape of `selected_pr_target` so the
    /// dispatch site reads the same way.
    pub(crate) fn selected_rebase_target(&self) -> Option<RebaseTarget> {
        let idx = self.selected_item?;
        let entry = self.display_list.get(idx)?;
        match entry {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                let wi = self.work_items.get(*wi_idx)?;
                let assoc = wi
                    .repo_associations
                    .iter()
                    .find(|a| a.worktree_path.is_some() && a.branch.is_some())?;
                // Carry the worktree path, not the registered repo path:
                // each git worktree has its own HEAD, and the rebase MUST
                // run against this work item's branch, not whatever the
                // main checkout has checked out. The `.find` above already
                // filtered out associations without a worktree, so the
                // `as_ref()?` here is infallible defence-in-depth.
                Some(RebaseTarget {
                    wi_id: wi.id.clone(),
                    worktree_path: assoc.worktree_path.clone()?,
                    branch: assoc.branch.clone()?,
                })
            }
            DisplayEntry::UnlinkedItem(_)
            | DisplayEntry::ReviewRequestItem(_)
            | DisplayEntry::GroupHeader { .. } => None,
        }
    }

    /// Entry point for the `m` keybinding. Resolves the selected
    /// rebase target and either spawns the rebase gate or sets a
    /// "nothing to rebase" status message. Goes through
    /// `spawn_rebase_gate` which itself routes through
    /// `try_begin_user_action` for single-flight admission.
    pub fn start_rebase_on_main(&mut self) {
        let Some(target) = self.selected_rebase_target() else {
            self.status_message = Some("No branch to rebase".into());
            return;
        };
        // Reject a rebase on a work item that already has a rebase gate
        // in flight before talking to the user-action guard, so the
        // status message names the right cause.
        if self.rebase_gates.contains_key(&target.wi_id) {
            self.status_message = Some("Rebase already in progress for this item".into());
            return;
        }
        // Reject a rebase while the work item has a live interactive
        // session OR a live terminal tab. The rebase gate spawns a
        // headless Claude in the same worktree; if the interactive
        // session or the user's shell is concurrently editing files or
        // running git commands, the two processes will race on the
        // index and working tree, producing nondeterministic rebase
        // results or index-lock errors. Pure in-memory check (no I/O):
        // reads session_key_for + the SessionEntry.alive flag populated
        // by check_liveness, and terminal_sessions for the Terminal tab.
        let has_live_session = self
            .session_key_for(&target.wi_id)
            .and_then(|key| self.sessions.get(&key))
            .is_some_and(|entry| entry.alive);
        let has_live_terminal = self
            .terminal_sessions
            .get(&target.wi_id)
            .is_some_and(|entry| entry.alive);
        if has_live_session || has_live_terminal {
            self.status_message =
                Some("Cannot rebase while a session is active for this item".into());
            return;
        }
        self.spawn_rebase_gate(target);
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
                    stale_worktree_path: None,
                });
                return;
            }
            let wt_target = Self::worktree_target_path(&repo_path, &branch, &wt_dir);
            // Reuse an existing worktree only if it lives at the exact
            // expected location (wt_target) and is NOT the main worktree.
            // See `find_reusable_worktree` for rationale.
            let reused_wt =
                Self::find_reusable_worktree(ws.as_ref(), &repo_path, &branch, &wt_target);
            let (wt_result, reused) = reused_wt.map_or_else(
                || (ws.create_worktree(&repo_path, &branch, &wt_target), false),
                |existing_wt| (Ok(existing_wt), true),
            );
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
                        stale_worktree_path: None,
                    });
                }
                Err(crate::worktree_service::WorktreeError::BranchLockedToWorktree {
                    ref locked_at,
                    ..
                }) => {
                    let _ = tx.send(WorktreeCreateResult {
                        wi_id: wi_id_clone,
                        repo_path,
                        branch: Some(branch),
                        path: None,
                        error: Some(format!(
                            "Imported: {title} - branch is locked to a stale worktree at '{}'\n\
                             (likely from an interrupted rebase).",
                            locked_at.display(),
                        )),
                        open_session: false,
                        branch_gone: false,
                        reused: false,
                        stale_worktree_path: Some(locked_at.clone()),
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
                        stale_worktree_path: None,
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
                let wi_id = record.id;
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
                let wi_id = record.id;
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
    /// 2. Multiple repos - return "`MULTIPLE_REPOS`" so the caller opens the
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
        let targets: Vec<PathBuf> =
            if let Some(w) = self.work_items.iter().find(|w| w.id == dlg.wi_id) {
                w.repo_associations
                    .iter()
                    .filter(|a| a.branch.is_none())
                    .map(|a| a.repo_path.clone())
                    .collect()
            } else {
                self.status_message = Some("Work item not found".into());
                return;
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
                self.apply_stage_change(&dlg.wi_id, from, to, "user");
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
    /// Persists the change via `backend.update_status()` and reassembles.
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
        //
        // The unclean-worktree merge guard used to live here as a
        // synchronous read against the cached `repo_data` worktree
        // info. That cached path stayed stale across long sessions
        // and would refuse to open the modal even after the user had
        // committed and pushed minutes ago. The authoritative merge
        // guard now lives in `execute_merge` as a background
        // `WorktreeService::list_worktrees` precheck (see
        // `spawn_merge_precheck` / `poll_merge_precheck`); having a
        // second cached guard here would short-circuit the live check
        // and re-introduce exactly the stale-cache failure mode the
        // precheck was added to fix. So this branch unconditionally
        // opens the strategy picker - the live precheck classifies
        // the worktree before the actual `gh pr merge` thread fires
        // and surfaces the same dirty/untracked/unpushed wording as
        // an alert if it blocks. `BehindOnly` and `Clean` continue to
        // proceed to the merge as before.
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

        self.apply_stage_change(&wi_id, current_status, new_status, "user");
    }

    /// Retreat the selected work item to the previous workflow stage.
    /// Persists the change via `backend.update_status()` and reassembles.
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
        //
        // The merge can be in either of two phases here:
        // 1. Live precheck (`UserActionPayload::PrMergePrecheck`).
        // 2. Actual `gh pr merge` (`UserActionPayload::PrMerge`).
        // Both phases share the same `UserActionKey::PrMerge` slot, so
        // `is_user_action_in_flight` is the single check that covers
        // them and `end_user_action` drops both receivers structurally
        // because they live inside the slot's payload (no sibling
        // `Option<Receiver>` field to forget).
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

        self.apply_stage_change(&wi_id, current_status, new_status, "user");
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
        self.apply_stage_change(wi_id, current, next, "user");

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
        current_status: WorkItemStatus,
        new_status: WorkItemStatus,
        source: &str,
    ) {
        // Merge-gate guard: Done requires a verified PR merge or a
        // submitted review.  All other callers must go through the merge
        // prompt / poll_pr_merge path (source == "pr_merge") or the review
        // submission path (source == "review_submitted").
        if new_status == WorkItemStatus::Done
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

        if let Err(e) = self.backend.update_status(wi_id, new_status) {
            self.status_message = Some(format!("Stage update error: {e}"));
            return;
        }

        // Track when items enter/leave Done for auto-archival.
        let mut done_at_error = false;
        if new_status == WorkItemStatus::Done {
            match crate::side_effects::clock::system_now().duration_since(std::time::UNIX_EPOCH) {
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
        } else if current_status == WorkItemStatus::Done
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
        if new_status == WorkItemStatus::Review && !is_review_request {
            self.spawn_pr_creation(wi_id);
        }

        // Cancel any pending session-open plan-read for this work item
        // BEFORE the session kill block. The plan-read receiver lives in
        // `session_open_rx` (no entry in `self.sessions` yet), so the
        // session-kill branch below would not see it; without this
        // unconditional cancel, a stale pending open from the old
        // stage would survive the transition and `finish_session_open`
        // would later spawn the agent for the new stage - including
        // no-session stages like Done or Mergequeue. Cancelling the
        // entry here also signals the worker to skip remaining file
        // writes, routes the committed tempfile through
        // `spawn_agent_file_cleanup`, and ends the
        // "Opening session..." spinner.
        self.cancel_session_open_entry(wi_id);

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

        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            return;
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
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.status_message = Some(
                "PR creation skipped: GitHub remote not yet cached (waiting for next fetch)".into(),
            );
            return;
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
        let Ok(result) = recv_result else {
            self.end_user_action(&UserActionKey::PrCreate);
            self.status_message = Some("PR creation: background thread exited unexpectedly".into());
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
    /// Two-phase flow:
    ///
    /// 1. Pre-flight validity checks (in-memory only): `wi`, branch,
    ///    `repo_path`, GitHub remote cache. Failures alert and return
    ///    BEFORE admitting the helper slot so an early return cannot
    ///    leave an orphaned `UserActionKey::PrMerge` entry. See
    ///    `docs/UI.md` "User action guard" for the desync-guard rule.
    /// 2. Admit the helper slot, hide the status-bar spinner, set the
    ///    in-progress modal flag, and spawn a background working-tree
    ///    precheck via `spawn_merge_precheck`. The
    ///    `poll_merge_precheck` background-tick poller drains the
    ///    receiver and either hands off to
    ///    `perform_merge_after_precheck` (Ready) or surfaces the live
    ///    blocker as an alert (Blocked).
    ///
    /// The cleanliness check used to live here as a synchronous cache
    /// read against `repo_data`. That cached path stayed stale across
    /// long-running sessions: a user who fixed a dirty worktree
    /// minutes ago could still see the "Uncommitted changes" alert
    /// when trying to merge. The precheck phase replaces that read
    /// with a live `WorktreeService::list_worktrees` call (plus a
    /// live `GithubClient::fetch_live_merge_state` call for the
    /// remote PR state) on a background thread, so the merge guard
    /// always reflects the current state.
    pub fn execute_merge(&mut self, wi_id: &WorkItemId, strategy: &str) {
        // Single-flight guard via the user-action helper. Rejecting when
        // another merge is already in flight preserves the existing alert
        // wording verbatim - the background thread may have already
        // merged a PR on GitHub, so silently replacing the receiver
        // would lose the result.
        if self.is_user_action_in_flight(&UserActionKey::PrMerge) {
            self.alert_message = Some(PR_MERGE_ALREADY_IN_PROGRESS.into());
            return;
        }

        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            self.confirm_merge = false;
            self.merge_wi_id = None;
            self.alert_message = Some("Cannot merge: no repo association".into());
            return;
        };
        let branch = if let Some(b) = assoc.branch.as_ref() {
            b.clone()
        } else {
            self.confirm_merge = false;
            self.merge_wi_id = None;
            self.alert_message = Some("Cannot merge: no branch associated".into());
            return;
        };
        let repo_path = assoc.repo_path.clone();

        // Read owner/repo from the cached fetcher result rather than shelling
        // out on the UI thread. If no entry exists yet, the first fetch has
        // not completed - surface that as an alert instead of blocking.
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.confirm_merge = false;
            self.merge_wi_id = None;
            self.alert_message =
                Some("Cannot merge: GitHub remote not yet cached (waiting for next fetch)".into());
            return;
        };
        let owner_repo = format!("{owner}/{repo_name}");

        // All in-memory validity checks have passed. Admit the action
        // now so any rejection above cannot leave the helper with an
        // empty slot. The slot is reserved across BOTH the precheck
        // phase and the actual merge phase - `poll_merge_precheck`
        // either hands off to `perform_merge_after_precheck` (which
        // attaches the merge payload without re-admitting) or releases
        // the slot via `end_user_action`.
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
        // its own in-progress spinner (and now also the
        // "Refreshing remote state..." precheck spinner), and stacking
        // two is confusing. The helper map entry is still the single
        // source of truth for `is_user_action_in_flight(&PrMerge)`.
        if let Some(state) = self.user_actions.in_flight.get(&UserActionKey::PrMerge) {
            let aid = state.activity_id;
            self.end_activity(aid);
        }

        // Modal renders the spinner from the moment the user pressed
        // "merge" - the precheck phase shows "Refreshing remote
        // state..." and the merge phase shows "Merging pull
        // request...". The renderer in `src/ui.rs` keys off
        // `App::is_merge_precheck_phase()` to pick the right body,
        // which checks the helper slot's `UserActionPayload` variant.
        self.merge_in_progress = true;

        self.spawn_merge_precheck(
            wi_id.clone(),
            strategy.to_string(),
            repo_path,
            branch,
            owner_repo,
        );
    }

    /// Spawn the live merge precheck for an in-flight merge.
    ///
    /// Runs two live fetches on a background thread:
    /// 1. `WorktreeService::list_worktrees` for the local worktree
    ///    state (dirty / untracked / unpushed).
    /// 2. `GithubClient::fetch_live_merge_state` for the remote PR
    ///    state (mergeable flag + CI rollup).
    ///
    /// The results are handed to `MergeReadiness::classify` which
    /// encodes the canonical priority order
    /// `Dirty > Untracked > Unpushed > PrConflict > CiFailing >
    /// BehindOnly > Clean`, and
    /// `MergeReadiness::merge_block_message` translates the
    /// classification to the user-facing alert string. A `None`
    /// message means the precheck clears the merge; any `Some` is
    /// reported via `MergePreCheckMessage::Blocked`.
    ///
    /// The receiver is stored structurally inside the helper slot's
    /// `UserActionPayload::PrMergePrecheck` variant via
    /// `attach_user_action_payload`, so any cancel path that calls
    /// `end_user_action(&UserActionKey::PrMerge)` automatically
    /// drops it. `poll_merge_precheck` drains the receiver on the
    /// next ~200ms background tick.
    ///
    /// No-worktree fallthrough: if `list_worktrees` returns no entry
    /// matching `branch`, the precheck passes `None` to
    /// `MergeReadiness::classify` and the local checks short-circuit
    /// to "nothing to protect". PR-only / reassembled work items -
    /// and items whose local worktree was removed after the branch
    /// was pushed - have no checked-out tree to protect, so there is
    /// nothing for the dirty / untracked / unpushed guards to flag.
    /// Refusing to merge in that case would make perfectly safe PRs
    /// unmergeable from the UI. The cached guard this replaced
    /// treated a missing cache entry as `Clean` for the same reason.
    ///
    /// No-PR fallthrough: if `fetch_live_merge_state` reports
    /// `has_open_pr: false`, the remote checks short-circuit to "no
    /// remote constraints" and the classifier falls back to the
    /// local state. The downstream merge thread then surfaces the
    /// existing `NoPr` outcome.
    ///
    /// Blocking-I/O note: every call inside the spawned closure
    /// (`list_worktrees`, `fetch_live_merge_state`) is allowed to
    /// block - that is the entire reason they live off the main
    /// thread. The UI thread sees only the receiver and the
    /// `MergePreCheckMessage`. See `docs/UI.md` "Blocking I/O
    /// Prohibition".
    fn spawn_merge_precheck(
        &mut self,
        wi_id: WorkItemId,
        strategy: String,
        repo_path: PathBuf,
        branch: String,
        owner_repo: String,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ws = Arc::clone(&self.worktree_service);
        let github = Arc::clone(&self.github_client);
        let wi_id_for_thread = wi_id;
        let strategy_for_thread = strategy;
        let repo_path_for_thread = repo_path;
        let branch_for_thread = branch;
        let owner_repo_for_thread = owner_repo;

        std::thread::spawn(move || {
            // 1. Live worktree state. Reusing list_worktrees keeps
            //    the test harness identical to the fetcher path -
            //    any mock that returns a clean `WorktreeInfo` for
            //    the fetcher will return clean here too.
            let worktrees = match ws.list_worktrees(&repo_path_for_thread) {
                Ok(list) => list,
                Err(e) => {
                    let _ = tx.send(MergePreCheckMessage::Blocked {
                        reason: format!("Cannot merge: working-tree check failed: {e}"),
                    });
                    return;
                }
            };
            let wt = worktrees
                .into_iter()
                .find(|w| w.branch.as_deref() == Some(&branch_for_thread));

            // 2. Live remote PR state. Split `owner_repo` at the
            //    first `/` - the caller guarantees this shape in
            //    `execute_merge`, which derives it from
            //    `repo_data[path].github_remote`. If the split fails
            //    (malformed remote URL), block with a diagnostic
            //    alert - the P0 "surface errors, don't auto-fix"
            //    posture.
            let (owner, repo) = match owner_repo_for_thread.split_once('/') {
                Some((o, r)) if !o.is_empty() && !r.is_empty() => (o.to_string(), r.to_string()),
                _ => {
                    let _ = tx.send(MergePreCheckMessage::Blocked {
                        reason: format!(
                            "Cannot merge: malformed owner/repo identifier: {owner_repo_for_thread}"
                        ),
                    });
                    return;
                }
            };
            let live_pr = match github.fetch_live_merge_state(&owner, &repo, &branch_for_thread) {
                Ok(state) => state,
                Err(e) => {
                    let _ = tx.send(MergePreCheckMessage::Blocked {
                        reason: format!("Cannot merge: remote merge-state check failed: {e}"),
                    });
                    return;
                }
            };

            // 3. Classify the combined state and translate to the
            //    precheck message. `classify` owns the priority
            //    order; `merge_block_message` owns the user-facing
            //    wording.
            let readiness = MergeReadiness::classify(wt.as_ref(), &live_pr);
            let msg = readiness.merge_block_message().map_or_else(
                || MergePreCheckMessage::Ready {
                    wi_id: wi_id_for_thread,
                    strategy: strategy_for_thread,
                    branch: branch_for_thread,
                    repo_path: repo_path_for_thread,
                    owner_repo: owner_repo_for_thread,
                },
                |reason| MergePreCheckMessage::Blocked {
                    reason: reason.to_string(),
                },
            );
            let _ = tx.send(msg);
        });

        // Move the receiver into the helper slot's payload so it is
        // owned structurally. This MUST come after `try_begin_user_action`
        // (called by `execute_merge` upstream) reserved the slot with
        // `UserActionPayload::Empty` - we are replacing that empty
        // payload with `PrMergePrecheck`. End-of-life is automatic:
        // every `end_user_action(&UserActionKey::PrMerge)` drops the
        // slot and the receiver in the same step.
        self.attach_user_action_payload(
            &UserActionKey::PrMerge,
            UserActionPayload::PrMergePrecheck { rx },
        );
    }

    /// Returns true when the `UserActionKey::PrMerge` slot is in the
    /// precheck phase - i.e. its payload is
    /// `UserActionPayload::PrMergePrecheck`. Used by the merge
    /// confirm modal renderer in `src/ui.rs` to switch between the
    /// "Refreshing remote state..." and "Merging pull request..."
    /// body strings without touching internal helper-map fields.
    /// Pure in-memory check, safe on the UI thread.
    pub fn is_merge_precheck_phase(&self) -> bool {
        matches!(
            self.user_action_payload(&UserActionKey::PrMerge),
            Some(UserActionPayload::PrMergePrecheck { .. })
        )
    }

    /// Optional hint line appended to the merge-confirm modal body
    /// whenever the cached repo state already shows a signal that
    /// the live precheck is likely to block on.
    ///
    /// This is a soft, advisory hint - it never refuses to open the
    /// modal and never short-circuits the precheck. The whole point
    /// of the precheck is that cache can be stale, so the cached
    /// state is consulted ONLY for a textual reassurance, never for
    /// a go / no-go decision. If the cache is stale the worst case
    /// is a spurious hint; the precheck still runs and is
    /// authoritative.
    ///
    /// Returned variants:
    /// - `Some("Live re-check will run before merging.")` when any
    ///   of `git_state.dirty`, `git_state.ahead > 0`,
    ///   `PrInfo.mergeable == Conflicting`, or `PrInfo.checks ==
    ///   Failing` is observed on any repo association.
    /// - `Some("CI still running; merge will queue on branch
    ///   protection.")` when the ONLY concerning signal is
    ///   `PrInfo.checks == Pending` (no other hard-block hint
    ///   fires). Pending CI does not block the merge but the user
    ///   may still want to know that branch protection will queue
    ///   the merge until checks land.
    /// - `None` in all other cases.
    ///
    /// Pure in-memory read, safe on the UI thread.
    pub fn merge_confirm_hint(&self, wi_id: &WorkItemId) -> Option<&'static str> {
        let wi = self.work_items.iter().find(|w| &w.id == wi_id)?;

        let mut hard_block = false;
        let mut pending_only = false;
        for assoc in &wi.repo_associations {
            if let Some(gs) = assoc.git_state.as_ref()
                && (gs.dirty || gs.ahead > 0)
            {
                hard_block = true;
            }
            if let Some(pr) = assoc.pr.as_ref() {
                if matches!(pr.mergeable, crate::work_item::MergeableState::Conflicting) {
                    hard_block = true;
                }
                match pr.checks {
                    crate::work_item::CheckStatus::Failing => hard_block = true,
                    crate::work_item::CheckStatus::Pending => pending_only = true,
                    _ => {}
                }
            }
        }

        if hard_block {
            Some("Live re-check will run before merging.")
        } else if pending_only {
            Some("CI still running; merge will queue on branch protection.")
        } else {
            None
        }
    }

    /// Drain the live merge precheck receiver on the background tick.
    ///
    /// Wired into the same ~200ms tick as every other background
    /// poller. Does nothing if the helper slot is empty or carries a
    /// non-precheck payload (e.g. the merge phase has already taken
    /// over). On a Ready message, hands off to
    /// `perform_merge_after_precheck`; on Blocked / disconnected,
    /// releases the helper slot and surfaces the reason as an alert.
    ///
    /// The receiver lives inside `UserActionPayload::PrMergePrecheck`
    /// so there is no separate `Option<Receiver>` field on `App`. If
    /// any cancel path called `end_user_action(&UserActionKey::PrMerge)`
    /// while the precheck thread was still running, the receiver was
    /// dropped in the same step and this poller's `try_recv` will
    /// either see `Empty` (slot already gone) or `Disconnected`
    /// (sender's channel half closed). The disconnected branch can
    /// no longer fire the "thread terminated unexpectedly" alert
    /// against an unrelated work item, because if the slot is gone
    /// we never enter the body in the first place.
    pub fn poll_merge_precheck(&mut self) {
        // First check: is there a precheck-phase payload at all?
        // `user_action_payload` returns `None` when the slot has
        // been released by any cancel path, which is the structural
        // equivalent of the old "merge_precheck_rx is None" guard.
        let Some(UserActionPayload::PrMergePrecheck { rx }) =
            self.user_action_payload(&UserActionKey::PrMerge)
        else {
            return;
        };
        let msg = match rx.try_recv() {
            Ok(m) => m,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                // Background thread died without sending. Release the
                // helper slot (which drops the receiver), clear the
                // modal state, and surface a generic error so the
                // user can retry.
                self.confirm_merge = false;
                self.merge_wi_id = None;
                self.merge_in_progress = false;
                self.end_user_action(&UserActionKey::PrMerge);
                self.alert_message = Some("Merge precheck thread terminated unexpectedly".into());
                return;
            }
        };

        match msg {
            MergePreCheckMessage::Ready {
                wi_id,
                strategy,
                branch,
                repo_path,
                owner_repo,
            } => {
                // The slot is still ours (we just observed a payload
                // above) and `perform_merge_after_precheck` will
                // replace `PrMergePrecheck` with `PrMerge` via
                // `attach_user_action_payload`, dropping the precheck
                // receiver in the same step.
                self.perform_merge_after_precheck(wi_id, strategy, branch, repo_path, owner_repo);
            }
            MergePreCheckMessage::Blocked { reason } => {
                // The live state blocks the merge. Release the
                // helper slot (which drops the precheck receiver
                // structurally), clear the modal, and surface the
                // user-facing reason. The wording for the
                // dirty / untracked / unpushed / PR-conflict /
                // CI-failing cases comes from
                // `MergeReadiness::merge_block_message`.
                self.confirm_merge = false;
                self.merge_wi_id = None;
                self.merge_in_progress = false;
                self.end_user_action(&UserActionKey::PrMerge);
                self.alert_message = Some(reason);
            }
        }
    }

    /// Spawn the actual `gh pr merge` background thread after the live
    /// precheck has cleared. Called from `poll_merge_precheck` on
    /// `MergePreCheckMessage::Ready` - the helper slot was already
    /// admitted in `execute_merge`, so this method does NOT re-admit
    /// the action key; it only attaches the merge payload to the
    /// already-reserved slot.
    fn perform_merge_after_precheck(
        &mut self,
        wi_id: WorkItemId,
        strategy: String,
        branch: String,
        repo_path: PathBuf,
        owner_repo: String,
    ) {
        let merge_flag = if strategy == "merge" {
            "--merge"
        } else {
            "--squash"
        };
        let strategy_owned = strategy;
        let wi_id_clone = wi_id;
        let merge_flag_owned = merge_flag.to_string();
        let repo_path_clone = repo_path;
        let branch_for_thread = branch;
        let owner_repo_for_thread = owner_repo;

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            // Check if a PR exists for this branch and fetch its identity.
            let pr_identity = match std::process::Command::new("gh")
                .args([
                    "pr",
                    "list",
                    "--head",
                    &branch_for_thread,
                    "--json",
                    "number,title,url",
                    "--repo",
                    &owner_repo_for_thread,
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
                        &branch_for_thread,
                        &merge_flag_owned,
                        "--delete-branch",
                        "--repo",
                        &owner_repo_for_thread,
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
                branch: branch_for_thread,
                repo_path: repo_path_clone,
                outcome,
            });
        });

        self.attach_user_action_payload(&UserActionKey::PrMerge, UserActionPayload::PrMerge { rx });
        // `merge_in_progress` was already set by `execute_merge` so the
        // modal spinner has been rendering throughout the precheck phase.
        // No need to set it again here.
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
        let Ok(result) = recv_result else {
            self.end_user_action(&UserActionKey::PrMerge);
            self.merge_in_progress = false;
            self.confirm_merge = false;
            self.merge_wi_id = None;
            self.alert_message = Some("PR merge: background thread exited unexpectedly".into());
            return;
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
                    WorkItemStatus::Review,
                    WorkItemStatus::Done,
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
                    WorkItemStatus::Review,
                    WorkItemStatus::Implementing,
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

        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            self.status_message = Some("Cannot submit review: no repo association".into());
            return;
        };
        let branch = if let Some(b) = assoc.branch.as_ref() {
            b.clone()
        } else {
            self.status_message = Some("Cannot submit review: no branch".into());
            return;
        };
        let repo_path = assoc.repo_path.clone();
        // Read owner/repo from the cached fetcher result rather than shelling
        // out on the UI thread. The first fetch populates it; until then we
        // surface a message rather than block.
        //
        // Early returns above run BEFORE try_begin_user_action so a
        // cache miss cannot leave an orphaned helper entry.
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.status_message = Some(
                "Cannot submit review: GitHub remote not yet cached (waiting for next fetch)"
                    .into(),
            );
            return;
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
        let Ok(result) = recv_result else {
            self.end_user_action(&UserActionKey::ReviewSubmit);
            self.status_message =
                Some("Review submission: background thread exited unexpectedly".into());
            return;
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
                    WorkItemStatus::Review,
                    WorkItemStatus::Done,
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
        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            self.status_message = Some("Cannot enter mergequeue: no repo association".into());
            return;
        };
        let branch = if let Some(b) = assoc.branch.as_ref() {
            b.clone()
        } else {
            self.status_message = Some("Cannot enter mergequeue: no branch".into());
            return;
        };
        let pr_number = if let Some(pr) = assoc.pr.as_ref() {
            pr.number
        } else {
            self.status_message = Some("Cannot enter mergequeue: no PR found".into());
            return;
        };
        let repo_path = assoc.repo_path.clone();
        // Read owner/repo from the cached fetcher result - never shell out
        // on the UI thread.
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.status_message = Some(
                "Cannot enter mergequeue: GitHub remote not yet cached \
                 (waiting for next fetch)"
                    .into(),
            );
            return;
        };
        let owner_repo = format!("{owner}/{repo_name}");

        self.mergequeue_watches.push(PrMergeWatch {
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
            WorkItemStatus::Review,
            WorkItemStatus::Mergequeue,
            "user",
        );
        self.status_message = Some("Entered mergequeue - polling PR until merged".into());
    }

    impl_pr_merge_poll_method!(
        /// Poll the PR state for items in the Mergequeue. Called on
        /// each timer tick. Spawns at most one background thread per
        /// watched item at a time, with a 30-second cooldown between
        /// polls.
        ///
        /// Generated from `impl_pr_merge_poll_method!` so it shares
        /// its Phase 1 drain, still-eligible guard, `pr_number`
        /// backfill, merge-gate dispatch, and Phase 2 cooldown /
        /// subprocess spawn logic with `poll_review_request_merges`.
        /// The per-stage deltas are passed as macro arguments.
        fn poll_mergequeue,
        watches = mergequeue_watches,
        polls = mergequeue_polls,
        errors = mergequeue_poll_errors,
        source_stage = WorkItemStatus::Mergequeue,
        kind_filter = None,
        strategy_tag = "external",
        merged_message = "PR merged externally - moved to [DN]",
        closed_message = "PR was closed without merging - retreat to Review or re-open the PR",
        poll_error_prefix = "Mergequeue poll error",
        poll_label_prefix = "Polling PR for merge",
        // Mergequeue path owns a worktree the user is actively
        // working in; clean it up on a successful merge so it does
        // not linger.
        cleanup_worktree_on_merge = true,
    );

    impl_pr_merge_poll_method!(
        /// Poll the PR state for `ReviewRequest` work items in Review
        /// so that an external merge of the PR can auto-transition
        /// the item to Done.
        ///
        /// This is the only code path that can observe a merged
        /// review-request PR: the author-filtered `gh pr list
        /// --author @me` fetch cannot see it (wrong author), the
        /// `review-requested:@me` search is `--state open` (wrong
        /// state), the `pr_identity` fallback is `Done`-only (wrong
        /// status), and the startup merged-PR backfill is also
        /// author-filtered. Without this poll the item stays stuck
        /// in `[RR][RV]` forever.
        ///
        /// Generated from `impl_pr_merge_poll_method!` so it shares
        /// its Phase 1 drain, still-eligible guard, `pr_number`
        /// backfill, merge-gate dispatch, and Phase 2 cooldown /
        /// subprocess spawn logic with `poll_mergequeue`. The
        /// per-stage deltas are passed as macro arguments. The
        /// distinct `strategy` tag (`external_review_merge`) lets
        /// metrics tell reviewer-side merges apart from author-side
        /// Mergequeue merges.
        ///
        /// This path must NOT call `cleanup_worktree_for_item`:
        /// `worktree_service.remove_worktree` is a blocking I/O
        /// operation and would freeze the UI from the timer tick
        /// (see `docs/UI.md` "Blocking I/O Prohibition"). The
        /// worktree is left on disk and cleaned up later by
        /// auto-archive (default 7 days) or immediately by the user
        /// with Ctrl+D.
        fn poll_review_request_merges,
        watches = review_request_merge_watches,
        polls = review_request_merge_polls,
        errors = review_request_merge_poll_errors,
        source_stage = WorkItemStatus::Review,
        kind_filter = Some(WorkItemKind::ReviewRequest),
        strategy_tag = "external_review_merge",
        merged_message = "Review request PR merged externally - moved to [DN]",
        closed_message = "Review request PR was closed without merging - Ctrl+D to clean up",
        poll_error_prefix = "Review request poll error",
        poll_label_prefix = "Polling review-request PR for merge",
        cleanup_worktree_on_merge = false,
    );

    impl_pr_merge_reconstruct_method!(
        /// Reconstruct mergequeue watches from the current
        /// `work_items` snapshot. Called after every reassembly so
        /// that any item in `Mergequeue` gets exactly one watch and
        /// app restarts never lose state.
        ///
        /// This is an add-only pass: stale watches / in-flight polls
        /// / errors for items that are no longer in `Mergequeue` are
        /// cleaned up lazily by `poll_mergequeue`'s Phase 1 drain
        /// (its still-eligible guard) on the next poll cycle. See
        /// `impl_pr_merge_reconstruct_method!` for the reasoning.
        ///
        /// Generated from `impl_pr_merge_reconstruct_method!` so it
        /// shares its idempotent-add logic with
        /// `reconstruct_review_request_merge_watches`.
        fn reconstruct_mergequeue_watches,
        watches = mergequeue_watches,
        source_stage = WorkItemStatus::Mergequeue,
        kind_filter = None,
    );

    impl_pr_merge_reconstruct_method!(
        /// Reconstruct `ReviewRequest` merge watches from the current
        /// `work_items` snapshot. Called after every reassembly so
        /// that any `ReviewRequest`-kind item in `Review` gets
        /// exactly one watch and app restarts never lose state.
        ///
        /// This is an add-only pass: stale watches / in-flight polls
        /// / errors for items that are no longer eligible are cleaned
        /// up lazily by `poll_review_request_merges`'s Phase 1 drain
        /// (its still-eligible guard) on the next poll cycle.
        ///
        /// `ReviewRequest` items almost never carry a live `assoc.pr`
        /// because the `--author @me` fetch filters their
        /// author-side PRs out, so reconstructions here will
        /// generally start with `pr_number = None` and rely on the
        /// first poll's branch-based fallback to resolve the number.
        ///
        /// Generated from `impl_pr_merge_reconstruct_method!` so it
        /// shares its idempotent-add logic with
        /// `reconstruct_mergequeue_watches`.
        fn reconstruct_review_request_merge_watches,
        watches = review_request_merge_watches,
        source_stage = WorkItemStatus::Review,
        kind_filter = Some(WorkItemKind::ReviewRequest),
    );

    /// Collect Done items that need PR identity backfill (have a branch but
    /// no persisted `pr_identity`). Returns tuples of
    /// (`wi_id`, `repo_path`, branch, `github_owner`, `github_repo`).
    ///
    /// Reads owner/repo from the cached `repo_data[path].github_remote`
    /// populated by the background fetcher. When the fetcher has not
    /// produced a result for a repo yet, that repo is silently skipped
    /// (the next reassembly will retry) - we NEVER shell out via
    /// `worktree_service.github_remote(...)` on the UI thread.
    ///
    /// Temporary migration helper - can be removed once all existing Done
    /// items have been backfilled (i.e. no Done items with `pr_identity=None`
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
                let Some((owner, repo_name)) = self
                    .repo_data
                    .get(&assoc.repo_path)
                    .and_then(|rd| rd.github_remote.clone())
                else {
                    continue;
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
        let Some(rx) = self.pr_identity_backfill_rx.as_ref() else {
            return false;
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
    /// Uses `delete_branch=true` so the merged branch is cleaned up. Uses force=false
    /// because post-merge worktrees should be clean and `-d` is safe for merged branches.
    fn cleanup_worktree_for_item(&mut self, wi_id: &WorkItemId) {
        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
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
            let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
                return ReviewGateSpawn::Blocked("Work item not found".into());
            };
            let Some(assoc) = wi.repo_associations.first() else {
                return ReviewGateSpawn::Blocked("Cannot enter Review: no repo association".into());
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

        // Resolve the per-work-item harness BEFORE starting any
        // activity or background work. The plan's Milestone 3
        // acceptance-criteria rule is "abort rather than default to
        // claude" - review gates only run after an interactive session
        // has existed (the c/x entry point records the choice), so a
        // missing `harness_choice` entry is a user-facing error, not a
        // silent default. See `docs/harness-contract.md` Change Log
        // 2026-04-16 and the
        // `harness_choice_applied_to_review_gate_spawn` test.
        let Some(agent_backend) = self.backend_for_work_item(wi_id) else {
            return ReviewGateSpawn::Blocked(
                    "Cannot run review gate: no harness chosen for this work item. Press c / x to pick one and re-open the session first.".into(),
                );
        };

        // Resolve per-repo MCP servers up-front (UI thread) and convert
        // them into `McpBridgeSpec` so the headless review gate can pass
        // them through to Codex via per-key `-c` overrides alongside the
        // workbridge bridge. HTTP entries are skipped because Codex's
        // `mcp_servers.<name>` schema requires command + args. R3-F-3:
        // surface the skip via a toast so the user knows why an HTTP
        // MCP server they configured is not visible to the Codex review
        // gate (would otherwise be a silent feature gap vs. Claude).
        let (review_extra_bridges, http_skipped_for_review): (
            Vec<crate::agent_backend::McpBridgeSpec>,
            usize,
        ) = {
            let repo_display = crate::config::collapse_home(&repo_path);
            let entries = self.config.mcp_servers_for_repo(&repo_display);
            let http_count = entries.iter().filter(|e| e.server_type == "http").count();
            let bridges: Vec<crate::agent_backend::McpBridgeSpec> = entries
                .into_iter()
                .filter(|entry| entry.server_type != "http")
                .filter_map(|entry| {
                    entry
                        .command
                        .as_ref()
                        .map(|cmd| crate::agent_backend::McpBridgeSpec {
                            name: entry.name.clone(),
                            command: PathBuf::from(cmd),
                            args: entry.args.clone(),
                        })
                })
                .collect();
            (bridges, http_count)
        };
        if agent_backend.kind() == AgentBackendKind::Codex && http_skipped_for_review > 0 {
            self.push_toast(format!(
                "Codex: {http_skipped_for_review} HTTP MCP server(s) skipped (Codex requires stdio)"
            ));
        }

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
        // 3. Adversarial code review (headless agent spawn)
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

            let default_branch = ws
                .default_branch(&repo_path)
                .unwrap_or_else(|_| "main".to_string());

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
            }

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

                let pr_num = current_pr_number
                    .or_else(|| Self::find_pr_for_branch(&gh_owner, &gh_repo, &branch));

                if pr_num.is_none() {
                    let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!(
                            "No pull request found for branch '{branch}'. \
                             Create a PR before requesting review."
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
                        crate::side_effects::clock::sleep(std::time::Duration::from_secs(15));
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
            let config_path = crate::side_effects::paths::temp_dir()
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
            let rg_bridge = crate::agent_backend::McpBridgeSpec {
                name: "workbridge".to_string(),
                command: exe_path,
                args: vec![
                    "--mcp-bridge".to_string(),
                    "--socket".to_string(),
                    gate_socket.to_string_lossy().into_owned(),
                ],
            };

            let json_schema = r#"{"type":"object","properties":{"approved":{"type":"boolean"},"detail":{"type":"string"}},"required":["approved","detail"]}"#;

            // Build the argv for the headless review-gate spawn via the
            // agent backend. See `docs/harness-contract.md` RP2 for the
            // Claude Code reference payload.
            let rg_cfg = ReviewGateSpawnConfig {
                system_prompt: &system,
                initial_prompt: &prompt,
                json_schema,
                mcp_config_path: &config_path,
                mcp_bridge: &rg_bridge,
                extra_bridges: &review_extra_bridges,
            };
            let rg_argv = agent_backend.build_review_gate_command(&rg_cfg);

            let result = match std::process::Command::new(agent_backend.command_name())
                .args(&rg_argv)
                .output()
            {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout).to_string();
                    let verdict = agent_backend.parse_review_gate_stdout(&text);
                    ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: verdict.approved,
                        detail: verdict.detail,
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!("{}: {stderr}", agent_backend.command_name()),
                    }
                }
                Err(e) => ReviewGateResult {
                    work_item_id: wi_id_clone,
                    approved: false,
                    detail: format!("could not run {}: {e}", agent_backend.command_name()),
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

    /// Spawn the async rebase-onto-main background gate for the given
    /// work item. Modelled on `spawn_review_gate`: every blocking step
    /// (`git fetch`, the headless harness child, default-branch
    /// resolution) runs inside the spawned thread so the UI thread is
    /// never blocked.
    ///
    /// Single-flight admission goes through `try_begin_user_action`
    /// with `UserActionKey::RebaseOnMain` and a 500 ms debounce, so
    /// rapid `m` presses are coalesced.
    pub fn spawn_rebase_gate(&mut self, target: RebaseTarget) {
        let RebaseTarget {
            wi_id,
            worktree_path,
            branch,
        } = target;

        // Resolve the per-work-item harness BEFORE admitting the user
        // action. The plan's Milestone 3 rule is "abort rather than
        // default to claude" - the rebase gate only runs after an
        // interactive session has existed, so a missing harness choice
        // is a user-facing error rather than a silent default. See
        // `docs/harness-contract.md` Change Log 2026-04-16 and the
        // `harness_choice_applied_to_rebase_gate_spawn` test. We bail
        // BEFORE `try_begin_user_action` so the 500 ms debounce does
        // not eat a repeat press - this way the user can press `c` to
        // pick a harness and immediately retry.
        let Some(agent_backend) = self.backend_for_work_item(&wi_id) else {
            self.status_message = Some(
                "Cannot rebase: no harness chosen for this work item. Press c / x to pick one first.".into(),
            );
            return;
        };

        // Resolve per-repo MCP servers up-front (UI thread) and convert
        // them into `McpBridgeSpec` so the background harness sub-thread
        // can pass them through to Codex via per-key `-c` overrides
        // alongside the workbridge bridge. Computing here (rather than
        // inside the thread) keeps `self.config` reads on the UI thread,
        // matching how `begin_session_open` does it. HTTP entries are
        // skipped: Codex's `mcp_servers.<name>` schema requires command
        // + args. See `agent_backend::McpBridgeSpec`. R3-F-3: count the
        // skipped HTTP entries so we can surface a toast (silent skip
        // is a feature gap vs Claude, where HTTP entries are still
        // visible via the `--mcp-config` JSON).
        let (rebase_extra_bridges, http_skipped_for_rebase): (
            Vec<crate::agent_backend::McpBridgeSpec>,
            usize,
        ) = self
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
            .and_then(|w| w.repo_associations.first())
            .map(|assoc| {
                let repo_display = crate::config::collapse_home(&assoc.repo_path);
                let entries = self.config.mcp_servers_for_repo(&repo_display);
                let http_count = entries.iter().filter(|e| e.server_type == "http").count();
                let bridges: Vec<crate::agent_backend::McpBridgeSpec> = entries
                    .into_iter()
                    .filter(|entry| entry.server_type != "http")
                    .filter_map(|entry| {
                        entry
                            .command
                            .as_ref()
                            .map(|cmd| crate::agent_backend::McpBridgeSpec {
                                name: entry.name.clone(),
                                command: PathBuf::from(cmd),
                                args: entry.args.clone(),
                            })
                    })
                    .collect();
                (bridges, http_count)
            })
            .unwrap_or_default();
        if agent_backend.kind() == AgentBackendKind::Codex && http_skipped_for_rebase > 0 {
            self.push_toast(format!(
                "Codex: {http_skipped_for_rebase} HTTP MCP server(s) skipped (Codex requires stdio)"
            ));
        }

        // Single-flight admission. The 500 ms debounce matches
        // `Ctrl+R`: rapid presses are intentionally coalesced.
        let Some(activity) = self.try_begin_user_action(
            UserActionKey::RebaseOnMain,
            Duration::from_millis(500),
            "Rebasing onto upstream main",
        ) else {
            return;
        };
        // Attach the WorkItemId payload so any caller that consults
        // `user_action_work_item(&RebaseOnMain)` can find the owning
        // item without scanning the rebase_gates map.
        self.attach_user_action_payload(
            &UserActionKey::RebaseOnMain,
            UserActionPayload::RebaseOnMain {
                wi_id: wi_id.clone(),
            },
        );

        let ws = Arc::clone(&self.worktree_service);
        let backend = Arc::clone(&self.backend);
        let (tx, rx) = crossbeam_channel::unbounded::<RebaseGateMessage>();
        let wi_id_clone = wi_id.clone();
        // Shared PID slot for the harness child. The outer thread
        // here owns the Arc and stores a clone in `RebaseGateState`
        // below; the inner harness sub-thread (further down) gets
        // its own clone so it can populate the PID after spawning
        // and clear it after `wait_with_output` returns. The main
        // thread can SIGKILL via `drop_rebase_gate` at any time.
        let child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        let child_pid_for_state = Arc::clone(&child_pid);
        // Cancellation flag for the pre-spawn window. The background
        // thread runs several blocking phases (default-branch
        // resolution, `git fetch`, MCP server start, temp-config
        // write) BEFORE the harness child has a PID, so the SIGKILL
        // path in `drop_rebase_gate` cannot stop the thread on its
        // own. The thread polls this flag at the start of each phase
        // and the harness sub-thread checks it again immediately
        // after `Command::spawn` returns. Set by `drop_rebase_gate`.
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_for_state = Arc::clone(&cancelled);

        // Insert the gate state into `rebase_gates` BEFORE spawning
        // the background thread. The state holds the Arc<AtomicBool>
        // cancellation flag and the Arc<Mutex<Option<u32>>> PID slot,
        // both of which the background thread reads. If we spawned
        // first and inserted second, there would be a (microsecond)
        // window in which the background thread is running but
        // `drop_rebase_gate` would find no entry to cancel; doing
        // the insert first eliminates that race entirely.
        self.rebase_gates.insert(
            wi_id.clone(),
            RebaseGateState {
                rx,
                progress: Some("Resolving base branch...".to_string()),
                activity,
                child_pid: child_pid_for_state,
                cancelled: cancelled_for_state,
            },
        );

        // `agent_backend` was resolved at the top of this function
        // from `harness_choice`; reuse it here rather than reading
        // `self.agent_backend` again.
        std::thread::spawn(move || {
            // Cancellation check at the very start of the thread.
            // If `drop_rebase_gate` ran between the insert above and
            // this thread getting scheduled, we exit immediately
            // without touching git or starting the MCP server.
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            // === Phase 1: resolve default branch (background only) ===
            //
            // `default_branch` is queried against the worktree path
            // because that is the git context every later phase will
            // use; refs are shared across worktrees in the same repo, so
            // the answer is identical to querying the main checkout, and
            // keeping the path consistent avoids a second source of
            // truth.
            let base_branch = ws
                .default_branch(&worktree_path)
                .unwrap_or_else(|_| "main".to_string());

            // Cancellation check between phase 1 and phase 2:
            // `default_branch` may shell out to git, so it is the
            // first observable place where the background thread can
            // notice that the gate has been torn down.
            if cancelled.load(Ordering::SeqCst) {
                return;
            }

            // The compute-result block below uses `break 'compute` to
            // emit a `RebaseResult` from any phase. Pre-harness
            // failures (fetch failure, MCP server start failure,
            // exe-path failure, config-write failure) used to `return`
            // immediately, which bypassed the audit-log append below
            // and silently dropped the `rebase_failed` entry that
            // RP6 / docs/UI.md promise will be written. The labeled
            // block routes every non-cancelled outcome through the
            // common audit path.
            //
            // `gate_server` and `config_path` are declared OUTSIDE
            // the block so cleanup can run uniformly after the block
            // exits, regardless of which branch caused the break.
            // Cancellation paths break with `None`, which the
            // post-block check converts into a bare `return` (no
            // audit, no send) per the cancellation contract in C10.
            let mut gate_server: Option<crate::mcp::McpSocketServer> = None;
            let mut config_path: Option<std::path::PathBuf> = None;
            let mut conflicts_attempted_observed = false;

            let computed: Option<RebaseResult> = 'compute: {
                // === Phase 2: git fetch origin <base> ===
                //
                // We use the explicit refspec
                // `+<base>:refs/remotes/origin/<base>` instead of the
                // shorthand `git fetch origin <base>` so the fetch is
                // guaranteed to update the remote-tracking ref the
                // harness and the verification below both consult. The
                // shorthand form relies on git's "opportunistic
                // remote-tracking branch update", which only fires when
                // the remote's configured fetch refspec covers `<base>`;
                // in repos cloned with `--single-branch` of a different
                // branch, or with a customised `[remote "origin"] fetch`
                // refspec that omits `<base>`, the shorthand would only
                // update FETCH_HEAD and `origin/<base>` could stay
                // stale, producing a false "Rebased onto origin/<base>"
                // success even though the rebase landed on an old tip.
                // The leading `+` enables non-fast-forward updates so a
                // force-pushed base branch is also handled correctly.
                let refspec = format!("+{base_branch}:refs/remotes/origin/{base_branch}");
                let _ = tx.send(RebaseGateMessage::Progress(format!(
                    "Fetching origin/{base_branch}..."
                )));
                // The fetch goes through `run_cancellable` so it
                // runs in its own process group and the PID slot is
                // managed with the correct "stash first, check
                // second" ordering. See `run_cancellable` for the
                // contract and why the ordering matters.
                match run_cancellable(
                    crate::worktree_service::git_command()
                        .arg("-C")
                        .arg(&worktree_path)
                        .args(["fetch", "origin", &refspec]),
                    &child_pid,
                    &cancelled,
                ) {
                    Ok(SubprocessOutcome::Completed(out)) if out.status.success() => {}
                    Ok(SubprocessOutcome::Completed(out)) => {
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        break 'compute Some(RebaseResult::Failure {
                            base_branch,
                            reason: format!("git fetch failed: {}", stderr.trim()),
                            conflicts_attempted: false,
                            activity_log_error: None,
                        });
                    }
                    Ok(SubprocessOutcome::Cancelled) => {
                        break 'compute None;
                    }
                    Err(e) => {
                        break 'compute Some(RebaseResult::Failure {
                            base_branch,
                            reason: format!("git fetch could not run: {e}"),
                            conflicts_attempted: false,
                            activity_log_error: None,
                        });
                    }
                }

                // Cancellation check between phase 2 and phase 3:
                // `git fetch` is the longest blocking step in the
                // pre-spawn window, so the gate may have been cancelled
                // while we were waiting on the network. Bailing here
                // avoids starting the MCP server, writing the temp
                // config, and spawning the harness child for nothing.
                if cancelled.load(Ordering::SeqCst) {
                    break 'compute None;
                }

                let _ = tx.send(RebaseGateMessage::Progress(
                    "Fetched. Asking the assistant to rebase...".into(),
                ));

                // === Phase 3: launch headless harness with workbridge MCP ===
                //
                // The MCP server gets its OWN local sender/receiver pair so
                // the spawning thread can drain `workbridge_log_event` /
                // `workbridge_report_progress` calls in real time and
                // translate them into `RebaseGateMessage::Progress`. The
                // server's tx is intentionally NOT `self.mcp_tx` because
                // routing the rebase gate's progress through the main
                // dispatch loop would mix it with unrelated events and
                // require new branches in the main `McpEvent` handler.
                let (gate_mcp_tx, gate_mcp_rx) = crossbeam_channel::unbounded::<McpEvent>();
                let gate_socket = crate::mcp::socket_path_for_session();
                match crate::mcp::McpSocketServer::start(
                    gate_socket.clone(),
                    serde_json::to_string(&wi_id_clone).unwrap_or_default(),
                    String::new(),
                    serde_json::json!({
                        "work_item_id": serde_json::to_string(&wi_id_clone).unwrap_or_default(),
                        "repo_path": worktree_path.display().to_string(),
                        "branch": branch,
                        "base_branch": base_branch,
                    })
                    .to_string(),
                    None,
                    gate_mcp_tx,
                    false, // read_only=false: harness must call workbridge_log_event for live progress
                ) {
                    Ok(s) => {
                        gate_server = Some(s);
                    }
                    Err(e) => {
                        break 'compute Some(RebaseResult::Failure {
                            base_branch,
                            reason: format!("rebase gate: could not start MCP server: {e}"),
                            conflicts_attempted: false,
                            activity_log_error: None,
                        });
                    }
                }

                // Cancellation check after starting the MCP server.
                // The post-block cleanup will drop `gate_server`.
                if cancelled.load(Ordering::SeqCst) {
                    break 'compute None;
                }

                let exe_path = match std::env::current_exe() {
                    Ok(p) => p,
                    Err(e) => {
                        break 'compute Some(RebaseResult::Failure {
                            base_branch,
                            reason: format!("rebase gate: could not resolve exe path: {e}"),
                            conflicts_attempted: false,
                            activity_log_error: None,
                        });
                    }
                };
                let mcp_config = crate::mcp::build_mcp_config(&exe_path, &gate_socket, &[]);
                let path = crate::side_effects::paths::temp_dir().join(format!(
                    "workbridge-rebase-mcp-{}.json",
                    uuid::Uuid::new_v4()
                ));
                if let Err(e) = std::fs::write(&path, &mcp_config) {
                    break 'compute Some(RebaseResult::Failure {
                        base_branch,
                        reason: format!("rebase gate: could not write MCP config: {e}"),
                        conflicts_attempted: false,
                        activity_log_error: None,
                    });
                }
                config_path = Some(path);
                // Structured bridge spec for Codex's per-field `-c`
                // overrides. Claude ignores it; see
                // `agent_backend::McpBridgeSpec`.
                let rebase_bridge = crate::agent_backend::McpBridgeSpec {
                    name: "workbridge".to_string(),
                    command: exe_path,
                    args: vec![
                        "--mcp-bridge".to_string(),
                        "--socket".to_string(),
                        gate_socket.to_string_lossy().into_owned(),
                    ],
                };

                // Cancellation check immediately before spawning the
                // harness sub-thread. This is the last cheap point
                // where we can avoid spawning the harness child
                // entirely; once the sub-thread runs `Command::spawn`,
                // the harness is alive and the kill must go through
                // `child_pid`. The post-block cleanup handles
                // dropping `gate_server` and removing `config_path`.
                if cancelled.load(Ordering::SeqCst) {
                    break 'compute None;
                }

                let prompt = format!(
                    "You are running inside a workbridge rebase gate. Your job is to rebase \
                 the current branch (`{branch}`) onto `origin/{base_branch}` in this \
                 working directory and resolve any conflicts that arise.\n\n\
                 Steps:\n\
                 1. Run `git rebase origin/{base_branch}`.\n\
                 2. If conflicts appear, inspect the conflicted files, resolve them \
                    in place (preferring the semantics of `{branch}` while keeping \
                    upstream changes intact), `git add` the resolved files, and run \
                    `git rebase --continue`. Repeat until the rebase completes.\n\
                 3. If you cannot resolve the conflicts, run `git rebase --abort` so \
                    the worktree is left clean.\n\
                 4. Do NOT run `git push` under any circumstances. The user will \
                    push manually.\n\n\
                 As you work, call the `workbridge_log_event` MCP tool with \
                 `event_type='rebase_progress'` and a `payload` object containing a \
                 `message` field describing what you are about to do. This streams \
                 progress to the workbridge UI.\n\n\
                 When you finish, respond with a single JSON object on stdout (no \
                 prose) of the shape:\n\
                 {{\"success\": <bool>, \"conflicts_resolved\": <bool>, \"detail\": \
                 <string>}}\n\n\
                 - `success` = true if the branch is now rebased onto \
                 `origin/{base_branch}`.\n\
                 - `conflicts_resolved` = true if you had to resolve at least one \
                 conflict before finishing.\n\
                 - `detail` = a human-readable one-line summary.\n\n\
                 Workbridge writes its own activity log entry for the rebase \
                 outcome (success or failure) after this process exits, so do NOT \
                 call `workbridge_set_status` to leave a record - the work item is \
                 already in `Implementing` and the activity log entry below is the \
                 audit trail."
                );

                let json_schema = r#"{"type":"object","properties":{"success":{"type":"boolean"},"conflicts_resolved":{"type":"boolean"},"detail":{"type":"string"}},"required":["success","detail"]}"#;

                // Spawn the harness child in a sub-thread so we can
                // drain gate_mcp_rx for live progress events while
                // waiting for the child to exit. The `current_dir`
                // MUST be the work item's worktree path (each git
                // worktree has its own HEAD). The sub-thread uses
                // `run_cancellable` which handles process-group
                // isolation, PID stashing, and the "stash first,
                // check second" ordering contract; see the helper's
                // doc comment for the full rationale.
                let (output_tx, output_rx) = crossbeam_channel::bounded::<SubprocessOutcome>(1);
                {
                    // `config_path` is unconditionally set by the
                    // `config_path = Some(path)` assignment a few
                    // lines above, before this block runs; the
                    // `as_ref()? ... .clone()` dance lets the code
                    // avoid a restriction-lint `expect()` without
                    // changing behaviour (on the impossible None
                    // path we just skip the spawn with an error).
                    let Some(config_path) = config_path.clone() else {
                        break 'compute Some(RebaseResult::Failure {
                            base_branch,
                            reason: "rebase gate: config path missing".into(),
                            conflicts_attempted: false,
                            activity_log_error: None,
                        });
                    };
                    let worktree_path = worktree_path.clone();
                    let child_pid = Arc::clone(&child_pid);
                    let cancelled = Arc::clone(&cancelled);
                    let agent_backend = Arc::clone(&agent_backend);
                    let bridge = rebase_bridge;
                    let extra_bridges = rebase_extra_bridges.clone();
                    std::thread::spawn(move || {
                        let rw_cfg = crate::agent_backend::ReviewGateSpawnConfig {
                            system_prompt: "",
                            initial_prompt: &prompt,
                            json_schema,
                            mcp_config_path: &config_path,
                            mcp_bridge: &bridge,
                            extra_bridges: &extra_bridges,
                        };
                        let argv = agent_backend.build_headless_rw_command(&rw_cfg);
                        let mut cmd = std::process::Command::new(agent_backend.command_name());
                        cmd.args(&argv)
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .current_dir(&worktree_path);

                        match run_cancellable(&mut cmd, &child_pid, &cancelled) {
                            Ok(outcome) => {
                                let _ = output_tx.send(outcome);
                            }
                            Err(e) => {
                                // Spawn or wait failed; wrap in the
                                // Completed variant with a failed output
                                // so the outer thread sees it as a
                                // harness error.
                                let _ = output_tx.send(SubprocessOutcome::Completed(
                                    std::process::Output {
                                        status: std::process::ExitStatus::default(),
                                        stdout: Vec::new(),
                                        stderr: format!(
                                            "could not run {}: {e}",
                                            agent_backend.command_name()
                                        )
                                        .into_bytes(),
                                    },
                                ));
                            }
                        }
                    });
                }

                // `conflicts_attempted_observed` is declared OUTSIDE the
                // labeled block (before the block starts) so this select-
                // loop can mutate it while still being inside the block.
                // Reset to a known state here (in case any future caller
                // factors out the block - currently this is the only
                // place that mutates it).
                let final_output = loop {
                    crossbeam_channel::select! {
                        recv(gate_mcp_rx) -> evt => {
                            match evt {
                                Ok(McpEvent::ReviewGateProgress { message, .. }) => {
                                    let _ = tx.send(RebaseGateMessage::Progress(message));
                                }
                                Ok(McpEvent::LogEvent { event_type, payload, .. }) => {
                                    if event_type == "rebase_progress" {
                                        let msg = payload
                                            .get("message")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("...")
                                            .to_string();
                                        if msg.to_lowercase().contains("conflict") {
                                            conflicts_attempted_observed = true;
                                        }
                                        let _ = tx.send(RebaseGateMessage::Progress(msg));
                                    }
                                }
                                Ok(_) | Err(_) => {
                                    // `Ok(_)`: other MCP events (StatusUpdate,
                                    // SetPlan, SetTitle, ...) are intentionally
                                    // ignored. The rebase gate writes its own
                                    // activity log entry from `poll_rebase_gate`
                                    // after the harness exits, so the prompt does
                                    // not ask the harness to call
                                    // `workbridge_set_status` and we do not
                                    // forward stray events here. Forwarding would
                                    // let a misbehaving harness rename the work
                                    // item or overwrite its plan as a side effect
                                    // of running a rebase.
                                    //
                                    // `Err(_)`: channel disconnected - server
                                    // gone. Continue waiting for the child to
                                    // exit; the output_rx arm below will fire
                                    // shortly.
                                }
                            }
                        }
                        recv(output_rx) -> output_result => {
                            break output_result;
                        }
                    }
                };

                // === Phase 5: build result from harness output ===
                //
                // This is the final break of the 'compute block on
                // the harness happy path. Pre-harness early failures
                // have already broken with their own `Some(Failure)`
                // values above; cancellation paths break with `None`.
                // Cleanup of `gate_server` and `config_path` happens
                // AFTER the block, uniformly for every break path.
                // Handle the Cancelled variant from the harness
                // sub-thread: if the gate was torn down while the
                // harness was running, `run_cancellable` already
                // killed the process group and returned Cancelled.
                // Break with None so the post-block code skips the
                // audit append and result send.
                let harness_output = match final_output {
                    Ok(SubprocessOutcome::Cancelled) => break 'compute None,
                    Ok(SubprocessOutcome::Completed(output)) => Ok(output),
                    Err(e) => Err(e),
                };

                Some(match harness_output {
                    Ok(output) if output.status.success() => {
                        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        match serde_json::from_str::<serde_json::Value>(&stdout) {
                            Ok(envelope) => {
                                let structured = &envelope["structured_output"];
                                let success = structured["success"].as_bool().unwrap_or(false);
                                let conflicts_resolved =
                                    structured["conflicts_resolved"].as_bool().unwrap_or(false);
                                let detail =
                                    structured["detail"].as_str().unwrap_or("").to_string();
                                if success {
                                    // Verify the harness's success claim
                                    // against local git state before
                                    // surfacing it to the user. The harness
                                    // can hallucinate, run the wrong
                                    // command, or emit a stale envelope; in
                                    // any of those cases the worktree's
                                    // HEAD will not actually contain
                                    // `origin/<base_branch>`. The
                                    // user-facing-claim rule in CLAUDE.md
                                    // requires that any "it happened"
                                    // status that the code can verify
                                    // locally MUST be verified before
                                    // rendering. `git merge-base
                                    // --is-ancestor A B` exits 0 iff A is
                                    // an ancestor of B and 1 otherwise; any
                                    // other exit is an error and is also
                                    // treated as "did not land".
                                    let ancestry_ok = match crate::worktree_service::git_command()
                                        .arg("-C")
                                        .arg(&worktree_path)
                                        .args([
                                            "merge-base",
                                            "--is-ancestor",
                                            &format!("origin/{base_branch}"),
                                            "HEAD",
                                        ])
                                        .output()
                                    {
                                        Ok(o) => o.status.success(),
                                        Err(_) => false,
                                    };
                                    // Also check that no rebase is
                                    // still in progress. During a
                                    // conflicted rebase HEAD has
                                    // already advanced past origin/
                                    // <base> so the ancestry check
                                    // passes, but REBASE_HEAD exists
                                    // while git is waiting for
                                    // conflict resolution. If the
                                    // harness hallucinated success
                                    // while leaving the worktree
                                    // mid-rebase, this catches it.
                                    let rebase_in_progress = crate::worktree_service::git_command()
                                        .arg("-C")
                                        .arg(&worktree_path)
                                        .args(["rev-parse", "--verify", "--quiet", "REBASE_HEAD"])
                                        .output()
                                        .is_ok_and(|o| o.status.success());
                                    if ancestry_ok && !rebase_in_progress {
                                        RebaseResult::Success {
                                            base_branch,
                                            conflicts_resolved,
                                            activity_log_error: None,
                                        }
                                    } else if !ancestry_ok {
                                        RebaseResult::Failure {
                                            base_branch: base_branch.clone(),
                                            reason: format!(
                                                "harness reported success but origin/{base_branch} is not an ancestor of HEAD"
                                            ),
                                            conflicts_attempted: conflicts_resolved
                                                || conflicts_attempted_observed,
                                            activity_log_error: None,
                                        }
                                    } else {
                                        // ancestry_ok but rebase_in_progress:
                                        // REBASE_HEAD exists, meaning git is
                                        // waiting for conflict resolution.
                                        // The harness left the worktree
                                        // mid-rebase.
                                        RebaseResult::Failure {
                                            base_branch,
                                            reason: "harness reported success but a rebase is still in progress (REBASE_HEAD exists)".into(),
                                            conflicts_attempted: true,
                                            activity_log_error: None,
                                        }
                                    }
                                } else {
                                    RebaseResult::Failure {
                                        base_branch,
                                        reason: if detail.is_empty() {
                                            "harness reported failure".into()
                                        } else {
                                            detail
                                        },
                                        conflicts_attempted: conflicts_resolved
                                            || conflicts_attempted_observed,
                                        activity_log_error: None,
                                    }
                                }
                            }
                            Err(e) => RebaseResult::Failure {
                                base_branch,
                                reason: format!("rebase gate: invalid JSON envelope: {e}"),
                                conflicts_attempted: conflicts_attempted_observed,
                                activity_log_error: None,
                            },
                        }
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                        RebaseResult::Failure {
                            base_branch,
                            reason: format!("harness exited with error: {}", stderr.trim()),
                            conflicts_attempted: conflicts_attempted_observed,
                            activity_log_error: None,
                        }
                    }
                    Err(e) => RebaseResult::Failure {
                        base_branch,
                        reason: format!("rebase gate: harness thread disconnected: {e}"),
                        conflicts_attempted: conflicts_attempted_observed,
                        activity_log_error: None,
                    },
                })
            };

            // === Post-'compute cleanup ===
            //
            // Drop the MCP server and remove the temp config file.
            // Both are wrapped in `Option<>` so this runs uniformly
            // regardless of which break arm exited the block: a
            // pre-server failure leaves both `None` (no-op cleanup),
            // a post-server pre-config failure drops the server but
            // skips the rm, and a successful run drops both. The
            // server MUST be alive while the harness child is
            // running - that constraint is satisfied by the harness
            // sub-thread spawning and waiting INSIDE the block, so
            // by the time we reach this cleanup the harness has
            // already exited or we have already broken with an
            // early failure that did not reach the spawn.
            if let Some(server) = gate_server.take() {
                drop(server);
            }
            if let Some(path) = config_path.take()
                && let Err(_e) = std::fs::remove_file(&path)
            {
                // Best-effort cleanup: the file is in `$TMPDIR` and
                // the OS will clean it up eventually. Logging would
                // be misleading because the typical "error" here is
                // ENOENT after a normal harness run that already
                // consumed the config.
            }

            // Convert the labeled-block result into either a
            // concrete `RebaseResult` (audit + send below) or a bare
            // `return` for the cancellation path. The `None` case
            // means a `cancelled` check inside the block fired; the
            // cancellation contract in C10 says cancelled gates do
            // NOT write to the activity log and do NOT send a
            // result through `tx`.
            let Some(result) = computed else { return };

            // If the result is a Failure, clean up any in-progress
            // rebase the harness may have left behind. The harness
            // is instructed to `git rebase --abort` on give-up, but
            // if it crashed, was killed, or hallucinated success
            // while REBASE_HEAD still exists, the worktree is left
            // mid-rebase with conflict markers and a locked index.
            // Running `git rebase --abort` here is idempotent: if
            // no rebase is in progress it exits non-zero with "No
            // rebase in progress?" and does nothing. The abort goes
            // through `run_cancellable` so it is also killable if
            // the gate is torn down while the abort is in flight
            // (the worktree is about to be removed anyway in that
            // case, so a partial abort is harmless).
            if matches!(&result, RebaseResult::Failure { .. }) {
                let _ = run_cancellable(
                    crate::worktree_service::git_command()
                        .arg("-C")
                        .arg(&worktree_path)
                        .args(["rebase", "--abort"]),
                    &child_pid,
                    &cancelled,
                );
            }

            // Early-out on cancellation: skip the append entirely
            // and do not send the result. This is a fast-path
            // optimization - the structural guarantee that a
            // cancelled gate cannot create an orphan active log
            // comes from `append_activity_existing_only` below,
            // NOT from this check. The check still matters because
            // it avoids doing the backend work (and sending a
            // result the dropped receiver would never read) when
            // we already know the gate is gone.
            if cancelled.load(Ordering::SeqCst) {
                return;
            }

            // Build the activity log entry from the result and
            // append it via the backend on THIS background thread.
            // The append used to live in `poll_rebase_gate` (i.e. on
            // the UI thread) which violated the absolute blocking-
            // I/O invariant: a slow filesystem could freeze the TUI.
            // Doing it here keeps the UI thread out of the file
            // write entirely.
            //
            // CRITICAL: we call `append_activity_existing_only`, NOT
            // `append_activity`. The former opens with
            // `OpenOptions::create(false)` so a `backend.delete` +
            // `archive_activity_log` that races the append cannot
            // recreate an orphan active activity log for a deleted
            // item. POSIX semantics: if the main thread renames
            // active -> archive AFTER we open the fd but BEFORE we
            // write, the write lands in the archived file because
            // the fd still points at the same inode. If the rename
            // happens before we open, the open returns `ENOENT` and
            // the method returns `Ok(false)`, which we handle as
            // "the item was deleted while we were finishing up - no
            // audit trail to write, no error to surface". This is
            // the load-bearing structural fix for the
            // "cancellation must precede destruction" rule; the
            // earlier cancellation check is now just an
            // optimization on top of it. Any other error
            // (permission, I/O) is captured into
            // `activity_log_error` and surfaced via the result.
            let activity_entry = match &result {
                RebaseResult::Success {
                    base_branch,
                    conflicts_resolved,
                    ..
                } => ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "rebase_completed".to_string(),
                    payload: serde_json::json!({
                        "base_branch": base_branch,
                        "conflicts_resolved": conflicts_resolved,
                        "source": "rebase_gate",
                    }),
                },
                RebaseResult::Failure {
                    base_branch,
                    reason,
                    conflicts_attempted,
                    ..
                } => ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "rebase_failed".to_string(),
                    payload: serde_json::json!({
                        "base_branch": base_branch,
                        "reason": reason,
                        "conflicts_attempted": conflicts_attempted,
                        "source": "rebase_gate",
                    }),
                },
            };
            let activity_log_error =
                match backend.append_activity_existing_only(&wi_id_clone, &activity_entry) {
                    // Appended successfully - either to the active log
                    // or (under a concurrent archive rename) to the
                    // now-archived file via the still-valid fd.
                    Ok(true) => None,
                    // Active log was missing when we tried to open it:
                    // the work item was deleted and its log archived
                    // between the cancellation check above and this
                    // append. Do NOT surface this as an error - the
                    // item is gone, so there is nothing to audit, and
                    // the result send below is a silent no-op because
                    // `drop_rebase_gate` already dropped the receiver.
                    // Returning here also prevents sending a spurious
                    // "activity log missing" suffix onto a status
                    // message that no UI will ever see.
                    Ok(false) => return,
                    Err(e) => Some(e.to_string()),
                };

            // Re-attach the activity_log_error to the appropriate
            // variant. The verbosity is intentional: keeping the
            // field structural (rather than passing it via a side
            // channel) means `poll_rebase_gate` cannot forget to
            // surface it in the status message.
            let result = match result {
                RebaseResult::Success {
                    base_branch,
                    conflicts_resolved,
                    ..
                } => RebaseResult::Success {
                    base_branch,
                    conflicts_resolved,
                    activity_log_error,
                },
                RebaseResult::Failure {
                    base_branch,
                    reason,
                    conflicts_attempted,
                    ..
                } => RebaseResult::Failure {
                    base_branch,
                    reason,
                    conflicts_attempted,
                    activity_log_error,
                },
            };

            let _ = tx.send(RebaseGateMessage::Result(result));
        });
    }

    /// Drop a rebase gate and end its status-bar activity. Mirrors
    /// `drop_review_gate`. The cancellation flag and the harness
    /// process-group SIGKILL now live in `Drop for RebaseGateState`,
    /// so removing the entry from `rebase_gates` is sufficient on its
    /// own to signal the background thread and kill the harness
    /// tree. This helper still exists because it ALSO ends the
    /// status-bar activity and releases the `UserActionKey::RebaseOnMain`
    /// single-flight slot - both of which need `App` access and so
    /// cannot live inside `Drop`. New code paths SHOULD prefer this
    /// helper (or the higher-level
    /// `App::abort_background_ops_for_work_item` that wraps it) over
    /// raw `rebase_gates.remove(...)`, but the structural insurance
    /// in `Drop` means a forgotten helper call is "leaked spinner /
    /// debounce slot" rather than "runaway harness against deleted
    /// worktree".
    ///
    /// Single-flight guard: the helper only ends the
    /// `UserActionKey::RebaseOnMain` user action if the slot is
    /// currently owned by `wi_id`. Without this guard, dropping a
    /// gate for one work item could clear the global single-flight
    /// slot while a different work item still owns it, admitting an
    /// overlapping rebase and breaking the `RebaseOnMain` invariant.
    fn drop_rebase_gate(&mut self, wi_id: &WorkItemId) {
        let removed = self.rebase_gates.remove(wi_id);
        let slot_owner_matches =
            self.user_action_work_item(&UserActionKey::RebaseOnMain) == Some(wi_id);

        if let Some(state) = removed {
            // Cancellation + killpg happen in `Drop for
            // RebaseGateState` when `state` falls out of scope at
            // the end of this block. We do NOT need to manually
            // signal them here.
            self.end_activity(state.activity);
        }

        // Only clear the user-action slot if it is owned by the
        // work item we are dropping. See the docstring above.
        if slot_owner_matches {
            self.end_user_action(&UserActionKey::RebaseOnMain);
        }
    }

    /// Cancel every long-running background operation associated
    /// with `wi_id` BEFORE the work item's backing data is
    /// destroyed. This is the entrypoint that
    /// `delete_work_item_by_id` (and any future
    /// resource-destruction site) MUST call before doing anything
    /// destructive to the work item's backend record, activity
    /// log, worktree, or in-memory state.
    ///
    /// The architectural rule this helper enforces is **"cancellation
    /// must precede destruction"**. The motivating failure mode: the
    /// rebase gate's background thread writes a `rebase_completed`
    /// or `rebase_failed` entry to the work item's activity log
    /// directly (background-thread write, off the UI thread per the
    /// blocking-I/O invariant). If `backend.delete` archives the
    /// active activity log BEFORE the background thread is told to
    /// stop, there is a window where the thread can still call
    /// `append_activity` and recreate an orphan active log via
    /// `OpenOptions::create(true)`. Routing every destructive call
    /// site through this helper closes that window structurally:
    /// after this returns, the gate has been removed from the map
    /// (so its `Drop` impl has set the `cancelled` flag and `SIGKILLed`
    /// the harness group) and the bg thread will exit on its next
    /// phase check without writing.
    ///
    /// Today this only cancels the rebase gate. Other long-running
    /// background ops with similar "writes after destruction"
    /// hazards (none currently) should be added here as a single
    /// extension point so future cleanup sites pick them up
    /// automatically.
    fn abort_background_ops_for_work_item(&mut self, wi_id: &WorkItemId) {
        self.drop_rebase_gate(wi_id);
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
            let Some(gate) = self.review_gates.get(&wi_id) else {
                continue;
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
                    .map_or(ReviewGateOrigin::Mcp, |g| g.origin);
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

            let Some(result) = result else { continue };

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
                .is_some_and(|w| {
                    w.status == WorkItemStatus::Implementing || w.status == WorkItemStatus::Blocked
                });

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
                    .map_or(WorkItemStatus::Implementing, |w| w.status);

                self.apply_stage_change(
                    &wi_id,
                    current_status,
                    WorkItemStatus::Review,
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

    /// Poll all async rebase gates for results. Called on each timer
    /// tick from `salsa.rs` next to `poll_review_gate`.
    ///
    /// On a final `Result`:
    ///
    /// - `Success` -> set a status message naming the base branch and
    ///   drop the gate. `drop_rebase_gate` clears the user-action guard
    ///   slot so a follow-up `m` press is admitted right away.
    /// - `Failure` -> set a status message with the reason and drop
    ///   the gate. The worktree is left in whatever state the harness
    ///   leaves it; the harness is responsible for `git rebase --abort`
    ///   on the give-up path. We do NOT shell out to `git status`
    ///   here - the next fetcher tick will refresh the cached
    ///   `git_state` and the indicators will re-render.
    pub fn poll_rebase_gate(&mut self) {
        if self.rebase_gates.is_empty() {
            return;
        }

        let wi_ids: Vec<WorkItemId> = self.rebase_gates.keys().cloned().collect();

        for wi_id in wi_ids {
            let Some(gate) = self.rebase_gates.get(&wi_id) else {
                continue;
            };

            let mut last_progress: Option<String> = None;
            let mut result: Option<RebaseResult> = None;
            let mut disconnected = false;

            loop {
                match gate.rx.try_recv() {
                    Ok(RebaseGateMessage::Progress(text)) => {
                        last_progress = Some(text);
                    }
                    Ok(RebaseGateMessage::Result(r)) => {
                        result = Some(r);
                        break;
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if let Some(progress) = last_progress
                && let Some(gate) = self.rebase_gates.get_mut(&wi_id)
            {
                gate.progress = Some(progress);
            }

            if disconnected && result.is_none() {
                self.drop_rebase_gate(&wi_id);
                self.status_message =
                    Some("Rebase gate: background thread exited unexpectedly".into());
                continue;
            }

            let Some(result) = result else { continue };

            self.drop_rebase_gate(&wi_id);

            // The activity log entry is written by the background
            // thread itself (see `spawn_rebase_gate`), so this poll
            // path does NOT touch `backend.append_activity` - that
            // would be blocking I/O on the UI thread. If the
            // background-thread append failed, the error string
            // travels back inside `RebaseResult::*::activity_log_error`
            // and we surface it as a suffix to the status message
            // so the user can see the audit trail did not land.
            let (mut status_message, activity_log_error) = match result {
                RebaseResult::Success {
                    base_branch,
                    conflicts_resolved,
                    activity_log_error,
                } => {
                    let msg = if conflicts_resolved {
                        format!("Rebased onto origin/{base_branch} (conflicts resolved by harness)")
                    } else {
                        format!("Rebased onto origin/{base_branch}")
                    };
                    (msg, activity_log_error)
                }
                RebaseResult::Failure {
                    base_branch,
                    reason,
                    conflicts_attempted,
                    activity_log_error,
                } => {
                    let msg = if conflicts_attempted {
                        format!(
                            "Rebase onto origin/{base_branch} failed after conflict resolution: {reason}"
                        )
                    } else {
                        format!("Rebase onto origin/{base_branch} failed: {reason}")
                    };
                    (msg, activity_log_error)
                }
            };
            if let Some(err) = activity_log_error {
                use std::fmt::Write as _;
                let _ = write!(status_message, " (activity log error: {err})");
            }
            self.status_message = Some(status_message);
        }
    }

    /// Get the `SessionEntry` for the currently selected work item, if any.
    pub fn active_session_entry(&self) -> Option<&SessionEntry> {
        let work_item_id = self.selected_work_item_id()?;
        let key = self.session_key_for(&work_item_id)?;
        self.sessions.get(&key)
    }

    /// Get a mutable reference to the `SessionEntry` for the currently selected
    /// work item. Needed by mouse scroll handling to update `scrollback_offset`.
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
    /// 1. If an in-flight preparation worker is still running, cancel
    ///    it: take the pending entry, end its spinner, and collect
    ///    the `config_path` it committed to so we can clean it up
    ///    below. Dropping the receiver makes the worker's
    ///    `tx.send(...)` a silent no-op; the `McpSocketServer` and
    ///    `Session` handles the worker eventually creates get
    ///    dropped with the result on scope exit, which stops the
    ///    accept loop, removes the socket file, and force-kills
    ///    the child process group.
    /// 2. SIGTERM + 50 ms grace + SIGKILL the `claude` child process
    ///    via `Session::kill` so no zombie survives.
    /// 3. Drop the `SessionEntry`; `Session::Drop` joins the reader thread.
    /// 4. Drop the MCP server (same as `cleanup_all_mcp`).
    /// 5. Remove the temp MCP config file on a background thread via
    ///    `spawn_agent_file_cleanup` - `std::fs::remove_file` blocks
    ///    on the filesystem and is forbidden on the UI thread per
    ///    `docs/UI.md` "Blocking I/O Prohibition". Both the
    ///    durable `global_mcp_config_path` (live session) AND the
    ///    pending-worker's `config_path` (cancelled preparation) are
    ///    fed into the same cleanup call.
    /// 6. Drop any keystrokes queued for the old session's PTY so they
    ///    don't leak into the next session on reopen.
    fn teardown_global_session(&mut self) {
        // Cancel any in-flight preparation. Take the pending entry so
        // we can (a) end its spinner without leaking it, (b) collect
        // the `config_path` it committed to so we can route the
        // cleanup through `spawn_agent_file_cleanup` alongside the
        // durable-session config path below, and (c) flip the
        // shared `cancelled` flag so the worker bails out of its
        // remaining blocking operations before they run. The
        // worker is left running; when its `tx.send(...)` fires on
        // a dropped receiver the result is silently discarded (the
        // `Session` and `McpSocketServer` handles run their own
        // `Drop` impls and clean themselves up).
        let mut files_to_clean: Vec<PathBuf> = Vec::new();
        if let Some(pending) = self.global_session_open_pending.take() {
            pending.cancelled.store(true, Ordering::Release);
            // If the worker already sent a result, drain it so
            // Session::Drop and McpSocketServer::Drop do not run
            // on the UI thread when the receiver is dropped.
            if let Ok(result) = pending.rx.try_recv() {
                if let Some(server) = result.mcp_server {
                    self.drop_mcp_server_off_thread(server);
                }
                if let Some(session) = result.session {
                    std::thread::spawn(move || drop(session));
                }
            }
            self.end_activity(pending.activity);
            files_to_clean.push(pending.config_path);
        }

        if let Some(ref mut entry) = self.global_session
            && let Some(ref mut session) = entry.session
        {
            session.kill();
        }
        // Drop Session off the UI thread: its Drop can join the
        // reader thread and kill the child.
        if let Some(entry) = self.global_session.take() {
            std::thread::spawn(move || drop(entry));
        }
        // Drop MCP server off the UI thread: its Drop unlinks the
        // socket file.
        if let Some(server) = self.global_mcp_server.take() {
            self.drop_mcp_server_off_thread(server);
        }
        if let Some(path) = self.global_mcp_config_path.take() {
            files_to_clean.push(path);
        }
        self.spawn_agent_file_cleanup(files_to_clean);
        self.pending_global_pty_bytes.clear();
    }

    /// Spawn the global assistant agent session.
    ///
    /// Goes through the pluggable `AgentBackend` trait - this file does
    /// not hard-code any harness-specific flags. See
    /// `docs/harness-contract.md` "Known Spawn Sites" (Global row) and
    /// C2 for the scratch cwd rationale.
    ///
    /// The UI thread only runs pure-CPU work in this function: it
    /// refreshes the shared MCP context, builds the system prompt
    /// from the cached repo list, clones the handful of Arcs the
    /// worker needs, and then spawns a background thread that runs
    /// ALL of the blocking work (`McpSocketServer::start_global`,
    /// the `--mcp-config` tempfile `std::fs::write`, the scratch
    /// `std::fs::create_dir_all`, and `Session::spawn` itself). The
    /// worker returns a `GlobalSessionPrepResult` through the
    /// `GlobalSessionOpenPending` receiver; `poll_global_session_open`
    /// drains it on the next background tick and moves the handles
    /// into the durable `App::global_*` fields. See `docs/UI.md`
    /// "Blocking I/O Prohibition" for why this split is mandatory.
    fn spawn_global_session(&mut self) {
        // If a previous preparation is still in flight, cancel it
        // first so we don't end up with two workers racing each
        // other on resource ownership. `teardown_global_session` is
        // the canonical cleanup path (it also routes the config
        // file through `spawn_agent_file_cleanup`), and
        // `toggle_global_drawer` already calls teardown before
        // spawning, so this branch is defence in depth.
        if self.global_session_open_pending.is_some() {
            self.teardown_global_session();
        }

        // Refresh the shared MCP context on the UI thread (pure CPU -
        // the context lives behind an `Arc<Mutex<String>>` that the
        // background worker's accept loop reads by reference, and
        // the dynamic state we pull from comes straight from the
        // in-memory repo / work-item caches).
        self.refresh_global_mcp_context();

        // Build the repo list and system prompt here (pure CPU on
        // UI-thread state).
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

        // Compute the temp `--mcp-config` path UP FRONT on the UI
        // thread so the main thread (not the worker) owns the
        // filename and can route cleanup through
        // `spawn_agent_file_cleanup` on cancellation. The filename
        // is per-call unique - PID for cross-process clarity + UUID
        // so two concurrent workers under rapid Ctrl+G cannot
        // collide on a shared path. Under the previous PID-only
        // scheme, teardown + respawn + the old worker finishing
        // late would delete the new worker's live config file out
        // from under it.
        let config_path = crate::side_effects::paths::temp_dir().join(format!(
            "workbridge-global-mcp-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));

        // Shared cancellation flag. `teardown_global_session` and
        // `cleanup_all_mcp` set it via `Ordering::Release`; the
        // worker checks it via `Ordering::Acquire` before each
        // blocking operation and bails out early. See the matching
        // flag on the work-item session path
        // (`SessionOpenPending::cancelled`) for the race-window
        // caveat.
        let cancelled = Arc::new(AtomicBool::new(false));

        // Resolve the global-assistant harness from config. If unset,
        // we should never have reached this function - `handle_ctrl_g`
        // opens the first-run modal in that case and only calls
        // `toggle_global_drawer` after a pick. Abort loudly (toast +
        // close drawer) rather than silently falling back to
        // `self.agent_backend`: CLAUDE.md has an [ABSOLUTE] rule
        // against silent default-harness substitution, and this is
        // the last line of defence for any future bypass of
        // `handle_ctrl_g`'s guard.
        let Some(kind) = self.global_assistant_harness_kind() else {
            self.global_drawer_open = false;
            self.focus = self.pre_drawer_focus;
            self.push_toast(
                "Cannot open global assistant: no harness configured. Press Ctrl+G again to pick one."
                    .into(),
            );
            return;
        };
        let agent_backend: Arc<dyn AgentBackend> = agent_backend::backend_for_kind(kind);

        // Capture everything the worker needs. All Send + Sync.
        let mcp_context_shared = Arc::clone(&self.global_mcp_context);
        let mcp_tx = self.mcp_tx.clone();
        let pane_cols = self.global_pane_cols;
        let pane_rows = self.global_pane_rows;
        let pre_drawer_focus = self.pre_drawer_focus;
        let worker_config_path = config_path.clone();
        let worker_cancelled = Arc::clone(&cancelled);

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            // Cancellation check before any blocking operation. If
            // the main thread cancelled this spawn already (rapid
            // Ctrl+G toggle, shutdown), bail out before the socket
            // bind so no socket file is ever created.
            if worker_cancelled.load(Ordering::Acquire) {
                return;
            }

            // Phase A: start the global MCP socket server. Socket
            // bind + stale-file remove + accept-loop thread spawn
            // all live here.
            let socket_path = crate::mcp::socket_path_for_session();
            let mcp_server =
                match McpSocketServer::start_global(socket_path, mcp_context_shared, mcp_tx) {
                    Ok(server) => server,
                    Err(e) => {
                        let _ = tx.send(GlobalSessionPrepResult {
                            mcp_server: None,
                            session: None,
                            error: Some(format!("Global assistant MCP error: {e}")),
                        });
                        return;
                    }
                };

            if worker_cancelled.load(Ordering::Acquire) {
                // Drop the server we just started (its Drop impl
                // stops the accept loop and removes the socket
                // file) and exit without writing the tempfile.
                drop(mcp_server);
                return;
            }

            // Phase B: resolve exe path and build MCP config bytes.
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(GlobalSessionPrepResult {
                        mcp_server: Some(mcp_server),
                        session: None,
                        error: Some(format!(
                            "Global assistant: cannot resolve executable path: {e}"
                        )),
                    });
                    return;
                }
            };
            let mcp_config = crate::mcp::build_mcp_config(&exe, &mcp_server.socket_path, &[]);
            let global_bridge = crate::agent_backend::McpBridgeSpec {
                name: "workbridge".to_string(),
                command: exe,
                args: vec![
                    "--mcp-bridge".to_string(),
                    "--socket".to_string(),
                    mcp_server.socket_path.to_string_lossy().into_owned(),
                ],
            };

            // Phase C: write the temp `--mcp-config` file at the
            // path the UI thread already committed to. The path is
            // tracked in `GlobalSessionOpenPending::config_path`, so
            // `teardown_global_session` can clean it up via
            // `spawn_agent_file_cleanup` if the drawer closes
            // mid-flight - the worker itself never needs to remove
            // the file on a cancellation path. Last cancellation
            // check right before the write; covers the common case
            // where the user toggles the drawer while the worker
            // is between Phase A and Phase C.
            if worker_cancelled.load(Ordering::Acquire) {
                drop(mcp_server);
                return;
            }
            if let Err(e) = std::fs::write(&worker_config_path, &mcp_config) {
                let _ = tx.send(GlobalSessionPrepResult {
                    mcp_server: Some(mcp_server),
                    session: None,
                    error: Some(format!("Global assistant MCP config error: {e}")),
                });
                return;
            }

            // Phase D: ensure the scratch cwd exists. We deliberately
            // avoid `$HOME` here: Claude Code's workspace trust
            // dialog persists its acceptance per-project in
            // `~/.claude.json`, but the home directory does not
            // reliably persist that acceptance, so using `$HOME` as
            // the cwd produces the trust prompt on every single
            // Ctrl+G. Every non-home project path Claude Code sees
            // DOES persist trust correctly, so a stable
            // workbridge-owned scratch directory sidesteps the
            // problem entirely without workbridge ever reading or
            // writing `~/.claude.json`. On macOS `$TMPDIR` is
            // per-user and stable across reboots. `create_dir_all`
            // is idempotent and handles the case where the OS tmp
            // cleaner has wiped the directory since the last spawn.
            if worker_cancelled.load(Ordering::Acquire) {
                // The main thread's cleanup may have already run
                // (and found a non-existent file) before we wrote
                // the config. Remove it here so the file is not
                // orphaned.
                let _ = std::fs::remove_file(&worker_config_path);
                drop(mcp_server);
                return;
            }
            let scratch =
                crate::side_effects::paths::temp_dir().join("workbridge-global-assistant-cwd");
            if let Err(e) = std::fs::create_dir_all(&scratch) {
                let _ = tx.send(GlobalSessionPrepResult {
                    mcp_server: Some(mcp_server),
                    session: None,
                    error: Some(format!("Global assistant scratch dir error: {e}")),
                });
                return;
            }

            // Phase E: build argv via the pluggable backend.
            // `stage: Implementing` is used solely so the C8
            // planning-reminder hook is NOT installed (Planning is
            // the only stage that triggers the reminder); the global
            // assistant has no stage concept. `auto_start_message:
            // None` because the global assistant waits for the first
            // user keystroke before doing anything.
            let cfg = SpawnConfig {
                stage: WorkItemStatus::Implementing,
                system_prompt: system_prompt.as_deref(),
                mcp_config_path: Some(&worker_config_path),
                mcp_bridge: Some(&global_bridge),
                // Global assistant has no per-repo context, so no
                // user-configured per-repo MCP servers to forward.
                extra_bridges: &[],
                allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
                auto_start_message: None,
                read_only: false,
            };
            let cmd = agent_backend.build_command(&cfg);
            let cmd_refs: Vec<&str> = cmd.iter().map(std::string::String::as_str).collect();

            // Phase F: spawn the PTY session. The fork+exec is
            // normally sub-millisecond but still blocks on process
            // creation, so it runs here rather than on the UI
            // thread. Last cancellation check: skip the fork+exec
            // if the drawer was closed while we were in Phase C/D.
            if worker_cancelled.load(Ordering::Acquire) {
                let _ = std::fs::remove_file(&worker_config_path);
                drop(mcp_server);
                return;
            }
            let session = match Session::spawn(pane_cols, pane_rows, Some(&scratch), &cmd_refs) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(GlobalSessionPrepResult {
                        mcp_server: Some(mcp_server),
                        session: None,
                        error: Some(format!("Global assistant spawn error: {e}")),
                    });
                    return;
                }
            };

            let result = GlobalSessionPrepResult {
                mcp_server: Some(mcp_server),
                session: Some(session),
                error: None,
            };

            // Hand the result back to the UI thread. If the
            // receiver has been dropped (drawer closed mid-flight),
            // the main thread's `teardown_global_session` already
            // scheduled a `spawn_agent_file_cleanup` for the shared
            // `config_path`, so the worker does not need to clean
            // up the tempfile itself. The `McpSocketServer` and
            // `Session` handles inside `result` run their own Drop
            // impls on scope exit, which stop the accept loop,
            // remove the socket file, and force-kill the child
            // process group respectively.
            let _ = tx.send(result);
        });

        let activity = self.start_activity("Opening global assistant...");
        self.global_session_open_pending = Some(GlobalSessionOpenPending {
            rx,
            activity,
            pre_drawer_focus,
            config_path,
            cancelled,
        });
    }

    /// Drain any pending global-assistant preparation worker result.
    /// Called from the background-work tick alongside the other
    /// `poll_*` methods. On success, the worker's session and
    /// server handles plus the UI-thread-committed config path are
    /// moved into the durable `global_session` / `global_mcp_server`
    /// / `global_mcp_config_path` fields. On error the drawer is
    /// reset to closed, the pre-drawer focus is restored, and the
    /// committed (possibly-written) config path is routed through
    /// `spawn_agent_file_cleanup` so no tempfile is leaked to `/tmp`
    /// even when the worker dies after Phase C.
    pub fn poll_global_session_open(&mut self) {
        let recv_result = match self.global_session_open_pending.as_ref() {
            Some(pending) => match pending.rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            },
            None => return,
        };
        let Some(pending) = self.global_session_open_pending.take() else {
            return;
        };
        self.end_activity(pending.activity);

        if let Ok(result) = recv_result {
            if let Some(err) = result.error {
                // Worker reported a fatal error. Drop MCP server
                // and session off the UI thread so their
                // destructors (socket unlink, child kill/join)
                // do not block the event loop.
                if let Some(server) = result.mcp_server {
                    self.drop_mcp_server_off_thread(server);
                }
                if let Some(session) = result.session {
                    std::thread::spawn(move || drop(session));
                }
                self.spawn_agent_file_cleanup(vec![pending.config_path]);
                self.status_message = Some(err);
                self.global_drawer_open = false;
                self.focus = pending.pre_drawer_focus;
                // Clear buffered keystrokes so they do not leak
                // into the next successful open.
                self.pending_global_pty_bytes.clear();
                return;
            }

            // Success path: move worker handles into the durable
            // App fields. The config path was owned by the
            // pending entry all along (not by the result) so
            // the worker cannot be in a state where it thinks
            // it owns the tempfile separately.
            if let Some(session) = result.session {
                let parser = Arc::clone(&session.parser);
                self.global_session = Some(SessionEntry {
                    parser,
                    alive: true,
                    session: Some(session),
                    scrollback_offset: 0,
                    selection: None,
                    agent_written_files: Vec::new(),
                });
            }
            if let Some(server) = result.mcp_server {
                self.global_mcp_server = Some(server);
            }
            self.global_mcp_config_path = Some(pending.config_path);
        } else {
            // Worker thread exited without sending. The config
            // path may or may not be on disk; route it through
            // cleanup anyway (same rationale as the error arm
            // above).
            self.spawn_agent_file_cleanup(vec![pending.config_path]);
            self.status_message =
                Some("Global assistant: preparation worker exited unexpectedly".into());
            self.global_drawer_open = false;
            self.focus = pending.pre_drawer_focus;
            self.pending_global_pty_bytes.clear();
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
                    .map_or("", |pr| pr.url.as_str());
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
        let Ok(list_result) = self.backend.list() else {
            // Backend list failed - the fetcher just won't have extras.
            // The error will surface through other paths (assembly, etc.).
            return map;
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
/// cache keys (keyed by `repo_path`) match assembly lookups. If canonicalization
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

/// Public crate-level accessor for `now_iso8601`, used by the event module.
pub fn now_iso8601_pub() -> String {
    now_iso8601()
}

/// Return the current time as an ISO 8601 string (UTC).
fn now_iso8601() -> String {
    let dur = crate::side_effects::clock::system_now()
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
pub const fn is_selectable(entry: &DisplayEntry) -> bool {
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

    fn prune_worktrees(
        &self,
        _repo_path: &std::path::Path,
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
}

#[cfg(test)]
mod tests;
