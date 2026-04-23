//! App aggregator module. Types live in `types_01` / `types_02`, the
//! `App` struct in `struct_app`, helpers in `helpers`, test stubs in
//! `stubs` (cfg test), per-subsystem `impl App` blocks in
//! subsystem-named sibling modules (for example `review_gate`,
//! `rebase_gate_spawn`, `session_spawn`, `mergequeue`), and the
//! `spawn_rebase_gate` background compute phase in
//! `rebase_gate_compute`. The full list of sibling modules is the
//! `mod ...;` declarations below. Submodule re-exports keep existing
//! `super::App` / `super::RebaseResult` / etc. import paths working
//! without changes in sibling submodules.

/// Generate a `poll_*` method that drives one PR-merge poller instance.
///
/// `poll_mergequeue` and `poll_review_request_merges` differ only in
/// which `App` fields they touch (watches / in-flight polls / errors),
/// which stage they treat as "eligible", and a few static data bits
/// (strategy tag, status messages, whether the merged branch schedules
/// `spawn_post_merge_worktree_cleanup`). Everything else - the Phase 1 drain
/// loop, the still-eligible guard, the `pr_number` backfill, the merge-
/// gate dispatch, the Phase 2 cooldown, the subprocess spawn - is
/// identical. Expressing both via a macro keeps the two methods on one
/// source of truth so they cannot drift as the `gh` path, the merge-
/// gate, or the JSON schema evolve.
///
/// See `spawn_gh_pr_view_poll` for the subprocess body itself.
macro_rules! impl_pr_merge_poll_method {
    (
        $(#[$meta:meta])*
        fn $method_name:ident,
        watches = $watches_field:ident,
        polls = $polls_field:ident,
        errors = $errors_field:ident,
        source_stage = $source_stage:expr,
        kind_filter = $kind_filter:expr,
        strategy_tag = $strategy_tag:expr,
        merged_message = $merged_message:expr,
        closed_message = $closed_message:expr,
        poll_error_prefix = $poll_error_prefix:expr,
        poll_label_prefix = $poll_label_prefix:expr,
        cleanup_worktree_on_merge = $cleanup_worktree:expr,
    ) => {
        $(#[$meta])*
        pub fn $method_name(&mut self) {
            // -- Phase 1: drain any in-flight results --
            let ready = drain_pr_merge_poll_results(
                &mut self.$polls_field,
                &mut self.activities,
            );

            let kind_filter: Option<WorkItemKind> = $kind_filter;
            for result in ready {
                let handled = pr_merge_poll_still_eligible(
                    &self.work_items,
                    &result,
                    $source_stage,
                    kind_filter.as_ref(),
                );
                if !handled {
                    self.$watches_field.retain(|w| w.wi_id != result.wi_id);
                    self.$errors_field.remove(&result.wi_id);
                    continue;
                }

                // Backfill pr_number on the watch the first time a
                // branch-based poll resolves to a concrete PR. This
                // pins subsequent polls to the exact PR so a
                // closed-then-reopened-on-same-branch race cannot
                // redirect the watch to a different PR.
                if let Some(identity) = &result.pr_identity
                    && let Some(watch) = self
                        .$watches_field
                        .iter_mut()
                        .find(|w| w.wi_id == result.wi_id)
                    && watch.pr_number.is_none()
                {
                    watch.pr_number = Some(identity.number);
                }

                match result.pr_state.as_str() {
                    "MERGED" => {
                        self.finalize_pr_merge_poll_merged(
                            &result,
                            $strategy_tag,
                            $source_stage,
                            $merged_message,
                            $cleanup_worktree,
                        );
                        self.$watches_field.retain(|w| w.wi_id != result.wi_id);
                        self.$errors_field.remove(&result.wi_id);
                    }
                    "CLOSED" => {
                        // A closed PR is NOT a merge - it must not
                        // bypass the merge-gate invariant. Leave the
                        // watch in place so we keep observing (in case
                        // somebody reopens the same PR) and surface a
                        // distinct warning.
                        self.$errors_field.remove(&result.wi_id);
                        self.shell.status_message = Some($closed_message.into());
                    }
                    s if s.starts_with("ERROR:") => {
                        let msg = format!(
                            "{} for {}: {}",
                            $poll_error_prefix, result.branch, result.pr_state
                        );
                        self.$errors_field
                            .insert(result.wi_id.clone(), msg.clone());
                        self.shell.status_message = Some(msg);
                        // Item stays in its source stage - will retry
                        // on next poll cycle.
                    }
                    _ => {
                        // Still open - no action, will poll again next
                        // cycle.
                        self.$errors_field.remove(&result.wi_id);
                    }
                }
            }

            // -- Phase 2: spawn polls for any watch whose per-item
            // cooldown has elapsed and which has no in-flight poll. --
            let to_spawn = collect_due_pr_merge_watches(
                &self.$watches_field,
                &self.$polls_field,
            );
            for (wi_id, pr_number, owner_repo, branch, repo_path) in to_spawn {
                let rx = spawn_gh_pr_view_poll(
                    wi_id.clone(),
                    pr_number,
                    owner_repo,
                    branch.clone(),
                    repo_path,
                );
                let activity =
                    self.activities.start(format!("{} ({branch})", $poll_label_prefix));
                self.$polls_field
                    .insert(wi_id.clone(), PrMergePollState { rx, activity });
                if let Some(w) = self
                    .$watches_field
                    .iter_mut()
                    .find(|w| w.wi_id == wi_id)
                {
                    w.last_polled = Some(crate::side_effects::clock::instant_now());
                }
            }
        }
    };
}

use std::path::PathBuf;

use crate::work_item::{WorkItemId, WorkItemKind, WorkItemStatus};
use crate::work_item_backend::ActivityEntry;

/// Tuple of fields captured from a `PrMergeWatch` at the moment
/// `collect_due_pr_merge_watches` decides the watch is due for a
/// new poll. The fields match the argv shape that
/// `spawn_gh_pr_view_poll` takes.
type PrMergeWatchSpawn = (WorkItemId, Option<u64>, String, String, PathBuf);

/// Helper shared by `impl_pr_merge_poll_method!`: Phase 1 drain of an
/// in-flight poll map. Removes every entry whose receiver has a ready
/// value or is disconnected, ending the corresponding activity, and
/// returns the successfully-received results.
fn drain_pr_merge_poll_results(
    polls: &mut std::collections::HashMap<WorkItemId, PrMergePollState>,
    activities: &mut Activities,
) -> Vec<PrMergePollResult> {
    // Collect into locals before acting so we don't borrow `self`
    // twice when calling into `apply_stage_change`, `end_activity`,
    // etc.
    let mut ready: Vec<PrMergePollResult> = Vec::new();
    let mut to_remove: Vec<WorkItemId> = Vec::new();
    for (wi_id, state) in polls.iter() {
        match state.rx.try_recv() {
            Ok(r) => {
                ready.push(r);
                to_remove.push(wi_id.clone());
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                to_remove.push(wi_id.clone());
            }
        }
    }
    for wi_id in &to_remove {
        if let Some(state) = polls.remove(wi_id) {
            activities.end(state.activity);
        }
    }
    ready
}

/// Helper shared by `impl_pr_merge_poll_method!`: actual-status guard
/// for a poll result. Re-checks the item is still in the expected
/// source stage (and matches the optional kind filter). The user may
/// have retreated / deleted the item between the spawn and the drain;
/// in that case the caller drops the watch / error entries.
fn pr_merge_poll_still_eligible(
    work_items: &[crate::work_item::WorkItem],
    result: &PrMergePollResult,
    source_stage: WorkItemStatus,
    kind_filter: Option<&WorkItemKind>,
) -> bool {
    work_items.iter().any(|w| {
        w.id == result.wi_id && w.status == source_stage && kind_filter.is_none_or(|k| w.kind == *k)
    })
}

/// Helper shared by `impl_pr_merge_poll_method!`: collect the watches
/// whose per-item cooldown has elapsed and which have no in-flight
/// poll, returning the tuples needed to spawn new polls.
fn collect_due_pr_merge_watches(
    watches: &[PrMergeWatch],
    polls: &std::collections::HashMap<WorkItemId, PrMergePollState>,
) -> Vec<PrMergeWatchSpawn> {
    let cooldown = std::time::Duration::from_secs(30);
    let now = crate::side_effects::clock::instant_now();
    let mut to_spawn: Vec<PrMergeWatchSpawn> = Vec::new();
    for watch in watches {
        if polls.contains_key(&watch.wi_id) {
            continue;
        }
        if let Some(last) = watch.last_polled
            && now.duration_since(last) < cooldown
        {
            continue;
        }
        to_spawn.push((
            watch.wi_id.clone(),
            watch.pr_number,
            watch.owner_repo.clone(),
            watch.branch.clone(),
            watch.repo_path.clone(),
        ));
    }
    to_spawn
}

impl App {
    /// Helper shared by `impl_pr_merge_poll_method!`: finalize a
    /// `MERGED` poll result. Persists PR identity so the subsequent
    /// reassembly can synthesize a Merged `PrInfo`, logs the merge to
    /// the activity log, optionally tears down the worktree, and
    /// advances the item to `Done`.
    fn finalize_pr_merge_poll_merged(
        &mut self,
        result: &PrMergePollResult,
        strategy_tag: &str,
        source_stage: WorkItemStatus,
        merged_message: &str,
        cleanup_worktree: bool,
    ) {
        // Persist PR identity BEFORE the stage change
        // so the subsequent `reassemble_work_items`
        // (fired from inside `apply_stage_change`)
        // finds the snapshot and can synthesize a
        // `PrInfo { state: Merged }` via the assembly
        // fallback. That in turn makes
        // `status_derived = true` and gives the item a
        // stable merged-PR link in the UI even after
        // the next fetch cycle clears any transient
        // data.
        if let Some(identity) = &result.pr_identity
            && let Err(e) =
                self.services
                    .backend
                    .save_pr_identity(&result.wi_id, &result.repo_path, identity)
        {
            self.shell.status_message = Some(format!("PR identity save error: {e}"));
        }

        let log_entry = ActivityEntry {
            timestamp: now_iso8601(),
            event_type: "pr_merged".to_string(),
            payload: serde_json::json!({
                "strategy": strategy_tag,
                "branch": result.branch,
            }),
        };
        if let Err(e) = self
            .services
            .backend
            .append_activity(&result.wi_id, &log_entry)
        {
            self.shell.status_message = Some(format!("Activity log error: {e}"));
        }

        if cleanup_worktree {
            self.spawn_post_merge_worktree_cleanup(&result.wi_id);
        }

        self.apply_stage_change(
            &result.wi_id,
            source_stage,
            WorkItemStatus::Done,
            "pr_merge",
        );
        self.shell.status_message = Some(merged_message.into());
    }
}

/// Generate a `reconstruct_*_watches` method that rebuilds one PR-merge
/// poller's watch list from the current `work_items` snapshot.
///
/// Both pollers share the same reconstruction shape: idempotently add a
/// watch for each eligible item that doesn't already have one. This is
/// strictly additive - stale watches / in-flight polls / errors for
/// items that are no longer eligible are cleaned up lazily by the
/// corresponding poll method's Phase 1 drain (via its still-eligible
/// guard), not here. That matches the historical `poll_mergequeue` flow
/// and avoids a subtle interaction with `reassemble_work_items`: after
/// a transient backend-read failure `work_items` can briefly be empty
/// even though the real state hasn't changed, and a proactive prune
/// here would then evict live watches that should have survived the
/// read.
macro_rules! impl_pr_merge_reconstruct_method {
    (
        $(#[$meta:meta])*
        fn $method_name:ident,
        watches = $watches_field:ident,
        source_stage = $source_stage:expr,
        kind_filter = $kind_filter:expr,
    ) => {
        $(#[$meta])*
        pub fn $method_name(&mut self) {
            let kind_filter: Option<WorkItemKind> = $kind_filter;

            for wi in &self.work_items {
                if wi.status != $source_stage {
                    continue;
                }
                if let Some(k) = kind_filter.as_ref()
                    && wi.kind != *k
                {
                    continue;
                }
                if self.$watches_field.iter().any(|w| w.wi_id == wi.id) {
                    continue;
                }
                let Some(assoc) = wi.repo_associations.first() else {
                    continue;
                };
                let Some(ref branch) = assoc.branch else {
                    continue;
                };
                // Read the GitHub remote from the cached fetcher
                // result so we never shell out to `git remote get-url`
                // on the UI thread. When the fetcher has not yet
                // populated this repo, skip - the next reassembly
                // (triggered on fetch completion) will retry.
                let Some((owner, repo_name)) = self
                    .repo_data
                    .get(&assoc.repo_path)
                    .and_then(|rd| rd.github_remote.clone())
                else {
                    continue;
                };
                // If the fetcher has already populated assoc.pr, pin
                // the number immediately. Otherwise the watch starts
                // with pr_number = None and the first poll falls back
                // to `gh pr view <branch>`. For ReviewRequest items
                // the fetcher almost never populates this (the
                // `--author @me` filter drops their author-side PRs),
                // so the branch fallback is the normal path there.
                let pr_number = assoc.pr.as_ref().map(|pr| pr.number);
                self.$watches_field.push(PrMergeWatch {
                    wi_id: wi.id.clone(),
                    pr_number,
                    owner_repo: format!("{owner}/{repo_name}"),
                    branch: branch.clone(),
                    repo_path: assoc.repo_path.clone(),
                    last_polled: None,
                });
            }
        }
    };
}

// === Submodule declarations ===
//
// Subsystem structs (`Toasts`, `Activities`, `ClickTracking`,
// `UserActionGuard`) live in their own owning-struct modules. The
// remainder of `impl App` is split into subsystem-named files below.
// Methods are grouped by the subsystem concern they serve - NOT by
// line-count budget. Each file's doc comment names the subsystem it
// implements. Moving methods between files is a logical-ownership
// change, not a mechanical one.
mod activities;
mod cleanup;
mod click_tracking;
mod display_list;
mod fetcher_bridge;
mod flags;
mod gate_polling;
mod global_drawer;
mod global_drawer_polling;
mod harness;
mod helpers;
mod mcp_bridge_and_imports;
mod mergequeue;
mod metrics;
mod orphan_cleanup;
mod pr_creation;
mod pr_creation_thread;
mod pr_identity_backfill;
mod pr_merge_and_review;
mod rebase_gate_compute;
mod rebase_gate_result;
mod rebase_gate_spawn;
mod review_gate;
mod session_open_prep;
mod session_spawn;
mod sessions_core;
mod settings_overlay;
mod setup_and_user_actions;
mod shared_services;
mod shell;
mod stage_transitions;
mod struct_app;
#[cfg(test)]
mod stubs;
#[cfg(test)]
mod tests;
mod toasts;
mod types_01;
mod types_02;
mod user_actions;
mod work_item_ops;
mod worktree_and_first_run;

// Re-exports so `super::<Type>` / `super::<helper>` paths in sibling
// submodules keep resolving without changing the import shape.
pub use activities::*;
pub use click_tracking::*;
pub use flags::*;
pub use global_drawer::*;
pub use helpers::*;
pub use metrics::*;
pub use orphan_cleanup::*;
pub use pr_identity_backfill::*;
pub use settings_overlay::*;
pub use shared_services::*;
pub use shell::*;
pub use struct_app::*;
#[cfg(test)]
pub use stubs::*;
pub use toasts::*;
pub use types_01::*;
pub use types_02::*;
pub use user_actions::*;
