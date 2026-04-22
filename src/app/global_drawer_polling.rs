//! Global assistant drawer subsystem.
//!
//! Drains the async global-session spawn channel
//! (`poll_global_session_open`), buffers/flushes PTY bytes to the
//! global session (`send_bytes_to_global`), refreshes the
//! dynamic MCP context passed to the global agent
//! (`refresh_global_mcp_context`), and collects the
//! `extra_branches_from_backend` surface used at fetcher startup.
//! The global assistant is one of the four known harness spawn
//! paths in `docs/harness-contract.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{GlobalSessionOpenPending, GlobalSessionPrepResult};
use crate::agent_backend::{self, AgentBackend, SpawnConfig, WORK_ITEM_ALLOWED_TOOLS};
use crate::mcp::McpSocketServer;
use crate::session::Session;
use crate::work_item::{SessionEntry, WorkItemStatus};

impl super::App {
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
    pub(super) fn spawn_global_session(&mut self) {
        // If a previous preparation is still in flight, cancel it
        // first so we don't end up with two workers racing each
        // other on resource ownership. `teardown_global_session` is
        // the canonical cleanup path (it also routes the config
        // file through `spawn_agent_file_cleanup`), and
        // `toggle_global_drawer` already calls teardown before
        // spawning, so this branch is defence in depth.
        if self.global_drawer.session_open_pending.is_some() {
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
        // `self.services.agent_backend`: CLAUDE.md has an [ABSOLUTE] rule
        // against silent default-harness substitution, and this is
        // the last line of defence for any future bypass of
        // `handle_ctrl_g`'s guard.
        let Some(kind) = self.global_assistant_harness_kind() else {
            self.global_drawer.open = false;
            self.shell.focus = self.global_drawer.pre_drawer_focus;
            self.toasts.push(
                "Cannot open global assistant: no harness configured. Press Ctrl+G again to pick one."
                    .into(),
            );
            return;
        };
        let agent_backend: Arc<dyn AgentBackend> = agent_backend::backend_for_kind(kind);

        // Capture everything the worker needs. All Send + Sync.
        let mcp_context_shared = Arc::clone(&self.global_drawer.mcp_context);
        let mcp_tx = self.mcp_tx.clone();
        let pane_cols = self.global_drawer.pane_cols;
        let pane_rows = self.global_drawer.pane_rows;
        let pre_drawer_focus = self.global_drawer.pre_drawer_focus;
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

        let activity = self.activities.start("Opening global assistant...");
        self.global_drawer.session_open_pending = Some(GlobalSessionOpenPending {
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
        let recv_result = match self.global_drawer.session_open_pending.as_ref() {
            Some(pending) => match pending.rx.try_recv() {
                Ok(r) => Ok(r),
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(crossbeam_channel::TryRecvError::Disconnected) => Err(()),
            },
            None => return,
        };
        let Some(pending) = self.global_drawer.session_open_pending.take() else {
            return;
        };
        self.activities.end(pending.activity);

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
                self.shell.status_message = Some(err);
                self.global_drawer.open = false;
                self.shell.focus = pending.pre_drawer_focus;
                // Clear buffered keystrokes so they do not leak
                // into the next successful open.
                self.global_drawer.pending_pty_bytes.clear();
                return;
            }

            // Success path: move worker handles into the durable
            // App fields. The config path was owned by the
            // pending entry all along (not by the result) so
            // the worker cannot be in a state where it thinks
            // it owns the tempfile separately.
            if let Some(session) = result.session {
                let parser = Arc::clone(&session.parser);
                self.global_drawer.session = Some(SessionEntry {
                    parser,
                    alive: true,
                    session: Some(session),
                    scrollback_offset: 0,
                    selection: None,
                    agent_written_files: Vec::new(),
                });
            }
            if let Some(server) = result.mcp_server {
                self.global_drawer.mcp_server = Some(server);
            }
            self.global_drawer.mcp_config_path = Some(pending.config_path);
        } else {
            // Worker thread exited without sending. The config
            // path may or may not be on disk; route it through
            // cleanup anyway (same rationale as the error arm
            // above).
            self.spawn_agent_file_cleanup(vec![pending.config_path]);
            self.shell.status_message =
                Some("Global assistant: preparation worker exited unexpectedly".into());
            self.global_drawer.open = false;
            self.shell.focus = pending.pre_drawer_focus;
            self.global_drawer.pending_pty_bytes.clear();
        }
    }

    /// Send raw bytes to the global assistant session's PTY.
    pub fn send_bytes_to_global(&mut self, data: &[u8]) {
        if let Some(ref entry) = self.global_drawer.session
            && entry.alive
            && let Some(ref session) = entry.session
            && let Err(e) = session.write_bytes(data)
        {
            self.shell.status_message = Some(format!("Global assistant write error: {e}"));
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

        match self.global_drawer.mcp_context.lock() {
            Ok(mut guard) => {
                *guard = ctx.to_string();
            }
            Err(e) => {
                self.shell.status_message = Some(format!("Global MCP context lock poisoned: {e}"));
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
        let Ok(list_result) = self.services.backend.list() else {
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
