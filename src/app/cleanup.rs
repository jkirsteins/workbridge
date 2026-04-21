//! Cleanup subsystem - unlinked PR close, delete cleanup, orphan
//! worktree cleanup, metrics poll.
//!
//! Groups every background cleanup operation behind one logical
//! surface: `spawn_unlinked_cleanup` / `poll_unlinked_cleanup`
//! for the unlinked-PR close flow, `spawn_delete_cleanup` /
//! `poll_delete_cleanup` for work-item deletion, and
//! `poll_orphan_cleanup_finished` for the background orphan-
//! worktree sweep spawned at startup / after delete. Also owns
//! `poll_metrics_snapshot` because metrics aggregation is a
//! background drain with the same lifecycle shape.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::mcp::McpSocketServer;

impl super::App {
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
            self.shell.status_message = Some("Unlinked PR cleanup already in progress".into());
            return;
        };
        // The cleanup modal already renders its own in-progress spinner
        // in the dialog body; a duplicate status-bar indicator would
        // mislead the user. Drop the visible activity but leave the
        // helper map entry intact so `is_user_action_in_flight` still
        // reports the true state to the modal / event / ui layers.
        self.activities.end(activity_id);

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
        let ws = Arc::clone(&self.services.worktree_service);
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
    /// Delegate to `Metrics::poll`. Left here as a thin forwarder so
    /// `salsa::app_event` keeps calling `app.poll_metrics_snapshot()`
    /// without having to reach through the subsystem. The heavy
    /// lifting (drain-to-latest, disconnect handling) lives on
    /// `Metrics::poll`.
    pub fn poll_metrics_snapshot(&mut self) {
        self.metrics.poll();
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
            self.shell.status_message = Some("Unlinked item closed".into());
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
    pub(super) fn gather_delete_cleanup_infos(
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
            self.activities.end(activity_id);
        }

        let ws = Arc::clone(&self.services.worktree_service);
        let pr_closer = Arc::clone(&self.services.pr_closer);
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
    pub(super) fn drop_mcp_server_off_thread(&self, server: McpSocketServer) {
        std::thread::spawn(move || {
            drop(server);
        });
    }

    pub(super) fn spawn_agent_file_cleanup(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let backend = Arc::clone(&self.services.agent_backend);
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
    pub(super) fn spawn_orphan_worktree_cleanup(
        &mut self,
        repo_path: PathBuf,
        worktree_path: PathBuf,
        branch: Option<String>,
    ) {
        let activity = self.activities.start(format!(
            "Cleaning up orphan worktree {}",
            worktree_path.display()
        ));
        let ws = Arc::clone(&self.services.worktree_service);
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
            self.activities.end(msg.activity);
            warnings.extend(msg.warnings);
        }
        if !warnings.is_empty() {
            self.shell.status_message = Some(warnings.join(" | "));
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
            self.shell.status_message = Some("Work item resource cleanup complete".into());
        } else {
            self.alert_message = Some(format!(
                "Delete cleanup warnings: {}",
                result.warnings.join("; ")
            ));
        }
    }
}
