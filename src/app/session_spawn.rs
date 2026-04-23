//! Session spawn subsystem - the main work-item session opener.
//!
//! Holds `spawn_session`, which routes to the current harness for
//! the work item (via `harness_choice`), writes the MCP config
//! side-car, starts the PTY, and registers the session in the
//! `sessions` map. The single spawn site on this subsystem is
//! one of the four known harness spawn paths enumerated in
//! `docs/harness-contract.md`.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{SessionOpenPending, UserActionKey, UserActionPayload, WorktreeCreateResult};
use crate::agent_backend::AgentBackendKind;
use crate::work_item::{WorkItemId, WorkItemStatus};

impl super::App {
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
            self.shell.status_message = Some("Worktree creation already in progress...".into());
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
                        &self.services.config.defaults.worktree_dir,
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
                        self.shell.status_message =
                            Some("Worktree creation already in progress...".into());
                        return;
                    }

                    let ws = Arc::clone(&self.services.worktree_service);
                    let wi_id_clone = work_item_id.clone();

                    let (tx, rx) = crossbeam_channel::bounded(1);

                    std::thread::spawn(move || {
                        Self::run_session_worktree_thread(
                            ws.as_ref(),
                            wi_id_clone,
                            repo_path,
                            branch,
                            &wt_target,
                            &tx,
                        );
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

    /// Background worker body for `spawn_session`: fetches (or
    /// creates) the branch and then creates / reuses the worktree for
    /// it. Reports the outcome back through `tx` as a
    /// `WorktreeCreateResult`. When the branch is locked to a stale
    /// worktree (interrupted rebase), captures that path so
    /// `poll_worktree_creation` can offer a stale-worktree recovery
    /// prompt.
    fn run_session_worktree_thread(
        ws: &dyn crate::worktree_service::WorktreeService,
        wi_id: WorkItemId,
        repo_path: PathBuf,
        branch: String,
        wt_target: &std::path::Path,
        tx: &crossbeam_channel::Sender<WorktreeCreateResult>,
    ) {
        // Fetch the branch from origin first.
        // If fetch fails, try to create a new local branch.
        if ws.fetch_branch(&repo_path, &branch).is_err()
            && let Err(create_err) = ws.create_branch(&repo_path, &branch)
        {
            let _ = tx.send(WorktreeCreateResult {
                wi_id,
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
        let reused_wt = Self::find_reusable_worktree(ws, &repo_path, &branch, wt_target);
        let (wt_result, reused) = reused_wt.map_or_else(
            || (ws.create_worktree(&repo_path, &branch, wt_target), false),
            |existing_wt| (Ok(existing_wt), true),
        );
        match wt_result {
            Ok(wt_info) => {
                let _ = tx.send(WorktreeCreateResult {
                    wi_id,
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
            Err(crate::worktree_service::WorktreeError::BranchLockedToWorktree {
                ref locked_at,
                ..
            }) => {
                let _ = tx.send(WorktreeCreateResult {
                    wi_id,
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
                    wi_id,
                    repo_path,
                    branch: Some(branch.clone()),
                    path: None,
                    error: Some(format!("Failed to create worktree for '{branch}': {e}")),
                    open_session: true,
                    branch_gone: false,
                    reused: false,
                    stale_worktree_path: None,
                });
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
    pub(super) fn begin_session_open(&mut self, work_item_id: &WorkItemId, cwd: &std::path::Path) {
        if self.session_open_rx.contains_key(work_item_id) {
            // Phase 1 already in flight - the pending worker will
            // finish the open. Re-surface the spinner message so a
            // repeat Enter press is not silent.
            self.shell.status_message = Some("Opening session...".into());
            return;
        }
        if self.session_spawn_rx.contains_key(work_item_id) {
            // Phase 2 PTY spawn already in flight - the pending
            // `poll_session_spawns` tick will install the session.
            self.shell.status_message = Some("Spawning agent session...".into());
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
            self.toasts.push(
                "Cannot open session: no harness chosen for this work item. Press c / x to pick one first."
                    .into(),
            );
            return;
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let backend = Arc::clone(&self.services.backend);
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
        // no filesystem or subprocess calls happen in this block.
        let socket_path = crate::mcp::socket_path_for_session();
        let wi_id_str = serde_json::to_string(work_item_id).unwrap_or_default();
        let (wi_kind, context_json, repo_mcp_servers) =
            self.precompute_session_open_mcp_inputs(work_item_id, &wi_id_str, &cwd_clone);
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
                self.toasts.push(format!(
                    "Codex: {http_skipped} HTTP MCP server(s) skipped (Codex requires stdio)"
                ));
            }
        }
        // `activity_path_for` is a pure in-memory path computation in
        // `LocalFileBackend` (no filesystem I/O); kept here on the UI
        // thread to avoid cloning the whole `Arc<dyn WorkItemBackend>`
        // into the worker purely for a path join.
        let activity_log_path = self.services.backend.activity_path_for(work_item_id);
        let mcp_tx = self.mcp_tx.clone();
        let socket_path_for_worker = socket_path;

        let prep = super::session_open_prep::SessionOpenPrepArgs {
            backend,
            wi_id: wi_id_clone,
            cwd: cwd_clone,
            worker_cancelled,
            socket_path: socket_path_for_worker,
            wi_id_str,
            wi_kind,
            context_json,
            activity_log_path,
            mcp_tx,
            agent_backend,
            repo_mcp_servers,
            worker_committed_files,
            worker_mcp_config_path,
        };
        std::thread::spawn(move || {
            super::session_open_prep::run_session_open_prep_worker(prep, &tx);
        });
        // Surface immediate feedback so a slow background phase does
        // not make the TUI look hung between the Enter keypress and
        // the next `poll_session_opens` tick (200ms). The spinner is
        // ended in `poll_session_opens` for every terminal arm
        // (success, read_error, disconnect) via `drop_session_open_entry`.
        let activity = self.activities.start("Opening session...");
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
    pub(super) fn drop_session_open_entry(&mut self, wi_id: &WorkItemId) {
        if let Some(entry) = self.session_open_rx.remove(wi_id) {
            self.activities.end(entry.activity);
        }
    }

    /// Pre-compute every `&self`-requiring MCP input the Phase 1
    /// session-open worker needs. Returns the per-work-item kind
    /// label, the JSON context string MCP clients see, and the
    /// repo-scoped `McpServerEntry` vec. All computations are pure
    /// in-memory cache reads - see `docs/UI.md` "Blocking I/O
    /// Prohibition" for why an audit of this exact block matters.
    fn precompute_session_open_mcp_inputs(
        &self,
        work_item_id: &WorkItemId,
        wi_id_str: &str,
        cwd: &std::path::Path,
    ) -> (String, String, Vec<crate::config::McpServerEntry>) {
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
                    "repo": cwd.display().to_string(),
                    "pr_url": pr_url,
                })
                .to_string()
            },
        );
        let repo_mcp_servers: Vec<crate::config::McpServerEntry> = wi
            .and_then(|w| w.repo_associations.first())
            .map(|assoc| {
                let repo_display = crate::config::collapse_home(&assoc.repo_path);
                self.services
                    .config
                    .mcp_servers_for_repo(&repo_display)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (wi_kind, context_json, repo_mcp_servers)
    }
}
