//! Subset of `impl App` methods extracted from `src/app/mod.rs`.
//!
//! The `impl App { ... }` is split across sibling files solely to
//! keep every file within the 700-line ceiling. Methods behave
//! identically to the original single-file layout.

use std::path::PathBuf;

use crate::mcp::McpEvent;
use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::{ActivityEntry, CreateWorkItem, RepoAssociationRecord};

use super::*;

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
            match event {
                McpEvent::StatusUpdate {
                    work_item_id: wi_id_str,
                    status: status_str,
                    reason,
                } => {
                    let new_status = match status_str.as_str() {
                        "Backlog" => WorkItemStatus::Backlog,
                        "Planning" => WorkItemStatus::Planning,
                        "Implementing" => WorkItemStatus::Implementing,
                        "Blocked" => WorkItemStatus::Blocked,
                        "Review" => WorkItemStatus::Review,
                        "Done" => WorkItemStatus::Done,
                        other => {
                            self.status_message =
                                Some(format!("MCP: unrecognized status '{other}'"));
                            continue;
                        }
                    };

                    // Find the work item ID from the serialized string.
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };

                    // Block Done via MCP - Done requires the merge gate
                    // which is user-initiated. Allowing MCP to set Done would
                    // bypass both the review gate and the merge gate.
                    if new_status == WorkItemStatus::Done {
                        self.status_message =
                            Some("MCP: cannot set Done directly (use the merge gate)".into());
                        continue;
                    }

                    // Check current status to route through review gate if needed.
                    let wi_ref = self.work_items.iter().find(|w| w.id == wi_id);

                    // Block transitions on derived statuses (e.g. merged PR -> Done)
                    // to prevent backend/display divergence, mirroring advance/retreat_stage.
                    if wi_ref.is_some_and(|w| w.status_derived) {
                        self.status_message = Some("MCP: status is derived from merged PR".into());
                        continue;
                    }

                    // Block all MCP transitions for review request items.
                    // Claude sessions should not drive workflow for someone else's PR.
                    if wi_ref.is_some_and(|w| w.kind == WorkItemKind::ReviewRequest) {
                        self.status_message = Some(
                            "MCP: status transitions not supported for review request items".into(),
                        );
                        continue;
                    }

                    let current_status = wi_ref.map(|w| w.status);

                    // Restrict MCP to valid forward transitions only.
                    // Allowed: Implementing -> Review (via gate), Implementing -> Blocked,
                    // Blocked -> Implementing, Blocked -> Review (via gate),
                    // Planning -> Implementing.
                    // All other transitions must go through the TUI keybinds.
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
                        self.status_message = Some(format!(
                            "MCP: transition from {} to {} is not allowed",
                            current_status.map_or("unknown", |s| s.badge_text()),
                            new_status.badge_text()
                        ));
                        continue;
                    }

                    // No-plan prompt: when Claude blocks because there is no
                    // implementation plan, offer the user a choice to retreat
                    // to Planning instead of staying blocked.
                    if let Some(current) = current_status
                        && current == WorkItemStatus::Implementing
                        && new_status == WorkItemStatus::Blocked
                        && reason.contains("No implementation plan")
                    {
                        // Apply the block first so the item is in Blocked state.
                        self.apply_stage_change(&wi_id, current, new_status, "mcp");

                        // Enqueue for the no-plan prompt (skip duplicates).
                        if !self.no_plan_prompt_queue.contains(&wi_id) {
                            self.no_plan_prompt_queue.push_back(wi_id);
                        }
                        if !self.no_plan_prompt_visible {
                            self.no_plan_prompt_visible = true;
                        }
                        continue;
                    }

                    // Review gate: when MCP requests Implementing/Blocked -> Review,
                    // a per-item review gate must approve the transition. The
                    // gate runs entirely on a background thread - any
                    // "cannot run" discovery (no plan, empty diff, git error)
                    // arrives as `ReviewGateMessage::Blocked` and is handled
                    // by `poll_review_gate` (which applies the rework flow).
                    // This main-thread path only ever sees synchronous
                    // pre-conditions (gate already running, no branch, no
                    // repo association, work item missing).
                    if (current_status.as_ref() == Some(&WorkItemStatus::Implementing)
                        || current_status.as_ref() == Some(&WorkItemStatus::Blocked))
                        && new_status == WorkItemStatus::Review
                    {
                        match self.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp) {
                            ReviewGateSpawn::Spawned => {
                                self.status_message =
                                    Some("Claude requested Review - running review gate...".into());
                            }
                            ReviewGateSpawn::Blocked(reason) => {
                                self.status_message = Some(reason);
                            }
                        }
                        continue;
                    }
                    // Non-Review transitions fall through to direct update.
                    // `current_status` is populated above from `wi_ref.map(...)`;
                    // if the work item disappeared between the map and here
                    // (it cannot, wi_ref is still live up the stack), skip
                    // the stage change rather than panic.
                    let Some(current) = current_status else {
                        continue;
                    };
                    self.apply_stage_change(&wi_id, current, new_status, "mcp");

                    // Build MCP-specific status message that preserves any
                    // detail from apply_stage_change (e.g. "PR created: URL").
                    let existing = self.status_message.take().unwrap_or_default();
                    let pr_suffix = if existing.contains("PR created") {
                        // Extract the PR info portion after the dash.
                        existing
                            .find("PR created")
                            .map(|idx| format!(" - {}", &existing[idx..]))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let reason_part = if reason.is_empty() {
                        String::new()
                    } else {
                        format!(" - {reason}")
                    };
                    self.status_message = Some(format!(
                        "Claude moved to {}{}{}",
                        new_status.badge_text(),
                        pr_suffix,
                        reason_part
                    ));
                }
                McpEvent::LogEvent {
                    work_item_id: wi_id_str,
                    event_type,
                    payload,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    let entry = ActivityEntry {
                        timestamp: now_iso8601(),
                        event_type,
                        payload,
                    };
                    if let Err(e) = self.backend.append_activity(&wi_id, &entry) {
                        self.status_message = Some(format!("Activity log error: {e}"));
                    }
                }
                McpEvent::SetPlan {
                    work_item_id: wi_id_str,
                    plan,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    if let Err(e) = self.backend.update_plan(&wi_id, &plan) {
                        self.status_message = Some(format!("Plan update error: {e}"));
                    } else {
                        // Log the plan set event to the activity log.
                        let entry = ActivityEntry {
                            timestamp: now_iso8601(),
                            event_type: "plan_set".to_string(),
                            payload: serde_json::json!({
                                "source": "mcp",
                                "plan_length": plan.len()
                            }),
                        };
                        if let Err(e) = self.backend.append_activity(&wi_id, &entry) {
                            self.status_message = Some(format!("Activity log error: {e}"));
                        } else {
                            self.status_message = Some("Plan saved by Claude".to_string());
                        }

                        // Auto-advance from Planning to Implementing when plan is set.
                        // Read authoritative status from disk rather than the
                        // in-memory cache, which may be stale. The orphan cleanup
                        // in check_liveness will kill the Planning session.
                        match self.backend.read(&wi_id) {
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
                                self.status_message =
                                    Some(format!("Plan saved but could not verify status: {e}"));
                            }
                        }
                    }
                }
                McpEvent::SetTitle {
                    work_item_id: wi_id_str,
                    title,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    if let Err(e) = self.backend.update_title(&wi_id, &title) {
                        self.status_message = Some(format!("Title update error: {e}"));
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
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
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
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
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
                        continue;
                    }

                    // Gather repo associations from the assembled work item.
                    let repo_associations: Vec<crate::work_item_backend::RepoAssociationRecord> =
                        self.work_items
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
                    if let Err(e) =
                        self.delete_work_item_by_id(&wi_id, &mut warnings, &mut orphan_worktrees)
                    {
                        self.status_message = Some(format!("MCP delete error: {e}"));
                        continue;
                    }

                    // Phase 5 may have captured an in-flight worktree-create
                    // result whose worktree is now orphaned. Forward each
                    // orphan to the background cleanup thread by synthesizing
                    // a `DeleteCleanupInfo` (no PR, no remote - this is a
                    // fresh worktree with no PR yet) so the
                    // `git worktree remove` and `git branch -D` both run off
                    // the UI thread. Running them here would be a P0
                    // blocking-I/O violation; see `docs/UI.md`.
                    // `branch_in_main_worktree: false` is correct by
                    // construction - a freshly-created worktree is never the
                    // main worktree.
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

                    // Spawn background thread for blocking resource cleanup
                    // (worktree removal, branch deletion, PR close). The MCP
                    // path always forces removal (no interactive confirmation
                    // is possible) and shows progress in the status bar
                    // because the user did not explicitly trigger the delete
                    // from a dialog.
                    if !cleanup_infos.is_empty() {
                        self.spawn_delete_cleanup(cleanup_infos, true, true);
                    }

                    // Clear selection identity if the deleted item was selected.
                    if self.selected_work_item_id() == Some(wi_id) {
                        self.selected_work_item = None;
                        self.selected_unlinked_branch = None;
                        self.selected_review_request_branch = None;
                    }

                    let old_idx = self.selected_item;
                    self.reassemble_work_items();
                    self.build_display_list();
                    self.fetcher_repos_changed = true;

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

                    self.focus = FocusPanel::Left;
                    if warnings.is_empty() {
                        self.status_message =
                            Some("Work item deleted via MCP (resource cleanup in progress)".into());
                    } else {
                        self.status_message = Some(format!(
                            "Deleted via MCP (with warnings: {})",
                            warnings.join("; ")
                        ));
                    }
                }
                McpEvent::SubmitReview {
                    work_item_id: wi_id_str,
                    action,
                    comment,
                } => {
                    let wi_id = match serde_json::from_str::<WorkItemId>(&wi_id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            self.status_message = Some(format!("MCP: invalid work item ID: {e}"));
                            continue;
                        }
                    };
                    let wi = self.work_items.iter().find(|w| w.id == wi_id);
                    if !wi.is_some_and(|w| w.kind == WorkItemKind::ReviewRequest) {
                        self.status_message =
                            Some("MCP: review tools only work on review request items".into());
                        continue;
                    }
                    if !wi.is_some_and(|w| w.status == WorkItemStatus::Review) {
                        self.status_message =
                            Some("MCP: review request is not in Review status".into());
                        continue;
                    }
                    self.spawn_review_submission(&wi_id, &action, &comment);
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
                    let repo = PathBuf::from(&repo_path);

                    // Validate that the repo exists in active_repo_cache.
                    let repo_valid = self
                        .active_repo_cache
                        .iter()
                        .any(|r| r.path == repo && r.git_dir_present);
                    if !repo_valid {
                        self.status_message = Some(format!(
                            "MCP: repo '{repo_path}' not found or has no git dir"
                        ));
                        continue;
                    }

                    let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
                    let suffix = crate::create_dialog::random_suffix();
                    let branch = format!("{username}/workitem-{suffix}");

                    let request = CreateWorkItem {
                        title: title.clone(),
                        description: Some(description),
                        status: WorkItemStatus::Planning,
                        kind: WorkItemKind::Own,
                        repo_associations: vec![RepoAssociationRecord {
                            repo_path: repo,
                            branch: Some(branch),
                            pr_identity: None,
                        }],
                    };

                    match self.backend.create(request) {
                        Ok(record) => {
                            let wi_id = record.id.clone();
                            self.reassemble_work_items();
                            self.fetcher_repos_changed = true;
                            self.selected_work_item = Some(wi_id.clone());
                            self.build_display_list();

                            // Close the global drawer and spawn the planning session.
                            self.global_drawer_open = false;
                            self.focus = self.pre_drawer_focus;
                            self.spawn_session(&wi_id);
                            self.status_message = Some(format!("Created work item: {title}"));
                        }
                        Err(e) => {
                            self.status_message =
                                Some(format!("MCP: failed to create work item: {e}"));
                        }
                    }
                }
            }
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

        match self.backend.import(unlinked) {
            Ok(record) => {
                let title = record.title.clone();
                let wi_id = record.id;
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_repos_changed = true;
                self.spawn_import_worktree(wi_id, repo_path, branch, title);
            }
            Err(e) => {
                self.status_message = Some(format!("Import error: {e}"));
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

        match self.backend.import_review_request(rr) {
            Ok(record) => {
                let title = record.title.clone();
                let wi_id = record.id;
                self.reassemble_work_items();
                self.build_display_list();
                self.fetcher_repos_changed = true;
                self.spawn_import_worktree(wi_id, repo_path, branch, title);
            }
            Err(e) => {
                self.status_message = Some(format!("Import error: {e}"));
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
