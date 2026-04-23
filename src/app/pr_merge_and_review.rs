//! PR merge polling + review submission subsystem.
//!
//! Drains the background merge-precheck channel
//! (`poll_merge_precheck`), drains the main PR-merge channel
//! (`poll_pr_merge`), and owns the review-submission flow
//! (`spawn_review_submission` / `poll_review_submission`) for
//! the reviewer role.

use std::path::PathBuf;
use std::time::Duration;

use super::{
    MergePreCheckMessage, PrMergeOutcome, PrMergeResult, REVIEW_SUBMIT_ALREADY_IN_PROGRESS,
    ReviewSubmitOutcome, ReviewSubmitResult, UserActionKey, UserActionPayload, now_iso8601,
};
use crate::work_item::{WorkItemId, WorkItemStatus};
use crate::work_item_backend::{ActivityEntry, PrIdentityRecord};

impl super::App {
    /// Drain the live merge precheck receiver on the background tick.
    ///
    /// Wired into the same ~200ms tick as every other background
    /// poller. Does nothing if the helper slot is empty or carries a
    /// non-precheck payload (e.g. the merge phase has already taken
    /// over). On a Ready message, hands off to
    /// `perform_merge_after_precheck`; on Blocked / disconnected,
    /// releases the helper slot and surfaces the reason as an alert.
    ///
    /// The receiver lives inside `UserActionPayload::PrMergePrecheck`
    /// so there is no separate `Option<Receiver>` field on `App`. If
    /// any cancel path called `end_user_action(&UserActionKey::PrMerge)`
    /// while the precheck thread was still running, the receiver was
    /// dropped in the same step and this poller's `try_recv` will
    /// either see `Empty` (slot already gone) or `Disconnected`
    /// (sender's channel half closed). The disconnected branch can
    /// no longer fire the "thread terminated unexpectedly" alert
    /// against an unrelated work item, because if the slot is gone
    /// we never enter the body in the first place.
    pub fn poll_merge_precheck(&mut self) {
        // First check: is there a precheck-phase payload at all?
        // `user_action_payload` returns `None` when the slot has
        // been released by any cancel path, which is the structural
        // equivalent of the old "merge_precheck_rx is None" guard.
        let Some(UserActionPayload::PrMergePrecheck { rx }) =
            self.user_action_payload(&UserActionKey::PrMerge)
        else {
            return;
        };
        let msg = match rx.try_recv() {
            Ok(m) => m,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                // Background thread died without sending. Release the
                // helper slot (which drops the receiver), clear the
                // modal state, and surface a generic error so the
                // user can retry.
                self.merge_flow.confirm = false;
                self.merge_wi_id = None;
                self.merge_flow.in_progress = false;
                self.end_user_action(&UserActionKey::PrMerge);
                self.alert_message = Some("Merge precheck thread terminated unexpectedly".into());
                return;
            }
        };

        match msg {
            MergePreCheckMessage::Ready {
                wi_id,
                strategy,
                branch,
                repo_path,
                owner_repo,
            } => {
                // The slot is still ours (we just observed a payload
                // above) and `perform_merge_after_precheck` will
                // replace `PrMergePrecheck` with `PrMerge` via
                // `attach_user_action_payload`, dropping the precheck
                // receiver in the same step.
                self.perform_merge_after_precheck(wi_id, strategy, branch, repo_path, owner_repo);
            }
            MergePreCheckMessage::Blocked { reason } => {
                // The live state blocks the merge. Release the
                // helper slot (which drops the precheck receiver
                // structurally), clear the modal, and surface the
                // user-facing reason. The wording for the
                // dirty / untracked / unpushed / PR-conflict /
                // CI-failing cases comes from
                // `MergeReadiness::merge_block_message`.
                self.merge_flow.confirm = false;
                self.merge_wi_id = None;
                self.merge_flow.in_progress = false;
                self.end_user_action(&UserActionKey::PrMerge);
                self.alert_message = Some(reason);
            }
        }
    }

    /// Spawn the actual `gh pr merge` background thread after the live
    /// precheck has cleared. Called from `poll_merge_precheck` on
    /// `MergePreCheckMessage::Ready` - the helper slot was already
    /// admitted in `execute_merge`, so this method does NOT re-admit
    /// the action key; it only attaches the merge payload to the
    /// already-reserved slot.
    pub(super) fn perform_merge_after_precheck(
        &mut self,
        wi_id: WorkItemId,
        strategy: String,
        branch: String,
        repo_path: PathBuf,
        owner_repo: String,
    ) {
        let merge_flag = if strategy == "merge" {
            "--merge"
        } else {
            "--squash"
        };
        let strategy_owned = strategy;
        let wi_id_clone = wi_id;
        let merge_flag_owned = merge_flag.to_string();
        let repo_path_clone = repo_path;
        let branch_for_thread = branch;
        let owner_repo_for_thread = owner_repo;

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            // Check if a PR exists for this branch and fetch its identity.
            let pr_identity = match std::process::Command::new("gh")
                .args([
                    "pr",
                    "list",
                    "--head",
                    &branch_for_thread,
                    "--json",
                    "number,title,url",
                    "--repo",
                    &owner_repo_for_thread,
                ])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    serde_json::from_str::<Vec<serde_json::Value>>(stdout.trim())
                        .ok()
                        .and_then(|arr| arr.into_iter().next())
                        .and_then(|obj| {
                            let number = obj.get("number")?.as_u64()?;
                            let title = obj.get("title")?.as_str()?.to_string();
                            let url = obj.get("url")?.as_str()?.to_string();
                            Some(PrIdentityRecord { number, title, url })
                        })
                }
                _ => None,
            };

            let outcome = if pr_identity.is_none() {
                PrMergeOutcome::NoPr
            } else {
                // Run gh pr merge.
                match std::process::Command::new("gh")
                    .args([
                        "pr",
                        "merge",
                        &branch_for_thread,
                        &merge_flag_owned,
                        "--delete-branch",
                        "--repo",
                        &owner_repo_for_thread,
                    ])
                    .output()
                {
                    Ok(output) if output.status.success() => PrMergeOutcome::Merged {
                        strategy: strategy_owned,
                        pr_identity,
                    },
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                        if stderr.to_lowercase().contains("conflict") {
                            PrMergeOutcome::Conflict { stderr }
                        } else {
                            PrMergeOutcome::Failed {
                                error: format!("Merge failed: {}", stderr.trim()),
                            }
                        }
                    }
                    Err(e) => PrMergeOutcome::Failed {
                        error: format!("Merge failed: {e}"),
                    },
                }
            };

            let _ = tx.send(PrMergeResult {
                wi_id: wi_id_clone,
                branch: branch_for_thread,
                repo_path: repo_path_clone,
                outcome,
            });
        });

        self.attach_user_action_payload(&UserActionKey::PrMerge, UserActionPayload::PrMerge { rx });
        // `merge_in_progress` was already set by `execute_merge` so the
        // modal spinner has been rendering throughout the precheck phase.
        // No need to set it again here.
    }

    /// Poll the async PR merge thread for a result. Called on each timer tick.
    pub fn poll_pr_merge(&mut self) {
        let recv_result = {
            let Some(UserActionPayload::PrMerge { rx }) =
                self.user_action_payload(&UserActionKey::PrMerge)
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
            self.end_user_action(&UserActionKey::PrMerge);
            self.merge_flow.in_progress = false;
            self.merge_flow.confirm = false;
            self.merge_wi_id = None;
            self.alert_message = Some("PR merge: background thread exited unexpectedly".into());
            return;
        };

        self.end_user_action(&UserActionKey::PrMerge);
        self.merge_flow.in_progress = false;
        self.merge_flow.confirm = false;
        self.merge_wi_id = None;

        // Guard: if the item's status changed while the merge was in-flight
        // (e.g. user retreated to Implementing), discard the stale result to
        // avoid forcing the item back to Done or deleting its worktree.
        let actual_status = self
            .work_items
            .iter()
            .find(|w| w.id == result.wi_id)
            .map(|w| w.status);

        match result.outcome {
            PrMergeOutcome::NoPr => {
                self.alert_message =
                    Some("Cannot merge: no PR found. Push branch and open a PR first.".into());
            }
            PrMergeOutcome::Merged {
                ref strategy,
                ref pr_identity,
            } => {
                self.handle_pr_merge_merged(&result, strategy, pr_identity.as_ref(), actual_status);
            }
            PrMergeOutcome::Conflict { ref stderr } => {
                self.handle_pr_merge_conflict(&result, stderr, actual_status);
            }
            PrMergeOutcome::Failed { ref error } => {
                self.alert_message = Some(error.clone());
            }
        }
    }

    /// Finalize a successful PR merge: persist PR identity, log the
    /// merge activity, and (if the item is still in Review) clean up
    /// the worktree and advance to Done.
    fn handle_pr_merge_merged(
        &mut self,
        result: &crate::app::PrMergeResult,
        strategy: &str,
        pr_identity: Option<&crate::work_item_backend::PrIdentityRecord>,
        actual_status: Option<WorkItemStatus>,
    ) {
        // Persist PR identity to backend so it survives reassembly.
        if let Some(identity) = pr_identity
            && let Err(e) =
                self.services
                    .backend
                    .save_pr_identity(&result.wi_id, &result.repo_path, identity)
        {
            self.shell.status_message = Some(format!("PR identity save error: {e}"));
        }

        // Log merge to activity log (always - the merge happened on GitHub).
        let log_entry = ActivityEntry {
            timestamp: now_iso8601(),
            event_type: "pr_merged".to_string(),
            payload: serde_json::json!({
                "strategy": strategy,
                "branch": result.branch
            }),
        };
        if let Err(e) = self
            .services
            .backend
            .append_activity(&result.wi_id, &log_entry)
        {
            self.shell.status_message = Some(format!("Activity log error: {e}"));
        }

        if actual_status.as_ref() != Some(&WorkItemStatus::Review) {
            // Item was moved away from Review while merge was in-flight.
            // The merge already happened on GitHub, but we do not change
            // the local status or delete the worktree.
            self.shell.status_message = Some(
                "PR merged on GitHub, but item status was changed - not advancing to Done"
                    .to_string(),
            );
            return;
        }

        // Clean up worktree directory.
        self.cleanup_worktree_for_item(&result.wi_id);

        // Advance to Done.
        self.apply_stage_change(
            &result.wi_id,
            WorkItemStatus::Review,
            WorkItemStatus::Done,
            "pr_merge",
        );
        self.shell.status_message = Some(format!("PR merged ({strategy}) and moved to [DN]"));
    }

    /// Handle a merge-conflict outcome: log the conflict, stash a
    /// rework reason, and bounce the item back to Implementing.
    fn handle_pr_merge_conflict(
        &mut self,
        result: &crate::app::PrMergeResult,
        stderr: &str,
        actual_status: Option<WorkItemStatus>,
    ) {
        if actual_status.as_ref() != Some(&WorkItemStatus::Review) {
            return;
        }
        // Log conflict to activity log.
        let conflict_entry = ActivityEntry {
            timestamp: now_iso8601(),
            event_type: "merge_conflict".to_string(),
            payload: serde_json::json!({
                "branch": result.branch,
                "stderr": stderr.trim()
            }),
        };
        if let Err(e) = self
            .services
            .backend
            .append_activity(&result.wi_id, &conflict_entry)
        {
            self.shell.status_message = Some(format!("Activity log error: {e}"));
        }
        let reason =
            "Merge failed due to conflicts. Rebase onto the base branch and resolve all conflicts."
                .to_string();
        self.rework_reasons.insert(result.wi_id.clone(), reason);
        self.apply_stage_change(
            &result.wi_id,
            WorkItemStatus::Review,
            WorkItemStatus::Implementing,
            "merge_conflict",
        );
        self.alert_message =
            Some("Merge conflict detected - moved back to [IM] for rebase/resolve".to_string());
    }

    /// Spawn a background thread to submit a PR review (approve or
    /// request-changes) via `gh pr review`. Results are polled by
    /// `poll_review_submission()` on each timer tick.
    pub fn spawn_review_submission(&mut self, wi_id: &WorkItemId, action: &str, comment: &str) {
        // In-flight guard via the user-action helper. Rejection message
        // is preserved verbatim.
        if self.is_user_action_in_flight(&UserActionKey::ReviewSubmit) {
            self.shell.status_message = Some(REVIEW_SUBMIT_ALREADY_IN_PROGRESS.into());
            return;
        }

        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            self.shell.status_message = Some("Cannot submit review: no repo association".into());
            return;
        };
        let branch = if let Some(b) = assoc.branch.as_ref() {
            b.clone()
        } else {
            self.shell.status_message = Some("Cannot submit review: no branch".into());
            return;
        };
        let repo_path = assoc.repo_path.clone();
        // Read owner/repo from the cached fetcher result rather than shelling
        // out on the UI thread. The first fetch populates it; until then we
        // surface a message rather than block.
        //
        // Early returns above run BEFORE try_begin_user_action so a
        // cache miss cannot leave an orphaned helper entry.
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.shell.status_message = Some(
                "Cannot submit review: GitHub remote not yet cached (waiting for next fetch)"
                    .into(),
            );
            return;
        };
        let owner_repo = format!("{owner}/{repo_name}");

        let verb = if action == "approve" {
            "Submitting approval"
        } else {
            "Requesting changes"
        };
        if self
            .try_begin_user_action(
                UserActionKey::ReviewSubmit,
                Duration::ZERO,
                format!("{verb}..."),
            )
            .is_none()
        {
            // Race with another in-flight submission; preserve the
            // pre-refactor wording.
            self.shell.status_message = Some(REVIEW_SUBMIT_ALREADY_IN_PROGRESS.into());
            return;
        }
        let action_owned = action.to_string();
        let comment_owned = comment.to_string();
        let wi_id_clone = wi_id.clone();

        let (tx, rx) = crossbeam_channel::bounded(1);

        std::thread::spawn(move || {
            let review_flag = if action_owned == "approve" {
                "--approve"
            } else {
                "--request-changes"
            };
            let mut args = vec![
                "pr".to_string(),
                "review".to_string(),
                branch,
                review_flag.to_string(),
                "--repo".to_string(),
                owner_repo,
            ];
            if !comment_owned.is_empty() {
                args.push("--body".to_string());
                args.push(comment_owned);
            }

            let outcome = match std::process::Command::new("gh").args(&args).output() {
                Ok(output) if output.status.success() => ReviewSubmitOutcome::Success,
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    ReviewSubmitOutcome::Failed {
                        error: format!("Review submission failed: {}", stderr.trim()),
                    }
                }
                Err(e) => ReviewSubmitOutcome::Failed {
                    error: format!("Review submission failed: {e}"),
                },
            };

            let _ = tx.send(ReviewSubmitResult {
                wi_id: wi_id_clone,
                action: action_owned,
                outcome,
            });
        });

        self.attach_user_action_payload(
            &UserActionKey::ReviewSubmit,
            UserActionPayload::ReviewSubmit {
                rx,
                wi_id: wi_id.clone(),
            },
        );
    }

    /// Poll the asynchronous review submission result.
    /// Called on the 200ms timer tick.
    pub fn poll_review_submission(&mut self) {
        let recv_result = {
            let Some(UserActionPayload::ReviewSubmit { rx, .. }) =
                self.user_action_payload(&UserActionKey::ReviewSubmit)
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
            self.end_user_action(&UserActionKey::ReviewSubmit);
            self.shell.status_message =
                Some("Review submission: background thread exited unexpectedly".into());
            return;
        };

        self.end_user_action(&UserActionKey::ReviewSubmit);

        match result.outcome {
            ReviewSubmitOutcome::Success => {
                let verb = if result.action == "approve" {
                    "approved"
                } else {
                    "changes requested"
                };

                let log_entry = ActivityEntry {
                    timestamp: now_iso8601(),
                    event_type: "review_submitted".to_string(),
                    payload: serde_json::json!({ "action": result.action }),
                };
                if let Err(e) = self
                    .services
                    .backend
                    .append_activity(&result.wi_id, &log_entry)
                {
                    self.shell.status_message = Some(format!("Activity log error: {e}"));
                }

                // Suppress re-open for this item until fresh repo_data
                // arrives. Without this, stale review-requested data in
                // repo_data would immediately bounce the item back to Review.
                self.review_reopen_suppress.insert(result.wi_id.clone());

                self.apply_stage_change(
                    &result.wi_id,
                    WorkItemStatus::Review,
                    WorkItemStatus::Done,
                    "review_submitted",
                );
                // Only show success if the item actually reached Done.
                // apply_stage_change may have set an error message on failure
                // (e.g. backend write error) - don't overwrite it.
                let reached_done = self
                    .work_items
                    .iter()
                    .find(|w| w.id == result.wi_id)
                    .is_some_and(|w| w.status == WorkItemStatus::Done);
                if reached_done {
                    self.shell.status_message = Some(format!("Review {verb} and moved to [DN]"));
                }
            }
            ReviewSubmitOutcome::Failed { ref error } => {
                self.shell.status_message = Some(error.clone());
            }
        }
    }
}
