//! Type definitions extracted from `src/app/mod.rs`. Split into
//! sibling files so each stays within the 700-line ceiling.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::*;
use crate::mcp::McpSocketServer;
use crate::session::Session;
use crate::work_item::{WorkItemId, WorkItemStatus};
use crate::work_item_backend::PrIdentityRecord;

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
    pub repo_path: PathBuf,
    pub branch: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub branch_in_main_worktree: bool,
    pub open_pr_number: Option<u64>,
    pub github_remote: Option<(String, String)>,
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
