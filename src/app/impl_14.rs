//! Subset of `impl App` methods extracted from `src/app/mod.rs`.
//!
//! The `impl App { ... }` is split across sibling files solely to
//! keep every file within the 700-line ceiling. Methods behave
//! identically to the original single-file layout.

use std::path::PathBuf;

use super::*;
use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::ActivityEntry;

impl super::App {
    /// Enter the Mergequeue state for a work item. The item must be in
    /// Review with an open PR. Registers a watch so `poll_mergequeue()`
    /// will check the PR state periodically.
    pub fn enter_mergequeue(&mut self, wi_id: &WorkItemId) {
        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            self.status_message = Some("Cannot enter mergequeue: no repo association".into());
            return;
        };
        let branch = if let Some(b) = assoc.branch.as_ref() {
            b.clone()
        } else {
            self.status_message = Some("Cannot enter mergequeue: no branch".into());
            return;
        };
        let pr_number = if let Some(pr) = assoc.pr.as_ref() {
            pr.number
        } else {
            self.status_message = Some("Cannot enter mergequeue: no PR found".into());
            return;
        };
        let repo_path = assoc.repo_path.clone();
        // Read owner/repo from the cached fetcher result - never shell out
        // on the UI thread.
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.status_message = Some(
                "Cannot enter mergequeue: GitHub remote not yet cached \
                 (waiting for next fetch)"
                    .into(),
            );
            return;
        };
        let owner_repo = format!("{owner}/{repo_name}");

        self.mergequeue_watches.push(PrMergeWatch {
            wi_id: wi_id.clone(),
            // Pin the exact PR number from assoc.pr so subsequent polls
            // target it unambiguously even if the branch later has a
            // different PR opened on it.
            pr_number: Some(pr_number),
            owner_repo,
            branch,
            repo_path,
            last_polled: None,
        });

        self.apply_stage_change(
            wi_id,
            WorkItemStatus::Review,
            WorkItemStatus::Mergequeue,
            "user",
        );
        self.status_message = Some("Entered mergequeue - polling PR until merged".into());
    }

    impl_pr_merge_poll_method!(
        /// Poll the PR state for items in the Mergequeue. Called on
        /// each timer tick. Spawns at most one background thread per
        /// watched item at a time, with a 30-second cooldown between
        /// polls.
        ///
        /// Generated from `impl_pr_merge_poll_method!` so it shares
        /// its Phase 1 drain, still-eligible guard, `pr_number`
        /// backfill, merge-gate dispatch, and Phase 2 cooldown /
        /// subprocess spawn logic with `poll_review_request_merges`.
        /// The per-stage deltas are passed as macro arguments.
        fn poll_mergequeue,
        watches = mergequeue_watches,
        polls = mergequeue_polls,
        errors = mergequeue_poll_errors,
        source_stage = WorkItemStatus::Mergequeue,
        kind_filter = None,
        strategy_tag = "external",
        merged_message = "PR merged externally - moved to [DN]",
        closed_message = "PR was closed without merging - retreat to Review or re-open the PR",
        poll_error_prefix = "Mergequeue poll error",
        poll_label_prefix = "Polling PR for merge",
        // Mergequeue path owns a worktree the user is actively
        // working in; clean it up on a successful merge so it does
        // not linger.
        cleanup_worktree_on_merge = true,
    );

    impl_pr_merge_poll_method!(
        /// Poll the PR state for `ReviewRequest` work items in Review
        /// so that an external merge of the PR can auto-transition
        /// the item to Done.
        ///
        /// This is the only code path that can observe a merged
        /// review-request PR: the author-filtered `gh pr list
        /// --author @me` fetch cannot see it (wrong author), the
        /// `review-requested:@me` search is `--state open` (wrong
        /// state), the `pr_identity` fallback is `Done`-only (wrong
        /// status), and the startup merged-PR backfill is also
        /// author-filtered. Without this poll the item stays stuck
        /// in `[RR][RV]` forever.
        ///
        /// Generated from `impl_pr_merge_poll_method!` so it shares
        /// its Phase 1 drain, still-eligible guard, `pr_number`
        /// backfill, merge-gate dispatch, and Phase 2 cooldown /
        /// subprocess spawn logic with `poll_mergequeue`. The
        /// per-stage deltas are passed as macro arguments. The
        /// distinct `strategy` tag (`external_review_merge`) lets
        /// metrics tell reviewer-side merges apart from author-side
        /// Mergequeue merges.
        ///
        /// This path must NOT call `cleanup_worktree_for_item`:
        /// `worktree_service.remove_worktree` is a blocking I/O
        /// operation and would freeze the UI from the timer tick
        /// (see `docs/UI.md` "Blocking I/O Prohibition"). The
        /// worktree is left on disk and cleaned up later by
        /// auto-archive (default 7 days) or immediately by the user
        /// with Ctrl+D.
        fn poll_review_request_merges,
        watches = review_request_merge_watches,
        polls = review_request_merge_polls,
        errors = review_request_merge_poll_errors,
        source_stage = WorkItemStatus::Review,
        kind_filter = Some(WorkItemKind::ReviewRequest),
        strategy_tag = "external_review_merge",
        merged_message = "Review request PR merged externally - moved to [DN]",
        closed_message = "Review request PR was closed without merging - Ctrl+D to clean up",
        poll_error_prefix = "Review request poll error",
        poll_label_prefix = "Polling review-request PR for merge",
        cleanup_worktree_on_merge = false,
    );

    impl_pr_merge_reconstruct_method!(
        /// Reconstruct mergequeue watches from the current
        /// `work_items` snapshot. Called after every reassembly so
        /// that any item in `Mergequeue` gets exactly one watch and
        /// app restarts never lose state.
        ///
        /// This is an add-only pass: stale watches / in-flight polls
        /// / errors for items that are no longer in `Mergequeue` are
        /// cleaned up lazily by `poll_mergequeue`'s Phase 1 drain
        /// (its still-eligible guard) on the next poll cycle. See
        /// `impl_pr_merge_reconstruct_method!` for the reasoning.
        ///
        /// Generated from `impl_pr_merge_reconstruct_method!` so it
        /// shares its idempotent-add logic with
        /// `reconstruct_review_request_merge_watches`.
        fn reconstruct_mergequeue_watches,
        watches = mergequeue_watches,
        source_stage = WorkItemStatus::Mergequeue,
        kind_filter = None,
    );

    impl_pr_merge_reconstruct_method!(
        /// Reconstruct `ReviewRequest` merge watches from the current
        /// `work_items` snapshot. Called after every reassembly so
        /// that any `ReviewRequest`-kind item in `Review` gets
        /// exactly one watch and app restarts never lose state.
        ///
        /// This is an add-only pass: stale watches / in-flight polls
        /// / errors for items that are no longer eligible are cleaned
        /// up lazily by `poll_review_request_merges`'s Phase 1 drain
        /// (its still-eligible guard) on the next poll cycle.
        ///
        /// `ReviewRequest` items almost never carry a live `assoc.pr`
        /// because the `--author @me` fetch filters their
        /// author-side PRs out, so reconstructions here will
        /// generally start with `pr_number = None` and rely on the
        /// first poll's branch-based fallback to resolve the number.
        ///
        /// Generated from `impl_pr_merge_reconstruct_method!` so it
        /// shares its idempotent-add logic with
        /// `reconstruct_mergequeue_watches`.
        fn reconstruct_review_request_merge_watches,
        watches = review_request_merge_watches,
        source_stage = WorkItemStatus::Review,
        kind_filter = Some(WorkItemKind::ReviewRequest),
    );

    /// Collect Done items that need PR identity backfill (have a branch but
    /// no persisted `pr_identity`). Returns tuples of
    /// (`wi_id`, `repo_path`, branch, `github_owner`, `github_repo`).
    ///
    /// Reads owner/repo from the cached `repo_data[path].github_remote`
    /// populated by the background fetcher. When the fetcher has not
    /// produced a result for a repo yet, that repo is silently skipped
    /// (the next reassembly will retry) - we NEVER shell out via
    /// `worktree_service.github_remote(...)` on the UI thread.
    ///
    /// Temporary migration helper - can be removed once all existing Done
    /// items have been backfilled (i.e. no Done items with `pr_identity=None`
    /// remain on disk).
    pub fn collect_backfill_requests(&self) -> Vec<(WorkItemId, PathBuf, String, String, String)> {
        let records = match self.backend.list() {
            Ok(lr) => lr.records,
            Err(_) => return vec![],
        };
        let mut requests = Vec::new();
        for record in &records {
            if record.status != WorkItemStatus::Done {
                continue;
            }
            for assoc in &record.repo_associations {
                if assoc.pr_identity.is_some() {
                    continue;
                }
                let branch = match &assoc.branch {
                    Some(b) => b.clone(),
                    None => continue,
                };
                let Some((owner, repo_name)) = self
                    .repo_data
                    .get(&assoc.repo_path)
                    .and_then(|rd| rd.github_remote.clone())
                else {
                    continue;
                };
                requests.push((
                    record.id.clone(),
                    assoc.repo_path.clone(),
                    branch,
                    owner,
                    repo_name,
                ));
            }
        }
        requests
    }

    /// Drain results from the PR identity backfill channel. Returns true
    /// if any identities were saved (caller should reassemble).
    ///
    /// Temporary migration helper - can be removed once all existing Done
    /// items have been backfilled.
    pub fn drain_pr_identity_backfill(&mut self) -> bool {
        let Some(rx) = self.pr_identity_backfill_rx.as_ref() else {
            return false;
        };
        let mut changed = false;
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok(msg) => match msg {
                    Ok(result) => {
                        if let Err(e) = self.backend.save_pr_identity(
                            &result.wi_id,
                            &result.repo_path,
                            &result.identity,
                        ) {
                            self.status_message = Some(format!("PR identity backfill error: {e}"));
                        } else {
                            changed = true;
                        }
                    }
                    Err(e) => {
                        self.status_message = Some(format!("PR identity backfill: {e}"));
                    }
                },
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.pr_identity_backfill_rx = None;
            if let Some(aid) = self.pr_identity_backfill_activity.take() {
                self.activities.end(aid);
            }
        }
        changed
    }

    /// Remove the worktree directory and local branch for a work item after merge.
    /// Uses `delete_branch=true` so the merged branch is cleaned up. Uses force=false
    /// because post-merge worktrees should be clean and `-d` is safe for merged branches.
    pub(super) fn cleanup_worktree_for_item(&mut self, wi_id: &WorkItemId) {
        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        for assoc in &wi.repo_associations {
            if let Some(ref wt_path) = assoc.worktree_path {
                if let Err(e) =
                    self.worktree_service
                        .remove_worktree(&assoc.repo_path, wt_path, true, false)
                {
                    self.status_message = Some(format!("Worktree cleanup warning: {e}"));
                }
            } else if let Some(ref branch) = assoc.branch {
                // No worktree but a branch exists - still clean up the branch.
                if let Err(e) = self
                    .worktree_service
                    .delete_branch(&assoc.repo_path, branch, false)
                {
                    self.status_message = Some(format!("Branch cleanup warning: {e}"));
                }
            }
        }
    }

    /// Find a PR number for a branch by querying `gh pr list --head <branch>`.
    /// Returns None if no PR exists. Runs on background thread (blocking I/O).
    pub(super) fn find_pr_for_branch(owner: &str, repo: &str, branch: &str) -> Option<u64> {
        let owner_repo = format!("{owner}/{repo}");
        let output = std::process::Command::new("gh")
            .args([
                "pr",
                "list",
                "--head",
                branch,
                "--json",
                "number",
                "--repo",
                &owner_repo,
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let arr: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
        arr.as_array()?.first()?.get("number")?.as_u64()
    }

    /// Fetch per-check CI status for a PR via `gh pr checks`.
    /// Returns empty vec on error (treated as "no checks configured").
    /// Runs on background thread (blocking I/O).
    pub(super) fn fetch_pr_checks(owner: &str, repo: &str, pr_number: u64) -> Vec<CiCheck> {
        let owner_repo = format!("{owner}/{repo}");
        let pr_str = pr_number.to_string();
        let output = match std::process::Command::new("gh")
            .args([
                "pr",
                "checks",
                &pr_str,
                "--repo",
                &owner_repo,
                "--json",
                "name,bucket",
            ])
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return Vec::new(),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let arr: Vec<serde_json::Value> = match serde_json::from_str(stdout.trim()) {
            Ok(a) => a,
            Err(_) => return Vec::new(),
        };
        // Expected bucket values from `gh pr checks`: "pass", "fail", "pending",
        // "skipping", "cancel". Unknown values are included in the result but
        // won't match any pass/fail filter, effectively treated as pending.
        arr.iter()
            .filter_map(|v| {
                Some(CiCheck {
                    name: v.get("name")?.as_str()?.to_string(),
                    bucket: v.get("bucket")?.as_str()?.to_string(),
                })
            })
            .collect()
    }
}
