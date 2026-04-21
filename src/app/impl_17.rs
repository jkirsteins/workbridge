//! Subset of `impl App` methods extracted from `src/app/mod.rs`.
//!
//! The `impl App { ... }` is split across sibling files solely to
//! keep every file within the 700-line ceiling. Methods behave
//! identically to the original single-file layout.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use super::*;
use crate::work_item::{SessionEntry, WorkItemId, WorkItemStatus};
use crate::work_item_backend::ActivityEntry;

impl super::App {
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
    pub(super) fn drop_rebase_gate(&mut self, wi_id: &WorkItemId) {
        let removed = self.rebase_gates.remove(wi_id);
        let slot_owner_matches =
            self.user_action_work_item(&UserActionKey::RebaseOnMain) == Some(wi_id);

        if let Some(state) = removed {
            // Cancellation + killpg happen in `Drop for
            // RebaseGateState` when `state` falls out of scope at
            // the end of this block. We do NOT need to manually
            // signal them here.
            self.activities.end(state.activity);
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
    pub(super) fn abort_background_ops_for_work_item(&mut self, wi_id: &WorkItemId) {
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
    pub(super) fn teardown_global_session(&mut self) {
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
            self.activities.end(pending.activity);
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
}
