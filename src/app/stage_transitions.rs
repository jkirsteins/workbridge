//! Stage transition subsystem - advance/retreat/apply + delete.
//!
//! Owns the cross-cutting `advance_stage`, `retreat_stage`, and
//! `apply_stage_change` methods that every stage change goes
//! through, plus `plan_from_branch` (auto-derive a plan title
//! from the branch name when promoting Backlog -> Planning) and
//! `confirm_delete_from_prompt` which finalizes the modal delete
//! started in `work_item_ops::open_delete_prompt`.

use std::path::PathBuf;

use super::*;
use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::ActivityEntry;

impl super::App {
    /// Execute the delete once the user has confirmed via the modal.
    ///
    /// Synchronously kills sessions and deletes the backend record, then
    /// spawns a background thread for the slow I/O (git worktree remove,
    /// git branch -D, gh pr close) following docs/UI.md "Blocking I/O
    /// Prohibition". The modal stays open with a spinner while the
    /// background thread runs; `poll_delete_cleanup` closes it on
    /// completion.
    pub fn confirm_delete_from_prompt(&mut self) {
        let Some(work_item_id) = self.delete_target_wi_id.clone() else {
            // Defensive: dialog was confirmed without a target. Just close it.
            self.cancel_delete_prompt();
            return;
        };

        // If a prior cleanup (MCP or modal) is still running, refuse to
        // start a second one. Alert the user and leave the modal closed -
        // they can retry once the other cleanup drains.
        if self.is_user_action_in_flight(&UserActionKey::DeleteCleanup) {
            self.cancel_delete_prompt();
            self.alert_message = Some(
                "Another delete cleanup is still in progress. \
                 Wait for it to finish and try again."
                    .into(),
            );
            return;
        }

        // Gather repo associations BEFORE touching the backend - once the
        // record is deleted we can no longer read its associations.
        let repo_associations: Vec<crate::work_item_backend::RepoAssociationRecord> = self
            .work_items
            .iter()
            .find(|w| w.id == work_item_id)
            .map(|wi| {
                wi.repo_associations
                    .iter()
                    .map(|a| crate::work_item_backend::RepoAssociationRecord {
                        repo_path: a.repo_path.clone(),
                        branch: a.branch.clone(),
                        pr_identity: None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Resource-cleanup data must be gathered before reassembly so the
        // repo_data lookups still reflect the pre-delete state.
        let mut cleanup_infos = self.gather_delete_cleanup_infos(&repo_associations);
        // The modal warns the user that uncommitted changes will be lost;
        // the background cleanup thread always runs with force=true.
        // See `open_delete_prompt` for why we do not shell out to
        // `git status --porcelain` on the UI thread.

        // Phases 2-6: backend delete, session kill, in-flight cancellation,
        // in-memory state cleanup. Resource cleanup (worktree/branch/PR)
        // runs on the background thread below via spawn_delete_cleanup.
        let mut warnings: Vec<String> = Vec::new();
        let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
        if let Err(e) =
            self.delete_work_item_by_id(&work_item_id, &mut warnings, &mut orphan_worktrees)
        {
            // Backend delete failed; nothing was spawned. Close the modal
            // and surface the error as an alert.
            self.cancel_delete_prompt();
            self.alert_message = Some(format!("Delete error: {e}"));
            return;
        }

        // Phase 5 may have captured an in-flight worktree-create result
        // whose worktree is now orphaned. Forward each orphan to the
        // background cleanup thread by synthesizing a `DeleteCleanupInfo`
        // (no PR, no remote - this is a fresh worktree with no PR yet)
        // so both `git worktree remove` and `git branch -D` run off the
        // UI thread. `branch_in_main_worktree: false` is correct by
        // construction - a freshly-created worktree is never the main
        // worktree.
        for orphan in orphan_worktrees {
            cleanup_infos.push(DeleteCleanupInfo {
                repo_path: orphan.repo_path,
                branch: orphan.branch,
                worktree_path: Some(orphan.worktree_path),
                branch_in_main_worktree: false,
                open_pr_number: None,
                github_remote: None,
            });
        }

        // -- Phase 7: Clear identity trackers and reassemble --
        self.selected_work_item = None;
        self.selected_unlinked_branch = None;
        self.selected_review_request_branch = None;

        let old_idx = self.selected_item;
        self.reassemble_work_items();
        self.build_display_list();
        self.fetcher_repos_changed = true;

        // Try to keep cursor near the old position instead of jumping to
        // the first item. If the old index is still valid, prefer it.
        if let Some(old) = old_idx {
            let mut found = false;
            for i in (0..self.display_list.len().min(old + 1)).rev() {
                if is_selectable(&self.display_list[i]) {
                    self.selected_item = Some(i);
                    found = true;
                    break;
                }
            }
            if !found {
                self.selected_item = None;
                for i in 0..self.display_list.len() {
                    if is_selectable(&self.display_list[i]) {
                        self.selected_item = Some(i);
                        break;
                    }
                }
            }
        }
        self.sync_selection_identity();
        self.shell.focus = FocusPanel::Left;

        // Spawn the background cleanup thread. Keep the modal visible and
        // flip it into the in-progress state; poll_delete_cleanup closes
        // it on completion and surfaces the final status/alert.
        self.delete_in_progress = true;
        if cleanup_infos.is_empty() {
            // No git/GitHub cleanup needed (e.g. work item never had a
            // worktree). Still go through finish_delete_cleanup so the
            // dialog closes via the same code path and warnings are
            // surfaced uniformly.
            self.finish_delete_cleanup(Vec::new(), Vec::new(), warnings);
        } else {
            // Stash the synchronous-phase warnings so poll_delete_cleanup
            // can merge them with the background thread's warnings when
            // the dialog closes. Previously these were dropped on the
            // floor in this branch, silently hiding Phase 2/Phase 5
            // errors from the user.
            self.delete_sync_warnings = warnings;
            // show_status_activity=false: the modal already shows a
            // spinner, a duplicate status-bar indicator would just be
            // noise. `force=true` is always passed because the modal
            // body warns the user that uncommitted changes will be lost.
            self.spawn_delete_cleanup(cleanup_infos, true, false);
        }
    }

    /// Finalize the modal delete flow after the background cleanup thread
    /// returns (or is skipped because there was nothing to clean up).
    /// Closes the modal, applies PR-eviction tracking, and surfaces
    /// either a success status message or an error alert.
    pub(super) fn finish_delete_cleanup(
        &mut self,
        cleanup_warnings: Vec<String>,
        closed_pr_branches: Vec<(PathBuf, String)>,
        mut pre_warnings: Vec<String>,
    ) {
        self.delete_in_progress = false;
        self.delete_prompt_visible = false;
        self.delete_target_wi_id = None;
        self.delete_target_title = None;

        if !closed_pr_branches.is_empty() {
            self.cleanup_evicted_branches.extend(closed_pr_branches);
            self.apply_cleanup_evictions();
            self.reassemble_work_items();
            self.build_display_list();
        }

        pre_warnings.extend(cleanup_warnings);
        if pre_warnings.is_empty() {
            self.shell.status_message = Some("Work item deleted".into());
        } else {
            self.alert_message = Some(format!(
                "Deleted with warnings: {}",
                pre_warnings.join("; ")
            ));
        }
    }

    /// Advance the selected work item to the next workflow stage.
    /// Persists the change via `backend.update_status()` and reassembles.
    /// When transitioning from Implementing to Review, runs the plan-based
    /// review gate if a plan exists.
    pub fn advance_stage(&mut self) {
        let Some(wi_id) = self.selected_work_item_id() else {
            return;
        };
        let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
            return;
        };
        if wi.status_derived {
            self.shell.status_message = Some("Status is derived from merged PR".into());
            return;
        }
        // Review request items cannot be manually advanced.  The only way
        // to complete them is via the approve/request-changes MCP tools.
        if wi.kind == WorkItemKind::ReviewRequest {
            self.shell.status_message =
                Some("Use approve/request-changes in the Claude session".into());
            return;
        }
        let current_status = wi.status;
        // Capture the branch invariant state before giving up our
        // borrow of `wi` below. `has_branch` is true when at least one
        // repo association already has a branch name; if false, the
        // Backlog -> Planning branch below opens the recovery dialog
        // instead of persisting a stage change that would produce a
        // stuck "Planning with no branch" item on disk.
        let has_branch = wi.repo_associations.iter().any(|a| a.branch.is_some());
        let Some(new_status) = current_status.next_stage() else {
            self.shell.status_message = Some("Already at final stage".into());
            return;
        };

        // Branch invariant: a work item must carry at least one branch
        // name by the time it leaves Backlog (everything past Backlog
        // implies "somebody is actively working on this branch"). The
        // only natural Backlog transition is -> Planning, but we gate on
        // the source status rather than the target so any future
        // Backlog -> X path inherits the same enforcement without a
        // silent gap. When the invariant fails, open the recovery dialog
        // so the user can set a branch and resume; the dialog re-drives
        // `apply_stage_change` on confirm (see
        // `confirm_set_branch_dialog`).
        if current_status == WorkItemStatus::Backlog && !has_branch {
            self.open_set_branch_dialog(
                wi_id.clone(),
                crate::create_dialog::PendingBranchAction::Advance {
                    from: current_status,
                    to: new_status,
                },
            );
            return;
        }

        // Planning -> Implementing is automatic (triggered by workbridge_set_plan).
        // Block manual advance to prevent skipping the plan handoff.
        if current_status == WorkItemStatus::Planning && new_status == WorkItemStatus::Implementing
        {
            self.shell.status_message =
                Some("Plan must be set via Claude session (workbridge_set_plan)".into());
            return;
        }

        // Review gate: each item gets its own async gate that must approve
        // the transition. Multiple gates can run concurrently for different
        // work items.
        if (current_status == WorkItemStatus::Implementing
            || current_status == WorkItemStatus::Blocked)
            && new_status == WorkItemStatus::Review
        {
            match self.spawn_review_gate(&wi_id, ReviewGateOrigin::Tui) {
                ReviewGateSpawn::Spawned => {
                    // Gate is running in background - do not advance yet.
                }
                ReviewGateSpawn::Blocked(reason) => {
                    self.shell.status_message = Some(reason);
                }
            }
            return;
        }

        // Merge prompt: when transitioning from Review to Done,
        // show the merge strategy prompt instead of advancing directly.
        //
        // The unclean-worktree merge guard used to live here as a
        // synchronous read against the cached `repo_data` worktree
        // info. That cached path stayed stale across long sessions
        // and would refuse to open the modal even after the user had
        // committed and pushed minutes ago. The authoritative merge
        // guard now lives in `execute_merge` as a background
        // `WorktreeService::list_worktrees` precheck (see
        // `spawn_merge_precheck` / `poll_merge_precheck`); having a
        // second cached guard here would short-circuit the live check
        // and re-introduce exactly the stale-cache failure mode the
        // precheck was added to fix. So this branch unconditionally
        // opens the strategy picker - the live precheck classifies
        // the worktree before the actual `gh pr merge` thread fires
        // and surfaces the same dirty/untracked/unpushed wording as
        // an alert if it blocks. `BehindOnly` and `Clean` continue to
        // proceed to the merge as before.
        if current_status == WorkItemStatus::Review && new_status == WorkItemStatus::Done {
            self.confirm_merge = true;
            self.merge_wi_id = Some(wi_id);
            return;
        }

        // Mergequeue items are waiting for an external merge - block manual advance.
        if current_status == WorkItemStatus::Mergequeue {
            self.shell.status_message =
                Some("Waiting for PR to be merged - retreat with Shift+Left to cancel".into());
            return;
        }

        self.apply_stage_change(&wi_id, current_status, new_status, "user");
    }

    /// Retreat the selected work item to the previous workflow stage.
    /// Persists the change via `backend.update_status()` and reassembles.
    pub fn retreat_stage(&mut self) {
        let Some(wi_id) = self.selected_work_item_id() else {
            return;
        };
        let Some(wi) = self.work_items.iter().find(|w| w.id == wi_id) else {
            return;
        };
        if wi.status_derived {
            self.shell.status_message = Some("Status is derived from merged PR".into());
            return;
        }
        // Review request items cannot retreat - there is no valid previous
        // stage for a review request in Review.
        if wi.kind == WorkItemKind::ReviewRequest {
            self.shell.status_message = Some("Review request items cannot be retreated".into());
            return;
        }
        let current_status = wi.status;
        let Some(new_status) = current_status.prev_stage() else {
            self.shell.status_message = Some("Already at first stage".into());
            return;
        };

        // If the retreating item has a pending review gate, cancel it.
        // The gate result would be stale since the user intentionally moved away.
        self.drop_review_gate(&wi_id);

        // Cancel any in-flight PR merge. Merges are only spawned from Review,
        // so when retreating from Review we drop the helper entry to prevent
        // poll_pr_merge from applying a stale result. The background thread
        // will finish on its own; we just ignore its result.
        //
        // The merge can be in either of two phases here:
        // 1. Live precheck (`UserActionPayload::PrMergePrecheck`).
        // 2. Actual `gh pr merge` (`UserActionPayload::PrMerge`).
        // Both phases share the same `UserActionKey::PrMerge` slot, so
        // `is_user_action_in_flight` is the single check that covers
        // them and `end_user_action` drops both receivers structurally
        // because they live inside the slot's payload (no sibling
        // `Option<Receiver>` field to forget).
        if current_status == WorkItemStatus::Review
            && self.is_user_action_in_flight(&UserActionKey::PrMerge)
        {
            self.end_user_action(&UserActionKey::PrMerge);
            self.merge_in_progress = false;
            self.confirm_merge = false;
            self.merge_wi_id = None;
        }

        // Cancel any in-flight or pending PR creation for the retreating item.
        // PR creation is spawned when entering Review; retreating means the user
        // no longer wants the PR. Drop the helper entry so poll_pr_creation
        // ignores the result, and remove the item from the pending queue.
        if current_status == WorkItemStatus::Review {
            if self.user_action_work_item(&UserActionKey::PrCreate) == Some(&wi_id) {
                self.end_user_action(&UserActionKey::PrCreate);
            }
            self.pr_create_pending.retain(|id| *id != wi_id);
        }

        // Clean up mergequeue watch and in-flight poll when retreating
        // from Mergequeue back to Review. The poll map is keyed by
        // WorkItemId, so removing this item's entry leaves polls for
        // other Mergequeue items untouched.
        if current_status == WorkItemStatus::Mergequeue {
            self.mergequeue_watches.retain(|w| w.wi_id != wi_id);
            self.mergequeue_poll_errors.remove(&wi_id);
            if let Some(state) = self.mergequeue_polls.remove(&wi_id) {
                self.activities.end(state.activity);
            }
        }

        // Rework prompt: when retreating from Review to Implementing,
        // show a text input for the rework reason instead of retreating directly.
        if current_status == WorkItemStatus::Review && new_status == WorkItemStatus::Implementing {
            self.rework_prompt_visible = true;
            self.rework_prompt_input.clear();
            self.rework_prompt_wi = Some(wi_id);
            return;
        }

        self.apply_stage_change(&wi_id, current_status, new_status, "user");
    }

    /// Move a blocked (no-plan) work item back to Planning so that Claude
    /// can analyze existing branch work and produce a retroactive plan.
    pub fn plan_from_branch(&mut self, wi_id: &WorkItemId) {
        // Guard: verify the work item is actually in Blocked state. MCP events
        // can change the status while the no-plan prompt is visible, so the
        // item may no longer be Blocked by the time the user responds.
        let is_blocked = self
            .work_items
            .iter()
            .find(|w| w.id == *wi_id)
            .is_some_and(|w| w.status == WorkItemStatus::Blocked);
        if !is_blocked {
            self.shell.status_message = Some("Work item is no longer blocked".into());
            return;
        }

        // Transition first, then clear the plan. Only clear the plan if
        // the transition actually succeeded (the work item is now Planning).
        let current = WorkItemStatus::Blocked;
        let next = WorkItemStatus::Planning;
        self.apply_stage_change(wi_id, current, next, "user");

        let is_planning = self
            .work_items
            .iter()
            .find(|w| w.id == *wi_id)
            .is_some_and(|w| w.status == WorkItemStatus::Planning);
        if !is_planning {
            // apply_stage_change already set a status_message with the error.
            return;
        }

        // Clear the plan so the planning session starts fresh.
        if let Err(e) = self.services.backend.update_plan(wi_id, "") {
            self.shell.status_message = Some(format!("Could not clear plan: {e}"));
        }
    }

    /// Shared logic for applying a stage change: log it, persist it, reassemble.
    ///
    /// Transitions to Done are only allowed when `source == "pr_merge"` or
    /// `source == "review_submitted"`, enforcing the merge-gate invariant
    /// at the chokepoint rather than relying on caller discipline alone.
    pub fn apply_stage_change(
        &mut self,
        wi_id: &WorkItemId,
        current_status: WorkItemStatus,
        new_status: WorkItemStatus,
        source: &str,
    ) {
        // Merge-gate guard: Done requires a verified PR merge or a
        // submitted review.  All other callers must go through the merge
        // prompt / poll_pr_merge path (source == "pr_merge") or the review
        // submission path (source == "review_submitted").
        if new_status == WorkItemStatus::Done
            && source != "pr_merge"
            && source != "review_submitted"
        {
            self.shell.status_message = Some("Cannot move to Done without a merged PR".to_string());
            return;
        }

        let entry = ActivityEntry {
            timestamp: now_iso8601(),
            event_type: "stage_change".to_string(),
            payload: serde_json::json!({
                "from": format!("{:?}", current_status),
                "to": format!("{:?}", new_status),
                "source": source
            }),
        };
        if let Err(e) = self.services.backend.append_activity(wi_id, &entry) {
            self.shell.status_message = Some(format!("Activity log error: {e}"));
        }

        if let Err(e) = self.services.backend.update_status(wi_id, new_status) {
            self.shell.status_message = Some(format!("Stage update error: {e}"));
            return;
        }

        // Track when items enter/leave Done for auto-archival.
        let mut done_at_error = false;
        if new_status == WorkItemStatus::Done {
            match crate::side_effects::clock::system_now().duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => {
                    if let Err(e) = self
                        .services
                        .backend
                        .set_done_at(wi_id, Some(duration.as_secs()))
                    {
                        self.shell.status_message =
                            Some(format!("Failed to set archive timestamp: {e}"));
                        done_at_error = true;
                    }
                }
                Err(e) => {
                    self.shell.status_message = Some(format!(
                        "System clock error, skipping archive timestamp: {e}"
                    ));
                    done_at_error = true;
                }
            }
        } else if current_status == WorkItemStatus::Done
            && let Err(e) = self.services.backend.set_done_at(wi_id, None)
        {
            self.shell.status_message = Some(format!("Failed to clear archive timestamp: {e}"));
            done_at_error = true;
        }

        self.reassemble_work_items();
        self.build_display_list();
        if !done_at_error {
            self.shell.status_message = Some(format!("Moved to {}", new_status.badge_text()));
        }

        // Feature 1: Auto-create PR when entering Review (async).
        // Skip for review requests - the PR already exists (it's someone else's).
        let is_review_request = self
            .work_items
            .iter()
            .find(|w| w.id == *wi_id)
            .is_some_and(|w| w.kind == WorkItemKind::ReviewRequest);
        if new_status == WorkItemStatus::Review && !is_review_request {
            self.spawn_pr_creation(wi_id);
        }

        // Cancel any pending session-open plan-read for this work item
        // BEFORE the session kill block. The plan-read receiver lives in
        // `session_open_rx` (no entry in `self.sessions` yet), so the
        // session-kill branch below would not see it; without this
        // unconditional cancel, a stale pending open from the old
        // stage would survive the transition and `finish_session_open`
        // would later spawn the agent for the new stage - including
        // no-session stages like Done or Mergequeue. Cancelling the
        // entry here also signals the worker to skip remaining file
        // writes, routes the committed tempfile through
        // `spawn_agent_file_cleanup`, and ends the
        // "Opening session..." spinner.
        self.cancel_session_open_entry(wi_id);

        // Kill the old session for this work item before spawning a new one.
        // Previously relied on orphan cleanup in check_liveness, but that
        // leaves two sessions alive briefly and the old one can do work
        // (push, commit, etc.) in the gap.
        if let Some(old_key) = self.session_key_for(wi_id)
            && let Some(mut entry) = self.sessions.remove(&old_key)
        {
            if let Some(ref mut session) = entry.session {
                session.kill();
            }
            self.cleanup_session_state_for(wi_id);
        }

        // Auto-spawn a session for stages that have prompts.
        if !matches!(
            new_status,
            WorkItemStatus::Backlog | WorkItemStatus::Done | WorkItemStatus::Mergequeue
        ) {
            self.spawn_session(wi_id);
        }
    }
}
