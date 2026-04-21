//! Subset of `impl App` methods extracted from `src/app/mod.rs`.
//!
//! The `impl App { ... }` is split across sibling files solely to
//! keep every file within the 700-line ceiling. Methods behave
//! identically to the original single-file layout.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::session::Session;
use crate::work_item::{SessionEntry, WorkItemId, WorkItemStatus};

use super::*;

impl super::App {
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
    pub(super) fn cleanup_session_state_for(&mut self, wi_id: &WorkItemId) {
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
    pub(super) fn has_alive_active_session(&self) -> bool {
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
    pub(super) fn has_alive_terminal_session(&self) -> bool {
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
}
