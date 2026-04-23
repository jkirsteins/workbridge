//! Assembly layer: bridges raw data sources (backend records + per-repo
//! GitHub fetch results) and the display model consumed by the UI.
//!
//! The public entry point is [`reassemble`], which produces a fully
//! populated `Vec<WorkItem>` plus the sidebar lists (unlinked PRs,
//! review-requested PRs) and a list of item ids that should be reopened
//! because a reviewer has re-requested review on an already-Done item.
//!
//! Implementation is split across three files:
//!
//! - [`convert`] holds the pure projections from `GithubPr` / `GithubIssue`
//!   / `WorkItemId` to the display enums.
//! - [`query`] holds the lookup helpers that scan per-repo fetch results
//!   and the two public collectors (`collect_unlinked_prs`,
//!   `collect_review_requested_prs`).
//! - This file holds the `reassemble` driver that composes both halves.

mod convert;
mod query;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;

pub use self::convert::derive_fallback_title;
use self::convert::{backend_type_from_id, convert_issue, convert_pr};
pub use self::query::{collect_review_requested_prs, collect_unlinked_prs};
use self::query::{
    extract_issue_number, find_issue_in_fetch, find_prs_by_branch, find_worktree_by_branch,
    issue_was_attempted, worktree_target_path,
};
use crate::work_item::{
    CheckStatus, GitState, IssueInfo, MergeableState, PrInfo, PrState, RepoAssociation,
    RepoFetchResult, ReviewDecision, ReviewRequestedPr, UnlinkedPr, WorkItem, WorkItemError,
    WorkItemId, WorkItemKind, WorkItemStatus,
};
use crate::work_item_backend::{RepoAssociationRecord, WorkItemRecord};

/// Reassemble work items from backend records and fetched repo data.
///
/// This is the core assembly function that bridges raw data sources and
/// the display model. It:
/// 1. Starts with backend records as skeleton work items
/// 2. Fills in worktree paths, git state, PR info, and issue info by
///    matching branch names against fetched repo data
/// 3. Derives titles (PR title > issue title > backend title > branch > "untitled")
/// 4. Collects errors (multiple PRs for branch, detached HEAD, issue not found)
/// 5. Identifies unlinked PRs (PRs whose branch does not appear in any work item)
pub fn reassemble(
    backend_records: &[WorkItemRecord],
    repo_data: &HashMap<PathBuf, RepoFetchResult>,
    issue_pattern: &str,
    worktree_dir: &str,
) -> (
    Vec<WorkItem>,
    Vec<UnlinkedPr>,
    Vec<ReviewRequestedPr>,
    Vec<WorkItemId>,
) {
    let pattern = Regex::new(issue_pattern).ok();

    // Track all branches claimed by work items so we can find unlinked PRs.
    let mut claimed_branches: HashSet<(PathBuf, String)> = HashSet::new();

    let mut work_items = Vec::new();

    for record in backend_records {
        let mut assembled_associations = Vec::new();
        let mut errors: Vec<WorkItemError> = Vec::new();
        let mut best_pr_title: Option<String> = None;
        let mut best_issue_title: Option<String> = None;
        let mut first_branch: Option<String> = None;

        for assoc_record in &record.repo_associations {
            let repo_path = &assoc_record.repo_path;

            let Some(branch) = &assoc_record.branch else {
                // branch=None: pre-planning state, skip all matching
                assembled_associations.push(RepoAssociation {
                    repo_path: repo_path.clone(),
                    branch: None,
                    worktree_path: None,
                    pr: None,
                    issue: None,
                    stale_worktree_path: None,
                    git_state: None,
                });
                continue;
            };

            if first_branch.is_none() {
                first_branch = Some(branch.clone());
            }

            // Register this branch as claimed.
            claimed_branches.insert((repo_path.clone(), branch.clone()));

            let fetch = repo_data.get(repo_path);

            // --- Worktree matching ---
            let (worktree_path, git_state, stale_wt) =
                match_worktree_for_branch(fetch, repo_path, branch, worktree_dir);

            // --- PR matching ---
            let pr_info = match_pr_for_branch(
                fetch,
                record,
                assoc_record,
                repo_path,
                branch,
                &mut errors,
                &mut best_pr_title,
            );

            // --- Issue matching ---
            let issue_info = match_issue_for_branch(
                fetch,
                pattern.as_ref(),
                repo_path,
                branch,
                &mut errors,
                &mut best_issue_title,
            );

            assembled_associations.push(RepoAssociation {
                repo_path: repo_path.clone(),
                branch: Some(branch.clone()),
                worktree_path,
                pr: pr_info,
                issue: issue_info,
                git_state,
                stale_worktree_path: stale_wt,
            });
        }

        // --- Title derivation ---
        // Priority: PR title > issue title > backend title > branch > "untitled"
        let title = derive_work_item_title(
            best_pr_title,
            best_issue_title,
            record,
            first_branch.as_ref(),
        );

        // --- Status ---
        // Done is derived: if any repo association has a merged PR, the work
        // item is Done regardless of the backend record's status. If the PR
        // gets reopened, the item reverts to its backend status.
        let has_merged_pr = assembled_associations
            .iter()
            .any(|a| a.pr.as_ref().is_some_and(|pr| pr.state == PrState::Merged));
        let (status, status_derived) = if has_merged_pr {
            (WorkItemStatus::Done, true)
        } else {
            (record.status, false)
        };

        work_items.push(WorkItem {
            id: record.id.clone(),
            backend_type: backend_type_from_id(&record.id),
            kind: record.kind.clone(),
            title,
            // display_id is a pass-through from the backend record -
            // the assembly layer does not derive it. `None` for
            // pre-feature records, which the list renderer silently
            // skips.
            display_id: record.display_id.clone(),
            description: record.description.clone(),
            status,
            status_derived,
            repo_associations: assembled_associations,
            errors,
        });
    }

    // --- Detect Done ReviewRequest items that should be re-opened ---
    let reopen_ids = detect_reopen_review_request_ids(&work_items, repo_data);

    // --- Collect unlinked PRs ---
    // All fetched PRs are already filtered to the authenticated user via
    // `--author @me` in the gh CLI calls, so no additional author check needed.
    let unlinked_prs = collect_unlinked_prs(repo_data, &claimed_branches);

    // --- Collect review-requested PRs ---
    let review_requested_prs = collect_review_requested_prs(repo_data, &claimed_branches);

    (work_items, unlinked_prs, review_requested_prs, reopen_ids)
}

/// Match a GitHub issue for `branch` (extracted via `issue_pattern`),
/// falling back to appending an `IssueNotFound` error only when the
/// fetcher actually attempted the lookup. Seeds `best_issue_title`
/// from the first matched issue title.
fn match_issue_for_branch(
    fetch: Option<&RepoFetchResult>,
    pattern: Option<&Regex>,
    repo_path: &Path,
    branch: &str,
    errors: &mut Vec<WorkItemError>,
    best_issue_title: &mut Option<String>,
) -> Option<IssueInfo> {
    let pat = pattern?;
    let issue_number = extract_issue_number(branch, pat)?;
    let fetch = fetch?;
    // Only evaluate IssueNotFound when fetch data exists for
    // this repo. When fetch is None (startup, unfetched repo),
    // we skip - the error will surface once the first fetch
    // completes.
    find_issue_in_fetch(&fetch.issues, issue_number).map_or_else(
        || {
            // The fetcher tried to fetch this issue but got an error
            // (e.g. 404). Only emit IssueNotFound when the fetcher
            // actually attempted the lookup. If the issue number is
            // absent from fetch.issues entirely, the fetcher never
            // tried (e.g. no worktree for this branch), so we leave
            // issue as None without an error.
            if issue_was_attempted(&fetch.issues, issue_number) {
                errors.push(WorkItemError::IssueNotFound {
                    repo_path: repo_path.to_path_buf(),
                    issue_number,
                });
            }
            None
        },
        |gh_issue| {
            let info = convert_issue(gh_issue);
            if best_issue_title.is_none() {
                *best_issue_title = Some(info.title.clone());
            }
            Some(info)
        },
    )
}

/// Derive the display title for a work item using the priority
/// PR title > issue title > backend title > branch > "untitled".
fn derive_work_item_title(
    best_pr_title: Option<String>,
    best_issue_title: Option<String>,
    record: &WorkItemRecord,
    first_branch: Option<&String>,
) -> String {
    best_pr_title.map_or_else(
        || {
            best_issue_title.map_or_else(
                || derive_fallback_title(&record.title, first_branch),
                |issue_title| {
                    if issue_title.is_empty() {
                        derive_fallback_title(&record.title, first_branch)
                    } else {
                        issue_title
                    }
                },
            )
        },
        |pr_title| {
            if pr_title.is_empty() {
                derive_fallback_title(&record.title, first_branch)
            } else {
                pr_title
            }
        },
    )
}

/// Build a set of `(repo_path, branch)` pairs where a review is
/// currently requested, then flag any Done `ReviewRequest` work items
/// whose (non-derived) status should be flipped back to Review.
fn detect_reopen_review_request_ids(
    work_items: &[WorkItem],
    repo_data: &HashMap<PathBuf, RepoFetchResult>,
) -> Vec<WorkItemId> {
    let mut review_requested_set: HashSet<(PathBuf, String)> = HashSet::new();
    for (repo_path, fetch) in repo_data {
        if let Ok(prs) = &fetch.review_requested_prs {
            for pr in prs {
                if !pr.head_branch.is_empty() {
                    review_requested_set.insert((repo_path.clone(), pr.head_branch.clone()));
                }
            }
        }
    }
    let mut reopen_ids: Vec<WorkItemId> = Vec::new();
    for wi in work_items {
        if wi.kind == WorkItemKind::ReviewRequest
            && wi.status == WorkItemStatus::Done
            && !wi.status_derived
        {
            for assoc in &wi.repo_associations {
                if let Some(ref branch) = assoc.branch
                    && review_requested_set.contains(&(assoc.repo_path.clone(), branch.clone()))
                {
                    reopen_ids.push(wi.id.clone());
                    break;
                }
            }
        }
    }
    reopen_ids
}

/// Match the expected worktree for `branch` in the given `fetch`
/// result. Returns the worktree path, the derived git state, and the
/// stale-worktree path (a detached-HEAD worktree sitting at the
/// expected target, typically left by an interrupted rebase). Pure
/// in-memory projection - respects the "no blocking I/O on the UI
/// thread" invariant.
fn match_worktree_for_branch(
    fetch: Option<&RepoFetchResult>,
    repo_path: &Path,
    branch: &str,
    worktree_dir: &str,
) -> (Option<PathBuf>, Option<GitState>, Option<PathBuf>) {
    let mut worktree_path: Option<PathBuf> = None;
    let mut git_state: Option<GitState> = None;
    let mut stale_wt: Option<PathBuf> = None;
    if let Some(fetch) = fetch
        && let Ok(worktrees) = &fetch.worktrees
        && let Some(wt) = find_worktree_by_branch(worktrees, branch)
    {
        worktree_path = Some(wt.path.clone());
        // Cleanliness fields on `WorktreeInfo` are populated by
        // the background fetcher (see `list_worktrees` in
        // `src/worktree_service.rs`). Reading them here is a
        // pure in-memory projection and respects the "no
        // blocking I/O on the UI thread" invariant. `None`
        // means "check not attempted or failed" and collapses
        // to the safe default of clean/zero so an unknown
        // state never flags a worktree as dirty/unpushed.
        //
        // `dirty` on `GitState` is the union of uncommitted
        // tracked changes and untracked files because both
        // block merging identically - callers that want to
        // distinguish them go through
        // `MergeReadiness::classify`, which reads the raw
        // `WorktreeInfo` fields directly.
        git_state = Some(GitState {
            dirty: wt.dirty.unwrap_or(false) || wt.untracked.unwrap_or(false),
            ahead: wt.unpushed.unwrap_or(0),
            behind: wt.behind_remote.unwrap_or(0),
        });
    } else if let Some(fetch) = fetch
        && let Ok(worktrees) = &fetch.worktrees
    {
        // No branch match - check for a detached-HEAD worktree
        // at the expected target path (stale worktree detection).
        let target = worktree_target_path(repo_path, branch, worktree_dir);
        if let Some(wt) = worktrees
            .iter()
            .find(|w| w.branch.is_none() && !w.is_main && w.path == target)
        {
            stale_wt = Some(wt.path.clone());
        }
    }
    (worktree_path, git_state, stale_wt)
}

/// Match a live PR for `branch`, or fall back to a persisted PR
/// identity for Done items. Updates `errors` with a
/// `MultiplePrsForBranch` entry when more than one live PR matches,
/// and seeds `best_pr_title` from the first PR title seen.
fn match_pr_for_branch(
    fetch: Option<&RepoFetchResult>,
    record: &WorkItemRecord,
    assoc_record: &RepoAssociationRecord,
    repo_path: &Path,
    branch: &str,
    errors: &mut Vec<WorkItemError>,
    best_pr_title: &mut Option<String>,
) -> Option<PrInfo> {
    let mut pr_info: Option<PrInfo> = None;

    // Extract repo owner from the fetch data for fork PR filtering.
    let repo_owner_str: Option<String> = fetch
        .and_then(|f| f.github_remote.as_ref())
        .map(|(owner, _)| owner.clone());

    if let Some(fetch) = fetch
        && let Ok(prs) = &fetch.prs
    {
        let matching = find_prs_by_branch(prs, branch, repo_owner_str.as_deref());
        if matching.len() > 1 {
            errors.push(WorkItemError::MultiplePrsForBranch {
                repo_path: repo_path.to_path_buf(),
                branch: branch.to_string(),
                count: matching.len(),
            });
        }
        if let Some(first_pr) = matching.first() {
            let info = convert_pr(first_pr);
            if best_pr_title.is_none() {
                *best_pr_title = Some(info.title.clone());
            }
            pr_info = Some(info);
        }
    }

    // Fallback: if no live PR matched, use persisted PR identity
    // (saved at merge time) so Done items keep their PR link.
    // Guard: only apply when the backend record is already Done.
    // If the user moves the item back (e.g., merge reverted), the
    // persisted identity is ignored and the item is not forced to Done.
    if pr_info.is_none()
        && record.status == WorkItemStatus::Done
        && let Some(ref identity) = assoc_record.pr_identity
    {
        let info = PrInfo {
            number: identity.number,
            title: identity.title.clone(),
            state: PrState::Merged,
            is_draft: false,
            review_decision: ReviewDecision::None,
            checks: CheckStatus::None,
            mergeable: MergeableState::Unknown,
            url: identity.url.clone(),
        };
        if best_pr_title.is_none() {
            *best_pr_title = Some(info.title.clone());
        }
        pr_info = Some(info);
    }
    pr_info
}
