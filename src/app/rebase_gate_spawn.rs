//! Rebase-gate subsystem - async rebase-gate spawn.
//!
//! Holds `spawn_rebase_gate`, which kicks off the background
//! rebase-gate job when the user presses the rebase-on-main key.
//! The rebase-gate is one of the three known harness spawn paths
//! in `docs/harness-contract.md`; the heavy compute phase lives
//! in the sibling `rebase_gate_compute` module.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::agent_backend::AgentBackendKind;
use crate::work_item_backend::ActivityEntry;

impl super::App {
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
        // inside the thread) keeps `self.services.config` reads on the UI thread,
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
                let entries = self.services.config.mcp_servers_for_repo(&repo_display);
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
            self.toasts.push(format!(
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

        let ws = Arc::clone(&self.services.worktree_service);
        let backend = Arc::clone(&self.services.backend);
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
        // `self.services.agent_backend` again.
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
            let super::rebase_gate_compute::CompDone {
                computed,
                mut gate_server,
                mut config_path,
            } = super::rebase_gate_compute::do_compute(super::rebase_gate_compute::ComputeInputs {
                tx: &tx,
                wi_id_clone: &wi_id_clone,
                child_pid: &child_pid,
                cancelled: &cancelled,
                agent_backend: &agent_backend,
                rebase_extra_bridges: &rebase_extra_bridges,
                worktree_path: &worktree_path,
                branch: &branch,
                base_branch: &base_branch,
            });
            // ws/backend are not currently used by the compute phase; they
            // remain captured in the outer thread body for the post-
            // compute cleanup. Silence dead-code hints.
            let _ = (&ws, &backend);

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
}
