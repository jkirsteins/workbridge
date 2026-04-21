//! Harness subsystem - per-item LLM harness choice + session
//! open/close polling.
//!
//! Owns the `harness_choice` map and every read of it: the
//! `backend_for_work_item` resolver (which every spawn site
//! funnels through), `agent_backend_display_name` and its
//! permission-marker variant, the `kk` double-press session-end
//! gesture, and the global-assistant harness-kind lookup. Also
//! drains the async session-open pipeline
//! (`poll_session_opens`, `poll_session_spawns`).

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;
use crate::agent_backend::{self, AgentBackend, AgentBackendKind};
use crate::session::Session;
use crate::work_item::{SessionEntry, WorkItemId, WorkItemStatus};

impl super::App {
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
    pub(super) fn cancel_session_open_entry(&mut self, wi_id: &WorkItemId) {
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
            self.activities.end(entry.activity);
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
    pub(super) fn finish_session_open(&mut self, result: SessionOpenPlanResult) {
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
            self.toasts.push(
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

        let activity = self.activities.start("Spawning agent session...");
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
                        self.activities.end(pending.activity);
                    }
                    self.status_message =
                        Some("Session spawn: background thread exited unexpectedly".into());
                    continue;
                }
            };
            if let Some(pending) = self.session_spawn_rx.remove(&wi_id) {
                self.activities.end(pending.activity);
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
    /// `self.services.agent_backend.kind().display_name()` as a fallback would
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
    pub(super) fn resolved_harness_kind(&self) -> Option<AgentBackendKind> {
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
    /// than silently defaulting to `self.services.agent_backend` - that was the
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
            self.toasts
                .push(format!("{}: command not found", kind.binary_name()));
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
                self.toasts
                    .push("session already running - press kk to end first".into());
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
            self.toasts.push("session ended".into());
        } else {
            self.last_k_press = Some((work_item_id, now));
            self.toasts
                .push("press k again within 1.5s to end session".into());
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
        let name = self
            .services
            .config
            .defaults
            .global_assistant_harness
            .as_deref()?;
        AgentBackendKind::from_str(name).ok()
    }
}
