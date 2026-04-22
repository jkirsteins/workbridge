//! Worktree creation + first-run global-harness modal subsystem.
//!
//! Drains the async worktree-creation channel
//! (`poll_worktree_creation`), spawns the stale-worktree recovery
//! background thread (`spawn_stale_worktree_recovery`), and handles
//! the first-run Ctrl+G modal that asks the user to pick a harness
//! for the global assistant (`handle_ctrl_g`,
//! `finish_first_run_global_pick`, `cancel_first_run_global_pick`).
//! Grouped together because both operations gate on the same
//! pre-conditions (no session open yet, explicit user intent).

use std::sync::Arc;
use std::time::Duration;

use super::{
    FirstRunGlobalHarnessModal, McpInjection, QUICKSTART_TITLE, StaleWorktreePrompt, UserActionKey,
    UserActionPayload, WorktreeCreateResult,
};
use crate::agent_backend::{
    self, AgentBackend, AgentBackendKind, SpawnConfig, WORK_ITEM_ALLOWED_TOOLS,
};
use crate::work_item::{WorkItemId, WorkItemStatus};

impl super::App {
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
            self.toasts.push(
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
        self.services.config.defaults.global_assistant_harness = Some(kind.canonical_name().into());
        // Persist via the configured provider. The helper swallows
        // errors as toasts so a read-only config dir does not take
        // down the UI; the in-memory value still reflects the pick
        // for this TUI session.
        if let Err(e) = self.services.config_provider.save(&self.services.config) {
            self.toasts.push(format!("could not save config: {e}"));
        }
        self.toggle_global_drawer();
    }

    /// Dismiss the first-run modal without a pick. Config stays at its
    /// previous (None) state; the drawer does not open.
    pub fn cancel_first_run_global_pick(&mut self) {
        self.first_run_global_harness_modal = None;
    }

    /// Test-only thin wrapper over `build_agent_cmd_with(self.services.agent_backend, ...)`.
    /// Exists so legacy tests can assert argv-shape without stitching
    /// a per-work-item backend; new production call sites use
    /// `build_agent_cmd_with` directly so the per-work-item harness
    /// choice is honored.
    #[cfg(test)]
    pub(super) fn build_agent_cmd(
        &self,
        status: WorkItemStatus,
        system_prompt: Option<&str>,
        mcp_config_path: Option<&std::path::Path>,
        force_auto_start: bool,
    ) -> Vec<String> {
        self.build_agent_cmd_with(
            self.services.agent_backend.as_ref(),
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
    pub(super) fn build_agent_cmd_with(
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
    pub(super) fn auto_start_message_for_stage(
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
            self.shell.status_message =
                Some("Worktree creation: background thread exited unexpectedly".into());
            return;
        };

        self.end_user_action(&UserActionKey::WorktreeCreate);

        // If this result came from a stale-worktree recovery, clear the
        // recovery modal. On success the prompt is dismissed; on failure
        // the error arm below will re-display the appropriate alert.
        if self.prompt_flags.stale_recovery_in_progress {
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
                        self.shell.status_message = Some(
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
                    self.shell.status_message = Some(
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
                    self.shell.status_message = Some("Imported (worktree created)".into());
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
                        self.shell.status_message = Some(error);
                    }
                }
            }
            (None, None) => {
                // Unexpected - no path and no error.
                self.shell.status_message =
                    Some("Worktree creation completed with no result".into());
            }
        }
    }

    /// Clear both stale-worktree recovery fields atomically. These two
    /// fields must always be cleared together; using this helper instead
    /// of setting them individually prevents a future cleanup site from
    /// clearing one but not the other (which would leave a stuck spinner).
    pub(super) fn clear_stale_recovery(&mut self) {
        self.stale_worktree_prompt = None;
        self.prompt_flags.stale_recovery_in_progress = false;
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
        self.prompt_flags.stale_recovery_in_progress = true;

        if self
            .try_begin_user_action(
                UserActionKey::WorktreeCreate,
                Duration::ZERO,
                "Recovering stale worktree...",
            )
            .is_none()
        {
            self.clear_stale_recovery();
            self.shell.status_message = Some("Worktree operation already in progress...".into());
            return;
        }

        let ws = Arc::clone(&self.services.worktree_service);
        let wt_dir = self.services.config.defaults.worktree_dir.clone();
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
    pub(super) fn stage_system_prompt(
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
    pub(super) fn branch_has_commits(&self, repo_path: &std::path::Path, branch: &str) -> bool {
        self.repo_data
            .get(repo_path)
            .and_then(|rd| rd.worktrees.as_ref().ok())
            .and_then(|wts| wts.iter().find(|wt| wt.branch.as_deref() == Some(branch)))
            .and_then(|wt| wt.has_commits_ahead)
            .unwrap_or(false)
    }
}
