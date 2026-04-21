//! PR creation + merge-execution subsystem (Phase 1).
//!
//! Drains the background PR-creation channel
//! (`poll_pr_creation`), admits a merge action
//! (`execute_merge`) through the `UserActionKey::PrMerge` guard,
//! and exposes the small `is_merge_precheck_phase` /
//! `merge_confirm_hint` predicates used by the merge modal
//! renderer. The actual merge polling lives in
//! `pr_merge_and_review`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::work_item::{WorkItemId, WorkItemStatus};
use crate::work_item_backend::ActivityEntry;

impl super::App {
    /// Best-effort async PR creation when entering Review.
    ///
    /// Gathers the needed data, then spawns a background thread to run
    /// the `gh` CLI commands. Results are polled by `poll_pr_creation()`
    /// on each timer tick.
    pub(super) fn spawn_pr_creation(&mut self, wi_id: &WorkItemId) {
        // If a PR creation is already in-flight, queue this one instead of
        // silently dropping it. The queue is drained in poll_pr_creation.
        if self.is_user_action_in_flight(&UserActionKey::PrCreate) {
            if !self.pr_create_pending.contains(wi_id) {
                self.pr_create_pending.push_back(wi_id.clone());
            }
            return;
        }

        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            return;
        };
        let branch = match assoc.branch.as_ref() {
            Some(b) => b.clone(),
            None => return,
        };
        let repo_path = assoc.repo_path.clone();
        let title = wi.title.clone();
        let wi_id = wi_id.clone();

        // Read owner/repo from the cached fetcher result rather than shelling
        // out via `worktree_service.github_remote(...)` on the UI thread. The
        // fetcher populates `repo_data[path].github_remote` on every cycle;
        // if no entry exists yet the first fetch hasn't completed and we
        // surface that to the user instead of blocking.
        //
        // This check runs BEFORE try_begin_user_action so an early return
        // (cache miss) cannot leave an orphaned helper entry - see the
        // "desync guard" discussion in `docs/UI.md` "User action guard".
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.status_message = Some(
                "PR creation skipped: GitHub remote not yet cached (waiting for next fetch)".into(),
            );
            return;
        };
        let owner_repo = format!("{owner}/{repo_name}");

        // Admit the action through the user-action guard. The early-return
        // cache check above runs first so we never hold a helper slot
        // across a rejected code path.
        if self
            .try_begin_user_action(
                UserActionKey::PrCreate,
                Duration::ZERO,
                "Creating pull request...",
            )
            .is_none()
        {
            // Unreachable today because the is_user_action_in_flight
            // check above already short-circuited the queueing path,
            // but defense in depth: if a race ever sneaks through, push
            // the request onto the pending queue rather than silently
            // dropping it.
            if !self.pr_create_pending.contains(&wi_id) {
                self.pr_create_pending.push_back(wi_id);
            }
            return;
        }

        // Clone the Arc'd backend and worktree service so the background
        // thread can run `read_plan` (filesystem) and `default_branch`
        // (git subprocess) off the UI thread.
        let backend = Arc::clone(&self.services.backend);
        let ws = Arc::clone(&self.services.worktree_service);

        let (tx, rx) = crossbeam_channel::bounded(1);
        let helper_wi_id = wi_id.clone();

        std::thread::spawn(move || {
            // Blocking reads run on the background thread. `read_plan` hits
            // the filesystem; `default_branch` shells out to git. Both are
            // cheap per-call but absolutely prohibited on the UI thread.
            let body = match backend.read_plan(&wi_id) {
                Ok(Some(plan)) if !plan.trim().is_empty() => plan,
                _ => String::new(),
            };
            let default_branch = ws
                .default_branch(&repo_path)
                .unwrap_or_else(|_| "main".to_string());

            // Check if a PR already exists for this branch.
            let check_output = std::process::Command::new("gh")
                .args([
                    "pr",
                    "list",
                    "--head",
                    &branch,
                    "--json",
                    "number",
                    "--repo",
                    &owner_repo,
                ])
                .output();

            match check_output {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Ok(arr) = serde_json::from_str::<serde_json::Value>(stdout.trim())
                        && arr.as_array().is_some_and(|a| !a.is_empty())
                    {
                        // PR already exists - nothing to do.
                        let _ = tx.send(PrCreateResult {
                            wi_id,
                            info: None,
                            error: None,
                            url: None,
                        });
                        return;
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("PR check failed (continuing): {stderr}")),
                        url: None,
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("PR check failed (continuing): {e}")),
                        url: None,
                    });
                    return;
                }
            }

            // Ensure the branch is pushed to the remote before creating the PR.
            let push_output = crate::worktree_service::git_command()
                .args(["push", "-u", "origin", &branch])
                .current_dir(&repo_path)
                .output();
            match push_output {
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("git push failed: {stderr}")),
                        url: None,
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("git push failed: {e}")),
                        url: None,
                    });
                    return;
                }
                _ => {} // push succeeded
            }

            // Create the PR.
            let create_result = std::process::Command::new("gh")
                .args([
                    "pr",
                    "create",
                    "--title",
                    &title,
                    "--body",
                    &body,
                    "--head",
                    &branch,
                    "--base",
                    &default_branch,
                    "--repo",
                    &owner_repo,
                ])
                .output();

            let result = match create_result {
                Ok(output) if output.status.success() => {
                    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    let info = format!("PR created: {url}");
                    PrCreateResult {
                        wi_id,
                        info: Some(info),
                        error: None,
                        url: Some(url),
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    PrCreateResult {
                        wi_id,
                        info: None,
                        error: Some(format!("PR creation failed (continuing): {stderr}")),
                        url: None,
                    }
                }
                Err(e) => PrCreateResult {
                    wi_id,
                    info: None,
                    error: Some(format!("PR creation failed (continuing): {e}")),
                    url: None,
                },
            };
            let _ = tx.send(result);
        });

        self.attach_user_action_payload(
            &UserActionKey::PrCreate,
            UserActionPayload::PrCreate {
                rx,
                wi_id: helper_wi_id,
            },
        );
    }

    /// Poll the async PR creation thread for a result. Called on each timer tick.
    pub fn poll_pr_creation(&mut self) {
        let recv_result = {
            let Some(UserActionPayload::PrCreate { rx, .. }) =
                self.user_action_payload(&UserActionKey::PrCreate)
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
            self.end_user_action(&UserActionKey::PrCreate);
            self.status_message = Some("PR creation: background thread exited unexpectedly".into());
            // Try next pending PR creation despite the failure.
            // Skip items that were deleted or retreated from Review.
            while let Some(next_id) = self.pr_create_pending.pop_front() {
                if self
                    .work_items
                    .iter()
                    .any(|w| w.id == next_id && w.status == WorkItemStatus::Review)
                {
                    self.spawn_pr_creation(&next_id);
                    break;
                }
            }
            return;
        };

        self.end_user_action(&UserActionKey::PrCreate);

        // Log PR creation to activity log.
        if let Some(ref url) = result.url {
            let log_entry = ActivityEntry {
                timestamp: now_iso8601(),
                event_type: "pr_created".to_string(),
                payload: serde_json::json!({ "url": url }),
            };
            if let Err(e) = self
                .services
                .backend
                .append_activity(&result.wi_id, &log_entry)
            {
                self.status_message = Some(format!("Activity log error: {e}"));
            }
        }

        // Update status message.
        if let Some(info) = result.info {
            self.status_message = Some(info);
        } else if let Some(error) = result.error {
            self.status_message = Some(error);
        }

        // Drain the pending queue: spawn the next queued PR creation if any.
        // Skip items that were deleted or retreated from Review while queued.
        while let Some(next_id) = self.pr_create_pending.pop_front() {
            if self
                .work_items
                .iter()
                .any(|w| w.id == next_id && w.status == WorkItemStatus::Review)
            {
                self.spawn_pr_creation(&next_id);
                break;
            }
        }
    }

    /// Spawn an async PR merge for the given work item.
    ///
    /// `strategy` is either "squash" or "merge". If any prerequisite is
    /// missing (no repo association, no branch, no GitHub remote), the
    /// request is blocked with an error message and the item stays in
    /// Review. The background thread also checks for an open PR and
    /// blocks if none exists (see `poll_pr_merge` / `NoPr` outcome).
    ///
    /// Two-phase flow:
    ///
    /// 1. Pre-flight validity checks (in-memory only): `wi`, branch,
    ///    `repo_path`, GitHub remote cache. Failures alert and return
    ///    BEFORE admitting the helper slot so an early return cannot
    ///    leave an orphaned `UserActionKey::PrMerge` entry. See
    ///    `docs/UI.md` "User action guard" for the desync-guard rule.
    /// 2. Admit the helper slot, hide the status-bar spinner, set the
    ///    in-progress modal flag, and spawn a background working-tree
    ///    precheck via `spawn_merge_precheck`. The
    ///    `poll_merge_precheck` background-tick poller drains the
    ///    receiver and either hands off to
    ///    `perform_merge_after_precheck` (Ready) or surfaces the live
    ///    blocker as an alert (Blocked).
    ///
    /// The cleanliness check used to live here as a synchronous cache
    /// read against `repo_data`. That cached path stayed stale across
    /// long-running sessions: a user who fixed a dirty worktree
    /// minutes ago could still see the "Uncommitted changes" alert
    /// when trying to merge. The precheck phase replaces that read
    /// with a live `WorktreeService::list_worktrees` call (plus a
    /// live `GithubClient::fetch_live_merge_state` call for the
    /// remote PR state) on a background thread, so the merge guard
    /// always reflects the current state.
    pub fn execute_merge(&mut self, wi_id: &WorkItemId, strategy: &str) {
        // Single-flight guard via the user-action helper. Rejecting when
        // another merge is already in flight preserves the existing alert
        // wording verbatim - the background thread may have already
        // merged a PR on GitHub, so silently replacing the receiver
        // would lose the result.
        if self.is_user_action_in_flight(&UserActionKey::PrMerge) {
            self.alert_message = Some(PR_MERGE_ALREADY_IN_PROGRESS.into());
            return;
        }

        let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
            return;
        };
        let Some(assoc) = wi.repo_associations.first() else {
            self.confirm_merge = false;
            self.merge_wi_id = None;
            self.alert_message = Some("Cannot merge: no repo association".into());
            return;
        };
        let branch = if let Some(b) = assoc.branch.as_ref() {
            b.clone()
        } else {
            self.confirm_merge = false;
            self.merge_wi_id = None;
            self.alert_message = Some("Cannot merge: no branch associated".into());
            return;
        };
        let repo_path = assoc.repo_path.clone();

        // Read owner/repo from the cached fetcher result rather than shelling
        // out on the UI thread. If no entry exists yet, the first fetch has
        // not completed - surface that as an alert instead of blocking.
        let Some((owner, repo_name)) = self
            .repo_data
            .get(&repo_path)
            .and_then(|rd| rd.github_remote.clone())
        else {
            self.confirm_merge = false;
            self.merge_wi_id = None;
            self.alert_message =
                Some("Cannot merge: GitHub remote not yet cached (waiting for next fetch)".into());
            return;
        };
        let owner_repo = format!("{owner}/{repo_name}");

        // All in-memory validity checks have passed. Admit the action
        // now so any rejection above cannot leave the helper with an
        // empty slot. The slot is reserved across BOTH the precheck
        // phase and the actual merge phase - `poll_merge_precheck`
        // either hands off to `perform_merge_after_precheck` (which
        // attaches the merge payload without re-admitting) or releases
        // the slot via `end_user_action`.
        if self
            .try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
            .is_none()
        {
            // Raced with another in-flight merge after the
            // `is_user_action_in_flight` check above. Mirror that
            // check's alert wording so the user sees a single, stable
            // rejection message regardless of which branch rejected.
            self.alert_message = Some(PR_MERGE_ALREADY_IN_PROGRESS.into());
            return;
        }
        // Hide the status-bar spinner: the merge modal already renders
        // its own in-progress spinner (and now also the
        // "Refreshing remote state..." precheck spinner), and stacking
        // two is confusing. The helper map entry is still the single
        // source of truth for `is_user_action_in_flight(&PrMerge)`.
        if let Some(state) = self.user_actions.in_flight.get(&UserActionKey::PrMerge) {
            let aid = state.activity_id;
            self.activities.end(aid);
        }

        // Modal renders the spinner from the moment the user pressed
        // "merge" - the precheck phase shows "Refreshing remote
        // state..." and the merge phase shows "Merging pull
        // request...". The renderer in `src/ui.rs` keys off
        // `App::is_merge_precheck_phase()` to pick the right body,
        // which checks the helper slot's `UserActionPayload` variant.
        self.merge_in_progress = true;

        self.spawn_merge_precheck(
            wi_id.clone(),
            strategy.to_string(),
            repo_path,
            branch,
            owner_repo,
        );
    }

    /// Spawn the live merge precheck for an in-flight merge.
    ///
    /// Runs two live fetches on a background thread:
    /// 1. `WorktreeService::list_worktrees` for the local worktree
    ///    state (dirty / untracked / unpushed).
    /// 2. `GithubClient::fetch_live_merge_state` for the remote PR
    ///    state (mergeable flag + CI rollup).
    ///
    /// The results are handed to `MergeReadiness::classify` which
    /// encodes the canonical priority order
    /// `Dirty > Untracked > Unpushed > PrConflict > CiFailing >
    /// BehindOnly > Clean`, and
    /// `MergeReadiness::merge_block_message` translates the
    /// classification to the user-facing alert string. A `None`
    /// message means the precheck clears the merge; any `Some` is
    /// reported via `MergePreCheckMessage::Blocked`.
    ///
    /// The receiver is stored structurally inside the helper slot's
    /// `UserActionPayload::PrMergePrecheck` variant via
    /// `attach_user_action_payload`, so any cancel path that calls
    /// `end_user_action(&UserActionKey::PrMerge)` automatically
    /// drops it. `poll_merge_precheck` drains the receiver on the
    /// next ~200ms background tick.
    ///
    /// No-worktree fallthrough: if `list_worktrees` returns no entry
    /// matching `branch`, the precheck passes `None` to
    /// `MergeReadiness::classify` and the local checks short-circuit
    /// to "nothing to protect". PR-only / reassembled work items -
    /// and items whose local worktree was removed after the branch
    /// was pushed - have no checked-out tree to protect, so there is
    /// nothing for the dirty / untracked / unpushed guards to flag.
    /// Refusing to merge in that case would make perfectly safe PRs
    /// unmergeable from the UI. The cached guard this replaced
    /// treated a missing cache entry as `Clean` for the same reason.
    ///
    /// No-PR fallthrough: if `fetch_live_merge_state` reports
    /// `has_open_pr: false`, the remote checks short-circuit to "no
    /// remote constraints" and the classifier falls back to the
    /// local state. The downstream merge thread then surfaces the
    /// existing `NoPr` outcome.
    ///
    /// Blocking-I/O note: every call inside the spawned closure
    /// (`list_worktrees`, `fetch_live_merge_state`) is allowed to
    /// block - that is the entire reason they live off the main
    /// thread. The UI thread sees only the receiver and the
    /// `MergePreCheckMessage`. See `docs/UI.md` "Blocking I/O
    /// Prohibition".
    pub(super) fn spawn_merge_precheck(
        &mut self,
        wi_id: WorkItemId,
        strategy: String,
        repo_path: PathBuf,
        branch: String,
        owner_repo: String,
    ) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ws = Arc::clone(&self.services.worktree_service);
        let github = Arc::clone(&self.services.github_client);
        let wi_id_for_thread = wi_id;
        let strategy_for_thread = strategy;
        let repo_path_for_thread = repo_path;
        let branch_for_thread = branch;
        let owner_repo_for_thread = owner_repo;

        std::thread::spawn(move || {
            // 1. Live worktree state. Reusing list_worktrees keeps
            //    the test harness identical to the fetcher path -
            //    any mock that returns a clean `WorktreeInfo` for
            //    the fetcher will return clean here too.
            let worktrees = match ws.list_worktrees(&repo_path_for_thread) {
                Ok(list) => list,
                Err(e) => {
                    let _ = tx.send(MergePreCheckMessage::Blocked {
                        reason: format!("Cannot merge: working-tree check failed: {e}"),
                    });
                    return;
                }
            };
            let wt = worktrees
                .into_iter()
                .find(|w| w.branch.as_deref() == Some(&branch_for_thread));

            // 2. Live remote PR state. Split `owner_repo` at the
            //    first `/` - the caller guarantees this shape in
            //    `execute_merge`, which derives it from
            //    `repo_data[path].github_remote`. If the split fails
            //    (malformed remote URL), block with a diagnostic
            //    alert - the P0 "surface errors, don't auto-fix"
            //    posture.
            let (owner, repo) = match owner_repo_for_thread.split_once('/') {
                Some((o, r)) if !o.is_empty() && !r.is_empty() => (o.to_string(), r.to_string()),
                _ => {
                    let _ = tx.send(MergePreCheckMessage::Blocked {
                        reason: format!(
                            "Cannot merge: malformed owner/repo identifier: {owner_repo_for_thread}"
                        ),
                    });
                    return;
                }
            };
            let live_pr = match github.fetch_live_merge_state(&owner, &repo, &branch_for_thread) {
                Ok(state) => state,
                Err(e) => {
                    let _ = tx.send(MergePreCheckMessage::Blocked {
                        reason: format!("Cannot merge: remote merge-state check failed: {e}"),
                    });
                    return;
                }
            };

            // 3. Classify the combined state and translate to the
            //    precheck message. `classify` owns the priority
            //    order; `merge_block_message` owns the user-facing
            //    wording.
            let readiness = MergeReadiness::classify(wt.as_ref(), &live_pr);
            let msg = readiness.merge_block_message().map_or_else(
                || MergePreCheckMessage::Ready {
                    wi_id: wi_id_for_thread,
                    strategy: strategy_for_thread,
                    branch: branch_for_thread,
                    repo_path: repo_path_for_thread,
                    owner_repo: owner_repo_for_thread,
                },
                |reason| MergePreCheckMessage::Blocked {
                    reason: reason.to_string(),
                },
            );
            let _ = tx.send(msg);
        });

        // Move the receiver into the helper slot's payload so it is
        // owned structurally. This MUST come after `try_begin_user_action`
        // (called by `execute_merge` upstream) reserved the slot with
        // `UserActionPayload::Empty` - we are replacing that empty
        // payload with `PrMergePrecheck`. End-of-life is automatic:
        // every `end_user_action(&UserActionKey::PrMerge)` drops the
        // slot and the receiver in the same step.
        self.attach_user_action_payload(
            &UserActionKey::PrMerge,
            UserActionPayload::PrMergePrecheck { rx },
        );
    }

    /// Returns true when the `UserActionKey::PrMerge` slot is in the
    /// precheck phase - i.e. its payload is
    /// `UserActionPayload::PrMergePrecheck`. Used by the merge
    /// confirm modal renderer in `src/ui.rs` to switch between the
    /// "Refreshing remote state..." and "Merging pull request..."
    /// body strings without touching internal helper-map fields.
    /// Pure in-memory check, safe on the UI thread.
    pub fn is_merge_precheck_phase(&self) -> bool {
        matches!(
            self.user_action_payload(&UserActionKey::PrMerge),
            Some(UserActionPayload::PrMergePrecheck { .. })
        )
    }

    /// Optional hint line appended to the merge-confirm modal body
    /// whenever the cached repo state already shows a signal that
    /// the live precheck is likely to block on.
    ///
    /// This is a soft, advisory hint - it never refuses to open the
    /// modal and never short-circuits the precheck. The whole point
    /// of the precheck is that cache can be stale, so the cached
    /// state is consulted ONLY for a textual reassurance, never for
    /// a go / no-go decision. If the cache is stale the worst case
    /// is a spurious hint; the precheck still runs and is
    /// authoritative.
    ///
    /// Returned variants:
    /// - `Some("Live re-check will run before merging.")` when any
    ///   of `git_state.dirty`, `git_state.ahead > 0`,
    ///   `PrInfo.mergeable == Conflicting`, or `PrInfo.checks ==
    ///   Failing` is observed on any repo association.
    /// - `Some("CI still running; merge will queue on branch
    ///   protection.")` when the ONLY concerning signal is
    ///   `PrInfo.checks == Pending` (no other hard-block hint
    ///   fires). Pending CI does not block the merge but the user
    ///   may still want to know that branch protection will queue
    ///   the merge until checks land.
    /// - `None` in all other cases.
    ///
    /// Pure in-memory read, safe on the UI thread.
    pub fn merge_confirm_hint(&self, wi_id: &WorkItemId) -> Option<&'static str> {
        let wi = self.work_items.iter().find(|w| &w.id == wi_id)?;

        let mut hard_block = false;
        let mut pending_only = false;
        for assoc in &wi.repo_associations {
            if let Some(gs) = assoc.git_state.as_ref()
                && (gs.dirty || gs.ahead > 0)
            {
                hard_block = true;
            }
            if let Some(pr) = assoc.pr.as_ref() {
                if matches!(pr.mergeable, crate::work_item::MergeableState::Conflicting) {
                    hard_block = true;
                }
                match pr.checks {
                    crate::work_item::CheckStatus::Failing => hard_block = true,
                    crate::work_item::CheckStatus::Pending => pending_only = true,
                    _ => {}
                }
            }
        }

        if hard_block {
            Some("Live re-check will run before merging.")
        } else if pending_only {
            Some("CI still running; merge will queue on branch protection.")
        } else {
            None
        }
    }
}
