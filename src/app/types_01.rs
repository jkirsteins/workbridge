//! Type definitions extracted from `src/app/mod.rs`. Split into
//! sibling files so each stays within the 700-line ceiling.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent_backend::AgentBackendKind;
use crate::click_targets::ClickKind;
use crate::work_item::{WorkItemId, WorkItemStatus};

use super::*;

pub use user_actions::{UserActionPayload, UserActionState};

/// A transient top-right notification shown after a click-to-copy
/// action. Auto-dismisses when `expires_at` is reached. Rendered by
/// `ui::draw_toasts` on top of everything else, including the global
/// drawer and settings overlay.
#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub expires_at: std::time::Instant,
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
pub const PR_MERGE_ALREADY_IN_PROGRESS: &str = "PR merge already in progress";

/// Rejection wording shown when `spawn_review_submission` refuses a
/// second concurrent review submission. Same duplication rationale as
/// `PR_MERGE_ALREADY_IN_PROGRESS`.
pub const REVIEW_SUBMIT_ALREADY_IN_PROGRESS: &str = "Review submission already in progress";

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
pub struct ActivityId(pub u64);

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
pub struct CiCheck {
    pub name: String,
    /// One of: pass, fail, pending, skipping, cancel
    pub bucket: String,
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
