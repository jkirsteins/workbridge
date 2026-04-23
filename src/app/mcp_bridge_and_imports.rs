//! MCP bridge subsystem + unlinked/review-request import.
//!
//! Drains MCP socket-server events on every tick
//! (`poll_mcp_status_updates`) so work-item agent-working state
//! stays in sync with live tool-call activity. Also owns the
//! import operations that promote a fetched unlinked-PR or
//! review-request row into a tracked work item
//! (`import_selected_unlinked`, `import_selected_review_request`,
//! `selected_pr_target`). Grouped because both surfaces read the
//! same selection-derived PR identity.

use std::path::PathBuf;

use super::{
    DeleteCleanupInfo, DisplayEntry, FocusPanel, OrphanWorktree, ReviewGateOrigin, ReviewGateSpawn,
    UserActionKey, is_selectable, now_iso8601,
};
use crate::mcp::McpEvent;
use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::{ActivityEntry, CreateWorkItem, RepoAssociationRecord};

impl super::App {
    /// Drain MCP events from the crossbeam channel.
    /// Called on the 200ms timer tick. Processes status updates, log events,
    /// and plan updates from all active MCP socket servers.
    pub fn poll_mcp_status_updates(&mut self) {
        let Some(ref rx) = self.mcp_rx else {
            return;
        };

        let mut events: Vec<McpEvent> = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        for event in events {
            self.dispatch_mcp_event(event);
        }
    }

    /// Route a single MCP event through its per-variant handler. The
    /// outer `poll_mcp_status_updates` collects events into a `Vec`
    /// first (so each handler can freely borrow `&mut self`) and
    /// then delegates to this dispatcher one event at a time.
    fn dispatch_mcp_event(&mut self, event: McpEvent) {
        match event {
            McpEvent::StatusUpdate {
                work_item_id: wi_id_str,
                status: status_str,
                reason,
            } => {
                self.handle_mcp_status_update(&wi_id_str, &status_str, &reason);
            }
            McpEvent::LogEvent {
                work_item_id: wi_id_str,
                event_type,
                payload,
            } => {
                let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        self.shell.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                        return;
                    }
                };
                let entry = ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type,
                    payload,
                };
                if let Err(e) = self.services.backend.append_activity(&wi_id, &entry) {
                    self.shell.status_message = Some(format!("Activity log error: {e}"));
                }
            }
            McpEvent::SetPlan {
                work_item_id: wi_id_str,
                plan,
            } => {
                self.handle_mcp_set_plan(&wi_id_str, &plan);
            }
            McpEvent::SetTitle {
                work_item_id: wi_id_str,
                title,
            } => {
                let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        self.shell.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                        return;
                    }
                };
                if let Err(e) = self.services.backend.update_title(&wi_id, &title) {
                    self.shell.status_message = Some(format!("Title update error: {e}"));
                } else {
                    self.reassemble_work_items();
                    self.build_display_list();
                }
            }
            McpEvent::SetActivity {
                work_item_id: wi_id_str,
                working,
            } => {
                let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                    Ok(id) => id,
                    Err(e) => {
                        self.shell.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                        return;
                    }
                };
                if working {
                    self.agent_working.insert(wi_id);
                } else {
                    self.agent_working.remove(&wi_id);
                }
            }
            McpEvent::DeleteWorkItem {
                work_item_id: wi_id_str,
            } => {
                self.handle_mcp_delete_work_item(&wi_id_str);
            }
            McpEvent::SubmitReview {
                work_item_id: wi_id_str,
                action,
                comment,
            } => {
                self.handle_mcp_submit_review(&wi_id_str, &action, &comment);
            }
            McpEvent::ReviewGateProgress {
                work_item_id: wi_id_str,
                message,
            } => {
                if let Ok(wi_id) = serde_json::from_str::<WorkItemId>(&wi_id_str)
                    && let Some(gate) = self.review_gates.get_mut(&wi_id)
                {
                    gate.progress = Some(message);
                }
            }
            McpEvent::CreateWorkItem {
                title,
                description,
                repo_path,
            } => {
                self.handle_mcp_create_work_item(&title, description, &repo_path);
            }
        }
    }

    /// Handle an MCP `StatusUpdate` event: parse the serialized work
    /// item id + status string, enforce the MCP-specific transition
    /// rules (no Done, no derived-status flip, no review-request
    /// drive, allowed-transition table), and route Review transitions
    /// through the review gate.
    fn handle_mcp_status_update(&mut self, wi_id_str: &str, status_str: &str, reason: &str) {
        let new_status = match status_str {
            "Backlog" => WorkItemStatus::Backlog,
            "Planning" => WorkItemStatus::Planning,
            "Implementing" => WorkItemStatus::Implementing,
            "Blocked" => WorkItemStatus::Blocked,
            "Review" => WorkItemStatus::Review,
            "Done" => WorkItemStatus::Done,
            other => {
                self.shell.status_message = Some(format!("MCP: unrecognized status '{other}'"));
                return;
            }
        };

        // Find the work item ID from the serialized string.
        let wi_id = match serde_json::from_str::<WorkItemId>(wi_id_str) {
            Ok(id) => id,
            Err(e) => {
                self.shell.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                return;
            }
        };

        // Block Done via MCP - Done requires the merge gate which is
        // user-initiated. Allowing MCP to set Done would bypass both
        // the review gate and the merge gate.
        if new_status == WorkItemStatus::Done {
            self.shell.status_message =
                Some("MCP: cannot set Done directly (use the merge gate)".into());
            return;
        }

        let wi_ref = self.work_items.iter().find(|w| w.id == wi_id);

        // Block transitions on derived statuses (e.g. merged PR -> Done)
        // to prevent backend/display divergence, mirroring advance/retreat_stage.
        if wi_ref.is_some_and(|w| w.status_derived) {
            self.shell.status_message = Some("MCP: status is derived from merged PR".into());
            return;
        }

        // Block all MCP transitions for review request items. Claude
        // sessions should not drive workflow for someone else's PR.
        if wi_ref.is_some_and(|w| w.kind == WorkItemKind::ReviewRequest) {
            self.shell.status_message =
                Some("MCP: status transitions not supported for review request items".into());
            return;
        }

        let current_status = wi_ref.map(|w| w.status);

        // Restrict MCP to valid forward transitions only.
        let allowed = matches!(
            (&current_status, &new_status),
            (
                Some(WorkItemStatus::Implementing | WorkItemStatus::Blocked),
                WorkItemStatus::Review
            ) | (Some(WorkItemStatus::Implementing), WorkItemStatus::Blocked)
                | (
                    Some(WorkItemStatus::Blocked | WorkItemStatus::Planning),
                    WorkItemStatus::Implementing
                )
        );
        if !allowed {
            self.shell.status_message = Some(format!(
                "MCP: transition from {} to {} is not allowed",
                current_status.map_or("unknown", |s| s.badge_text()),
                new_status.badge_text()
            ));
            return;
        }

        // No-plan prompt: when Claude blocks because there is no
        // implementation plan, offer the user a choice to retreat to
        // Planning instead of staying blocked.
        if let Some(current) = current_status
            && current == WorkItemStatus::Implementing
            && new_status == WorkItemStatus::Blocked
            && reason.contains("No implementation plan")
        {
            // Apply the block first so the item is in Blocked state.
            self.apply_stage_change(&wi_id, current, new_status, "mcp");
            if !self.no_plan_prompt_queue.contains(&wi_id) {
                self.no_plan_prompt_queue.push_back(wi_id);
            }
            if !self.prompt_flags.no_plan_visible {
                self.prompt_flags.no_plan_visible = true;
            }
            return;
        }

        // Review gate: when MCP requests Implementing/Blocked ->
        // Review, a per-item review gate must approve the transition.
        if (current_status.as_ref() == Some(&WorkItemStatus::Implementing)
            || current_status.as_ref() == Some(&WorkItemStatus::Blocked))
            && new_status == WorkItemStatus::Review
        {
            match self.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp) {
                ReviewGateSpawn::Spawned => {
                    self.shell.status_message =
                        Some("Claude requested Review - running review gate...".into());
                }
                ReviewGateSpawn::Blocked(reason) => {
                    self.shell.status_message = Some(reason);
                }
            }
            return;
        }
        // Non-Review transitions fall through to direct update.
        let Some(current) = current_status else {
            return;
        };
        self.apply_stage_change(&wi_id, current, new_status, "mcp");
        let existing = self.shell.status_message.take();
        self.shell.status_message = Some(format_mcp_transition_status(
            existing.as_deref(),
            new_status,
            reason,
        ));
    }

    /// Handle an MCP `SubmitReview` event: enforce that the target
    /// is a review-request item in Review and forward to
    /// `spawn_review_submission` so the actual `gh pr review` shell
    /// out runs on a background thread.
    fn handle_mcp_submit_review(&mut self, wi_id_str: &str, action: &str, comment: &str) {
        let wi_id = match serde_json::from_str::<WorkItemId>(wi_id_str) {
            Ok(id) => id,
            Err(e) => {
                self.shell.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                return;
            }
        };
        let wi = self.work_items.iter().find(|w| w.id == wi_id);
        if !wi.is_some_and(|w| w.kind == WorkItemKind::ReviewRequest) {
            self.shell.status_message =
                Some("MCP: review tools only work on review request items".into());
            return;
        }
        if !wi.is_some_and(|w| w.status == WorkItemStatus::Review) {
            self.shell.status_message = Some("MCP: review request is not in Review status".into());
            return;
        }
        self.spawn_review_submission(&wi_id, action, comment);
    }

    /// Handle an MCP `SetPlan` event: persist the plan text through
    /// the backend, log a `plan_set` activity entry, and auto-advance
    /// from Planning to Implementing when the backend record is still
    /// in Planning.
    fn handle_mcp_set_plan(&mut self, wi_id_str: &str, plan: &str) {
        let wi_id = match serde_json::from_str::<WorkItemId>(wi_id_str) {
            Ok(id) => id,
            Err(e) => {
                self.shell.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                return;
            }
        };
        if let Err(e) = self.services.backend.update_plan(&wi_id, plan) {
            self.shell.status_message = Some(format!("Plan update error: {e}"));
            return;
        }
        // Log the plan set event to the activity log.
        let entry = ActivityEntry {
            timestamp: now_iso8601(),
            event_type: "plan_set".to_string(),
            payload: serde_json::json!({
                "source": "mcp",
                "plan_length": plan.len()
            }),
        };
        if let Err(e) = self.services.backend.append_activity(&wi_id, &entry) {
            self.shell.status_message = Some(format!("Activity log error: {e}"));
        } else {
            self.shell.status_message = Some("Plan saved by Claude".to_string());
        }

        // Auto-advance from Planning to Implementing when plan is set.
        // Read authoritative status from disk rather than the
        // in-memory cache, which may be stale. The orphan cleanup
        // in check_liveness will kill the Planning session.
        match self.services.backend.read(&wi_id) {
            Ok(record) if record.status == WorkItemStatus::Planning => {
                self.apply_stage_change(
                    &wi_id,
                    WorkItemStatus::Planning,
                    WorkItemStatus::Implementing,
                    "mcp",
                );
            }
            Ok(_) => {}
            Err(e) => {
                self.shell.status_message =
                    Some(format!("Plan saved but could not verify status: {e}"));
            }
        }
    }

    /// Handle an MCP `CreateWorkItem` event from the global
    /// assistant: validate the target repo, synthesize a branch name
    /// (`$USER/workitem-<suffix>`), create the backend record in
    /// Planning, close the drawer, and spawn a planning session.
    fn handle_mcp_create_work_item(&mut self, title: &str, description: String, repo_path: &str) {
        let repo = PathBuf::from(repo_path);

        // Validate that the repo exists in active_repo_cache.
        let repo_valid = self
            .active_repo_cache
            .iter()
            .any(|r| r.path == repo && r.git_dir_present);
        if !repo_valid {
            self.shell.status_message = Some(format!(
                "MCP: repo '{repo_path}' not found or has no git dir"
            ));
            return;
        }

        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        let suffix = crate::create_dialog::random_suffix();
        let branch = format!("{username}/workitem-{suffix}");

        let request = CreateWorkItem {
            title: title.to_string(),
            description: Some(description),
            status: WorkItemStatus::Planning,
            kind: WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: repo,
                branch: Some(branch),
                pr_identity: None,
            }],
        };

        match self.services.backend.create(request) {
            Ok(record) => {
                let wi_id = record.id;
                self.reassemble_work_items();
                self.fetcher_flags.repos_changed = true;
                self.selected_work_item = Some(wi_id.clone());
                self.build_display_list();

                // Close the global drawer and spawn the planning session.
                self.global_drawer.open = false;
                self.shell.focus = self.global_drawer.pre_drawer_focus;
                self.spawn_session(&wi_id);
                self.shell.status_message = Some(format!("Created work item: {title}"));
            }
            Err(e) => {
                self.shell.status_message = Some(format!("MCP: failed to create work item: {e}"));
            }
        }
    }

    /// Handle an MCP `DeleteWorkItem` event: parse the work item id,
    /// refuse if another delete cleanup is already running, run the
    /// non-blocking backend delete + session teardown, and forward
    /// resource cleanup (worktree remove, branch delete, PR close)
    /// to the background cleanup thread.
    fn handle_mcp_delete_work_item(&mut self, wi_id_str: &str) {
        let wi_id = match serde_json::from_str::<WorkItemId>(wi_id_str) {
            Ok(id) => id,
            Err(e) => {
                self.shell.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                return;
            }
        };

        // Guard against concurrent cleanup: if a prior delete (from
        // the modal OR a previous MCP call) is still running, refuse
        // THIS delete before touching the backend. Without this check
        // the backend record and session would be destroyed but
        // spawn_delete_cleanup would early-return, silently orphaning
        // the worktree, branch, and open PR. Mirror the modal's guard
        // (confirm_delete_from_prompt) here so both entry points have
        // the same ordering: check availability -> delete backend ->
        // spawn cleanup.
        if self.is_user_action_in_flight(&UserActionKey::DeleteCleanup) {
            self.alert_message = Some(
                "MCP delete refused: another delete cleanup is still \
                     in progress. Wait for it to finish and try again."
                    .into(),
            );
            return;
        }

        // Gather repo associations from the assembled work item.
        let repo_associations: Vec<crate::work_item_backend::RepoAssociationRecord> = self
            .work_items
            .iter()
            .find(|w| w.id == wi_id)
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

        // Gather cleanup info BEFORE deleting (needs repo_data lookups).
        let mut cleanup_infos = self.gather_delete_cleanup_infos(&repo_associations);

        // Non-blocking phases: backend delete, session kill, in-memory
        // cleanup. Resource cleanup (worktree removal, branch
        // deletion, PR close) runs on a background thread below
        // via `spawn_delete_cleanup`.
        let mut warnings: Vec<String> = Vec::new();
        let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
        if let Err(e) = self.delete_work_item_by_id(&wi_id, &mut warnings, &mut orphan_worktrees) {
            self.shell.status_message = Some(format!("MCP delete error: {e}"));
            return;
        }

        // Phase 5 may have captured an in-flight worktree-create
        // result whose worktree is now orphaned. Forward each
        // orphan to the background cleanup thread by synthesizing
        // a `DeleteCleanupInfo`.
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

        // Spawn background thread for blocking resource cleanup.
        if !cleanup_infos.is_empty() {
            self.spawn_delete_cleanup(cleanup_infos, true, true);
        }

        // Clear selection identity if the deleted item was selected.
        if self.selected_work_item_id().is_some_and(|id| id == wi_id) {
            self.selected_work_item = None;
            self.selected_unlinked_branch = None;
            self.selected_review_request_branch = None;
        }

        let old_idx = self.selected_item;
        self.reassemble_work_items();
        self.build_display_list();
        self.fetcher_flags.repos_changed = true;

        // Re-sync cursor position.
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
        if warnings.is_empty() {
            self.shell.status_message =
                Some("Work item deleted via MCP (resource cleanup in progress)".into());
        } else {
            self.shell.status_message = Some(format!(
                "Deleted via MCP (with warnings: {})",
                warnings.join("; ")
            ));
        }
    }

    /// Import the currently selected unlinked PR as a work item.
    ///
    /// Calls `backend.import()` then spawns a background thread to fetch the
    /// branch and create a worktree. The UI remains responsive while the
    /// git operations run. Results are picked up by `poll_worktree_creation()`.
    pub fn import_selected_unlinked(&mut self) {
        let Some(idx) = self.selected_item else {
            return;
        };
        let unlinked_idx = match self.display_list.get(idx) {
            Some(DisplayEntry::UnlinkedItem(i)) => *i,
            _ => return,
        };
        let Some(unlinked) = self.unlinked_prs.get(unlinked_idx) else {
            return;
        };

        let repo_path = unlinked.repo_path.clone();
        let branch = unlinked.branch.clone();

        match self.services.backend.import(unlinked) {
            Ok(record) => {
                let title = record.title.clone();
                let wi_id = record.id;
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_flags.repos_changed = true;
                self.spawn_import_worktree(wi_id, repo_path, branch, &title);
            }
            Err(e) => {
                self.shell.status_message = Some(format!("Import error: {e}"));
            }
        }
    }

    /// Import the currently selected review-requested PR as a work item.
    ///
    /// Calls `backend.import_review_request()` then spawns a background thread
    /// to fetch the branch and create a worktree. The UI remains responsive
    /// while the git operations run.
    pub fn import_selected_review_request(&mut self) {
        let Some(idx) = self.selected_item else {
            return;
        };
        let rr_idx = match self.display_list.get(idx) {
            Some(DisplayEntry::ReviewRequestItem(i)) => *i,
            _ => return,
        };
        let Some(rr) = self.review_requested_prs.get(rr_idx) else {
            return;
        };

        let repo_path = rr.repo_path.clone();
        let branch = rr.branch.clone();

        match self.services.backend.import_review_request(rr) {
            Ok(record) => {
                let title = record.title.clone();
                let wi_id = record.id;
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_flags.repos_changed = true;
                self.spawn_import_worktree(wi_id, repo_path, branch, &title);
            }
            Err(e) => {
                self.shell.status_message = Some(format!("Import error: {e}"));
            }
        }
    }

    /// Resolve the currently selected left-panel entry to the PR URL (and a
    /// short human-readable label) that should open when the user presses
    /// `o`. Returns `None` if there is no selection, the entry is not
    /// PR-bearing (e.g. a group header), or the selected work item has no
    /// repo association with a PR attached.
    ///
    /// For work items with multiple repo associations, the first association
    /// whose `pr` field is `Some(_)` wins. This is deterministic across
    /// repeat presses because `repo_associations` preserves insertion order
    /// through reassembly.
    ///
    /// Pure: does not spawn, does not shell out, does not mutate `self`.
    /// Split out so the dispatch logic can be unit-tested without shelling
    /// out to `open`.
    pub(crate) fn selected_pr_target(&self) -> Option<(String, String)> {
        let idx = self.selected_item?;
        let entry = self.display_list.get(idx)?;
        match entry {
            DisplayEntry::WorkItemEntry(wi_idx) => {
                let wi = self.work_items.get(*wi_idx)?;
                let pr = wi.repo_associations.iter().find_map(|a| a.pr.as_ref())?;
                Some((pr.url.clone(), format!("PR #{}", pr.number)))
            }
            DisplayEntry::UnlinkedItem(u_idx) => {
                let ul = self.unlinked_prs.get(*u_idx)?;
                Some((ul.pr.url.clone(), format!("PR #{}", ul.pr.number)))
            }
            DisplayEntry::ReviewRequestItem(r_idx) => {
                let rr = self.review_requested_prs.get(*r_idx)?;
                Some((rr.pr.url.clone(), format!("PR #{}", rr.pr.number)))
            }
            DisplayEntry::GroupHeader { .. } => None,
        }
    }
}

/// Build the MCP-specific status-bar string shown after a
/// `StatusUpdate` transition applies. Preserves any PR-created detail
/// from `apply_stage_change` and appends the MCP-provided reason
/// (when non-empty) so the user sees both the stage change and the
/// rationale in one line.
fn format_mcp_transition_status(
    existing_msg: Option<&str>,
    new_status: WorkItemStatus,
    reason: &str,
) -> String {
    let existing = existing_msg.unwrap_or("");
    let pr_suffix = existing
        .find("PR created")
        .map(|idx| format!(" - {}", &existing[idx..]))
        .unwrap_or_default();
    let reason_part = if reason.is_empty() {
        String::new()
    } else {
        format!(" - {reason}")
    };
    format!(
        "Claude moved to {}{pr_suffix}{reason_part}",
        new_status.badge_text()
    )
}
