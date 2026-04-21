//! Subset of `impl App` methods extracted from `src/app/mod.rs`.
//!
//! The `impl App { ... }` is split across sibling files solely to
//! keep every file within the 700-line ceiling. Methods behave
//! identically to the original single-file layout.

use std::sync::mpsc;

use crate::assembly;
use crate::github_client::GithubError;
use crate::work_item::{FetchMessage, WorkItemId, WorkItemStatus};
use crate::work_item_backend::ActivityEntry;

use super::*;

impl super::App {
    /// Route buffered bytes to whichever right-panel tab is active.
    pub fn buffer_bytes_to_right_panel(&mut self, data: &[u8]) {
        match self.right_panel_tab {
            RightPanelTab::ClaudeCode => self.buffer_bytes_to_active(data),
            RightPanelTab::Terminal => self.buffer_bytes_to_terminal(data),
        }
    }

    /// Returns true if the currently selected work item has a worktree path.
    pub fn selected_work_item_has_worktree(&self) -> bool {
        let Some(wi_id) = self.selected_work_item_id() else {
            return false;
        };
        let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
            return false;
        };
        wi.repo_associations
            .iter()
            .any(|a| a.worktree_path.is_some())
    }

    /// Drain pending fetch results from the background fetcher channel.
    ///
    /// Calls `try_recv()` in a loop until the channel is empty, storing each
    /// `RepoData` result in `self.repo_data`. `FetcherError` messages are surfaced
    /// via the status bar.
    ///
    /// Returns true if any messages were received (meaning reassembly is
    /// warranted).
    pub fn drain_fetch_results(&mut self) -> bool {
        // First, collect all pending messages into a local Vec so the
        // `self.fetch_rx` borrow is released before we call any
        // `&mut self` helpers (`end_user_action`, `end_activity`, etc.).
        // Previously this function reached directly into
        // `self.user_actions.in_flight.remove(...)` and
        // `self.activities.retain(...)` because the `rx` borrow blocked
        // it from routing through `end_user_action`; that created a
        // drift hazard if `end_user_action` ever grew side effects.
        // Now every cleanup path here is identical to the rest of the
        // codebase.
        let mut messages = Vec::new();
        let mut disconnected = false;
        {
            let Some(ref rx) = self.fetch_rx else {
                return false;
            };
            loop {
                match rx.try_recv() {
                    Ok(msg) => messages.push(msg),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut received_any = false;
        for msg in messages {
            match msg {
                FetchMessage::FetchStarted => {
                    // Show a spinner while GitHub data is being fetched.
                    // Track how many repos are in-flight so the spinner
                    // persists until all repos have reported back.
                    //
                    // If the Ctrl+R path has already admitted a
                    // `GithubRefresh` action, reuse its activity - do NOT
                    // start a second one. Otherwise (structural restart
                    // path: manage/unmanage, quickstart create, delete
                    // cleanup, etc.) own the spinner locally via
                    // `structural_fetch_activity` so the single-spinner
                    // invariant holds.
                    self.pending_fetch_count += 1;
                    let helper_owns_it =
                        self.is_user_action_in_flight(&UserActionKey::GithubRefresh);
                    if !helper_owns_it && self.structural_fetch_activity.is_none() {
                        let id = self.start_activity("Refreshing GitHub data");
                        self.structural_fetch_activity = Some(id);
                    }
                }
                FetchMessage::RepoData(result) => {
                    received_any = true;
                    self.pending_fetch_count = self.pending_fetch_count.saturating_sub(1);
                    // End both possible owners of the fetch spinner:
                    // the Ctrl+R helper entry (if it started this
                    // cycle) and the structural fallback (if the
                    // restart path started it). Exactly one of them
                    // actually holds an activity at any given time.
                    if self.pending_fetch_count == 0 {
                        self.end_user_action(&UserActionKey::GithubRefresh);
                        if let Some(id) = self.structural_fetch_activity.take() {
                            self.end_activity(id);
                        }
                    }
                    // Surface worktree errors in the status bar. One-time
                    // per repo to avoid flooding on every fetch cycle.
                    if let Err(ref e) = result.worktrees
                        && self.worktree_errors_shown.insert(result.repo_path.clone())
                    {
                        self.status_message = Some(format!(
                            "Worktree error ({}): {e}",
                            result.repo_path.display(),
                        ));
                    }
                    // Surface GitHub errors in the status bar. One-time
                    // messages for CliNotFound and AuthRequired so we
                    // don't spam on every fetch cycle.
                    if let Err(ref e) = result.prs {
                        match e {
                            GithubError::CliNotFound => {
                                if !self.gh_cli_not_found_shown {
                                    self.gh_cli_not_found_shown = true;
                                    self.status_message =
                                        Some("gh CLI not found - GitHub features disabled".into());
                                }
                            }
                            GithubError::AuthRequired => {
                                if !self.gh_auth_required_shown {
                                    self.gh_auth_required_shown = true;
                                    self.status_message =
                                        Some("gh auth required - run 'gh auth login'".into());
                                }
                            }
                            _ => {
                                let msg = format!("GitHub: {e}");
                                if self.status_message.is_none() {
                                    self.status_message = Some(msg);
                                } else {
                                    self.pending_fetch_errors.push(msg);
                                }
                            }
                        }
                    }
                    // Capture the authenticated user's login so review-
                    // request row rendering can classify direct-to-you vs.
                    // team. Never clobber a known login with None - a
                    // transient `gh api user` failure should not erase a
                    // value that was successfully resolved earlier in the
                    // session.
                    if let Some(login) = result.current_user_login.clone() {
                        self.current_user_login = Some(login);
                    }
                    self.repo_data.insert(result.repo_path.clone(), *result);
                    // Clear re-open suppression only after ALL repos have
                    // reported back.  In multi-repo setups, clearing on every
                    // single RepoData arrival lets an early repo's stale data
                    // re-open items that were just reviewed in a later repo.
                    if self.pending_fetch_count == 0 {
                        self.review_reopen_suppress.clear();
                    }
                }
                FetchMessage::FetcherError { repo_path, error } => {
                    received_any = true;
                    self.pending_fetch_count = self.pending_fetch_count.saturating_sub(1);
                    if self.pending_fetch_count == 0 {
                        self.end_user_action(&UserActionKey::GithubRefresh);
                        if let Some(id) = self.structural_fetch_activity.take() {
                            self.end_activity(id);
                        }
                        // Clear re-open suppression when all repos have
                        // reported back, even if they all failed.  This
                        // mirrors the clear in the RepoData arm.
                        self.review_reopen_suppress.clear();
                    }
                    let msg = format!("Fetch error ({}): {error}", repo_path.display());
                    if self.status_message.is_none() {
                        self.status_message = Some(msg);
                    } else {
                        self.pending_fetch_errors.push(msg);
                    }
                }
            }
        }

        if disconnected && !self.fetcher_disconnected {
            self.fetcher_disconnected = true;
            let msg = "Background fetcher stopped unexpectedly".to_string();
            if self.status_message.is_none() {
                self.status_message = Some(msg);
            } else {
                self.pending_fetch_errors.push(msg);
            }
        }
        received_any
    }

    /// Show the next pending fetch error if the status bar is free.
    /// Called on each tick so that errors queued while the status bar
    /// was occupied eventually surface. Shows one error per tick to
    /// avoid overwhelming the user.
    pub fn drain_pending_fetch_errors(&mut self) {
        if self.status_message.is_none()
            && let Some(msg) = self.pending_fetch_errors.first().cloned()
        {
            self.pending_fetch_errors.remove(0);
            self.status_message = Some(msg);
        }
    }

    /// Reassemble work items from backend records and cached repo data.
    ///
    /// Calls `backend.list()` for fresh records, then runs the assembly
    /// layer to produce `work_items` and `unlinked_prs`. Surfaces any
    /// corrupt backend records to the user via the status bar.
    pub fn reassemble_work_items(&mut self) {
        let list_result = match self.backend.list() {
            Ok(r) => r,
            Err(e) => {
                self.status_message = Some(format!("Backend error: {e}"));
                return;
            }
        };
        if !list_result.corrupt.is_empty() {
            let count = list_result.corrupt.len();
            let first = &list_result.corrupt[0];
            self.status_message = Some(format!(
                "{count} corrupt work item file(s): {} ({})",
                first.path.display(),
                first.reason,
            ));
        }

        let issue_pattern = &self.config.defaults.branch_issue_pattern;
        let (items, unlinked, review_requested, mut reopen_ids) = assembly::reassemble(
            &list_result.records,
            &self.repo_data,
            issue_pattern,
            &self.config.defaults.worktree_dir,
        );
        self.work_items = items;
        self.unlinked_prs = unlinked;
        self.review_requested_prs = review_requested;

        // Start the archival clock for items that became Done through PR merge
        // (derived status) but don't yet have a done_at timestamp.
        if self.config.defaults.archive_after_days > 0 {
            match crate::side_effects::clock::system_now().duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => {
                    let epoch = duration.as_secs();
                    for record in &list_result.records {
                        if record.status != WorkItemStatus::Done
                            && record.done_at.is_none()
                            && let Some(wi) = self.work_items.iter().find(|w| w.id == record.id)
                            && wi.status == WorkItemStatus::Done
                            && wi.status_derived
                            && let Err(e) = self.backend.set_done_at(&record.id, Some(epoch))
                        {
                            self.status_message =
                                Some(format!("Failed to set archive timestamp: {e}"));
                        }
                    }
                }
                Err(e) => {
                    self.status_message = Some(format!(
                        "System clock error, skipping archive timestamps: {e}"
                    ));
                }
            }
        }

        // Exclude items whose reviews were recently submitted. Stale
        // repo_data may still list them as review-requested until the
        // next GitHub fetch cycle refreshes the data.
        reopen_ids.retain(|id| !self.review_reopen_suppress.contains(id));

        // Re-open Done ReviewRequest items that have been re-requested.
        if !reopen_ids.is_empty() {
            for wi_id in &reopen_ids {
                if let Err(e) = self.backend.update_status(wi_id, WorkItemStatus::Review) {
                    self.status_message = Some(format!("Re-open error: {e}"));
                    continue;
                }
                // Clear done_at so auto-archive won't delete the re-opened item.
                if let Err(e) = self.backend.set_done_at(wi_id, None) {
                    self.status_message =
                        Some(format!("Failed to clear archive timestamp on re-open: {e}"));
                }
                let entry = ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "stage_change".to_string(),
                    payload: serde_json::json!({
                        "from": "Done",
                        "to": "Review",
                        "source": "review_re_requested"
                    }),
                };
                let _ = self.backend.append_activity(wi_id, &entry);
            }
            // Reassemble again to pick up the status changes.
            let Ok(list_result) = self.backend.list() else {
                return;
            };
            let (items, unlinked, review_requested, _) = assembly::reassemble(
                &list_result.records,
                &self.repo_data,
                issue_pattern,
                &self.config.defaults.worktree_dir,
            );
            self.work_items = items;
            self.unlinked_prs = unlinked;
            self.review_requested_prs = review_requested;

            let count = reopen_ids.len();
            self.status_message = Some(format!("{count} review request(s) re-opened"));
        }

        // Auto-archive: delete Done items that have exceeded the retention period.
        // This runs AFTER re-open detection so that re-opened items have their
        // done_at cleared and won't be incorrectly archived.
        // Skip entirely when archive is disabled (archive_after_days == 0).
        if self.config.defaults.archive_after_days > 0 {
            match self.backend.list() {
                Ok(pre_archive_list) => {
                    let pre_archive_count = pre_archive_list.records.len();
                    let kept = self.auto_archive_done_items(pre_archive_list.records);
                    if kept.len() < pre_archive_count {
                        // Items were archived; reassemble to update display state.
                        let pattern = &self.config.defaults.branch_issue_pattern;
                        let (items, unlinked, review_requested, _) = assembly::reassemble(
                            &kept,
                            &self.repo_data,
                            pattern,
                            &self.config.defaults.worktree_dir,
                        );
                        self.work_items = items;
                        self.unlinked_prs = unlinked;
                        self.review_requested_prs = review_requested;
                    }
                }
                Err(e) => {
                    self.status_message =
                        Some(format!("Failed to list items for auto-archive: {e}"));
                }
            }
        }

        // Reconstruct mergequeue watches for items that are in Mergequeue
        // but don't have a watch (e.g., after app restart).
        self.reconstruct_mergequeue_watches();

        // Reconstruct ReviewRequest merge watches for any ReviewRequest
        // item in Review (also prunes watches whose owning item no
        // longer qualifies, e.g. after an auto-transition to Done or a
        // delete). The `--author @me` and `review-requested:@me` fetch
        // paths cannot observe a merged review-request PR, so this
        // background poll is the ONLY code path that can detect the
        // merge and advance the item to Done.
        self.reconstruct_review_request_merge_watches();
    }

    /// Core work-item deletion. Removes the backend record, kills the
    /// Claude and terminal sessions, cancels in-flight background
    /// operations (worktree create, PR create, merge, review submit,
    /// mergequeue poll) and clears in-memory state.
    ///
    /// Does NOT touch selection/cursor/display state - callers handle that.
    ///
    /// Resource cleanup (worktree removal, branch deletion, PR close) is
    /// NOT performed here - it is blocking I/O and must run on a
    /// background thread. Callers that need resource cleanup first call
    /// `gather_delete_cleanup_infos` (a pure cache lookup) and then
    /// `spawn_delete_cleanup` to run the actual `git` / `gh` commands off
    /// the UI thread. Auto-archive skips resource cleanup entirely
    /// because Done items have already been through the merge flow.
    ///
    /// Warnings (best-effort cleanup failures from Phase 5 orphan
    /// handling) are appended to `warnings`. Orphaned worktrees
    /// discovered in Phase 5 (an in-flight worktree-create thread that
    /// had already produced a path before the user requested the
    /// delete) are appended to `orphan_worktrees` as `OrphanWorktree`
    /// entries so the caller can forward them to `spawn_delete_cleanup`
    /// and run both `git worktree remove` and `git branch -D` on the
    /// background cleanup thread. The branch name is preserved so
    /// `spawn_delete_cleanup` can delete the stale branch ref too
    /// (dropping it would leak the branch - master deleted it inline
    /// before the async refactor). This function MUST NOT call
    /// `self.worktree_service.remove_worktree(...)` directly - it runs
    /// on the UI thread (either the MCP tick handler or the modal
    /// confirm handler) where blocking I/O is forbidden by
    /// `docs/UI.md` "Blocking I/O Prohibition".
    ///
    /// Returns Err only if the backend delete itself fails (fatal).
    pub(super) fn delete_work_item_by_id(
        &mut self,
        wi_id: &WorkItemId,
        warnings: &mut Vec<String>,
        orphan_worktrees: &mut Vec<OrphanWorktree>,
    ) -> Result<(), crate::work_item_backend::BackendError> {
        // -- Phase 1: Cancel long-running background ops BEFORE
        //    destroying any backend state. The architectural rule
        //    here is "cancellation must precede destruction" - the
        //    rebase gate's background thread writes its own activity
        //    log entry, and if `backend.delete` archives the active
        //    log first there is a window where the bg thread can
        //    call `append_activity` and recreate an orphan active
        //    log for a deleted item (the failure mode described in
        //    docs/harness-contract.md C10). Routing every delete
        //    site through `abort_background_ops_for_work_item`
        //    closes that window structurally: by the time we reach
        //    `backend.delete` below, the gate has been removed from
        //    the map (so its `Drop` impl set the cancelled flag and
        //    SIGKILLed the harness group) and the bg thread will
        //    exit on its next phase check without writing.
        self.abort_background_ops_for_work_item(wi_id);

        // -- Phase 2: Backend cleanup (fatal on delete failure) --
        if let Err(e) = self.backend.pre_delete_cleanup(wi_id) {
            warnings.push(format!("pre-delete cleanup: {e}"));
        }
        self.backend.delete(wi_id)?;

        // -- Phase 3: Kill session and clean up MCP --
        self.cleanup_session_state_for(wi_id);
        if let Some(key) = self.session_key_for(wi_id)
            && let Some(mut entry) = self.sessions.remove(&key)
        {
            // Hand the written-files list back to the backend so it can
            // reverse any side-car files it wrote on spawn (the
            // `--mcp-config` tempfile, or future backend equivalents).
            // See `docs/harness-contract.md` C4 and
            // `AgentBackend::write_session_files`. The actual
            // `std::fs::remove_file` calls run on a dedicated
            // background thread via `spawn_agent_file_cleanup` -
            // doing them inline would block the UI thread on slow
            // or wedged filesystems, forbidden by `docs/UI.md`
            // "Blocking I/O Prohibition".
            self.spawn_agent_file_cleanup(std::mem::take(&mut entry.agent_written_files));
            if let Some(ref mut session) = entry.session {
                session.kill();
            }
        }
        // Kill associated terminal session.
        if let Some(mut entry) = self.terminal_sessions.remove(wi_id)
            && let Some(ref mut session) = entry.session
        {
            session.kill();
        }

        // -- Phase 4: (removed) Resource cleanup runs on a background
        //    thread via `spawn_delete_cleanup`. Doing it synchronously
        //    here would block the UI thread on `git worktree remove`,
        //    `git branch -D`, and `gh pr close` - all forbidden by
        //    `docs/UI.md` "Blocking I/O Prohibition".

        // -- Phase 5: Cancel in-flight operations --
        if self.user_action_work_item(&UserActionKey::WorktreeCreate) == Some(wi_id) {
            // Drain the helper payload's receiver. If the thread has
            // finished, capture the (non-reused) worktree path so the
            // caller can run background cleanup; if the thread is still
            // running, leave the helper entry intact so
            // `poll_worktree_creation` can drain it on the next tick
            // and run its orphan-cleanup path.
            let (thread_done, captured_orphan) = self
                .user_actions
                .in_flight
                .get(&UserActionKey::WorktreeCreate)
                .map_or((true, None), |state| match &state.payload {
                    UserActionPayload::WorktreeCreate { rx, .. } => match rx.try_recv() {
                        Ok(result) => {
                            let orphan = if !result.reused
                                && let Some(ref path) = result.path
                            {
                                Some(OrphanWorktree {
                                    repo_path: result.repo_path.clone(),
                                    worktree_path: path.clone(),
                                    branch: result.branch.clone(),
                                })
                            } else {
                                None
                            };
                            (true, orphan)
                        }
                        Err(crossbeam_channel::TryRecvError::Disconnected) => (true, None),
                        Err(crossbeam_channel::TryRecvError::Empty) => (false, None),
                    },
                    _ => (true, None),
                });
            if let Some(orphan) = captured_orphan {
                orphan_worktrees.push(orphan);
            }
            if thread_done {
                self.end_user_action(&UserActionKey::WorktreeCreate);
            }
        }
        if self.user_action_work_item(&UserActionKey::PrCreate) == Some(wi_id) {
            self.end_user_action(&UserActionKey::PrCreate);
        }
        self.pr_create_pending.retain(|id| id != wi_id);
        if self.merge_wi_id.as_ref() == Some(wi_id)
            && self.is_user_action_in_flight(&UserActionKey::PrMerge)
        {
            // `end_user_action` drops the slot and any payload it
            // owns - both `PrMergePrecheck` and `PrMerge` variants
            // store their receivers structurally inside the helper
            // entry, so no sibling clears are required.
            self.end_user_action(&UserActionKey::PrMerge);
            self.merge_in_progress = false;
        }
        if self.user_action_work_item(&UserActionKey::ReviewSubmit) == Some(wi_id) {
            self.end_user_action(&UserActionKey::ReviewSubmit);
        }
        self.mergequeue_watches.retain(|w| w.wi_id != *wi_id);
        self.mergequeue_poll_errors.remove(wi_id);
        if let Some(state) = self.mergequeue_polls.remove(wi_id) {
            self.end_activity(state.activity);
        }

        // -- Phase 6: In-memory state cleanup --
        self.rework_reasons.remove(wi_id);
        self.review_gate_findings.remove(wi_id);
        self.review_reopen_suppress.remove(wi_id);
        self.no_plan_prompt_queue.retain(|id| id != wi_id);
        if self.no_plan_prompt_queue.is_empty() {
            self.no_plan_prompt_visible = false;
        }
        if self.rework_prompt_wi.as_ref() == Some(wi_id) {
            self.rework_prompt_wi = None;
            self.rework_prompt_visible = false;
        }
        if self.merge_wi_id.as_ref() == Some(wi_id) {
            self.merge_wi_id = None;
            self.confirm_merge = false;
        }
        self.drop_review_gate(wi_id);
        // The rebase gate was already torn down in Phase 1 via
        // `abort_background_ops_for_work_item`, BEFORE
        // `backend.delete` ran, so no second call is needed here.
        // Calling `drop_rebase_gate` again would be a no-op (the
        // map entry is gone) but the redundancy would invite future
        // confusion about the canonical cancellation point.
        if self
            .branch_gone_prompt
            .as_ref()
            .is_some_and(|(id, _)| id == wi_id)
        {
            self.branch_gone_prompt = None;
        }
        if self
            .stale_worktree_prompt
            .as_ref()
            .is_some_and(|p| p.wi_id == *wi_id)
        {
            self.clear_stale_recovery();
        }

        Ok(())
    }
}
