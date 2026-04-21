//! Lookup helpers that scan per-repo fetch results for matches against a
//! work item's branches and issue numbers, plus the collectors that
//! surface untracked PRs and review-requested PRs to the UI sidebar.
//!
//! Like [`super::convert`], every function here is a pure in-memory
//! projection: no blocking I/O, no hidden mutations. `reassemble` in
//! `super` composes these helpers with the conversion layer to assemble
//! display-ready `WorkItem`s.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;

use super::convert::{convert_pr, convert_review_decision};
use crate::github_client::{GithubIssue, GithubPr};
use crate::work_item::{RepoFetchResult, ReviewDecision, ReviewRequestedPr, UnlinkedPr};
use crate::worktree_service::WorktreeInfo;

/// Extract an issue number from a branch name using the given regex pattern.
///
/// The pattern must contain a capture group; the first capture group's content
/// is parsed as a u64 issue number. Returns None if the pattern does not match
/// or the capture is not a valid number.
pub(super) fn extract_issue_number(branch: &str, pattern: &Regex) -> Option<u64> {
    pattern
        .captures(branch)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
}

/// Find a worktree matching the given branch in the repo's fetched worktree list.
pub(super) fn find_worktree_by_branch<'a>(
    worktrees: &'a [WorktreeInfo],
    branch: &str,
) -> Option<&'a WorktreeInfo> {
    worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch))
}

/// Compute the expected worktree target path for a branch. Must match
/// `App::worktree_target_path` exactly so the stale-worktree detection
/// in the assembly layer agrees with the creation path in `spawn_session`.
pub(super) fn worktree_target_path(repo_path: &Path, branch: &str, worktree_dir: &str) -> PathBuf {
    let sanitized = branch.replace('/', "-");
    repo_path.join(worktree_dir).join(sanitized)
}

/// Find all PRs matching the given branch in the repo's fetched PR list.
///
/// When `repo_owner` is provided, fork PRs (where `head_repo_owner` differs
/// from the repo owner) are excluded. This prevents a fork PR from being
/// incorrectly matched to a local branch with the same name. PRs where
/// `head_repo_owner` is None (gh CLI did not return the field) are still
/// included to preserve backwards compatibility.
pub(super) fn find_prs_by_branch<'a>(
    prs: &'a [GithubPr],
    branch: &str,
    repo_owner: Option<&str>,
) -> Vec<&'a GithubPr> {
    prs.iter()
        .filter(|p| {
            if p.head_branch != branch {
                return false;
            }
            // If we know both the repo owner and the PR's head repo owner,
            // exclude fork PRs (where they differ).
            if let Some(owner) = repo_owner
                && let Some(ref pr_owner) = p.head_repo_owner
            {
                return pr_owner == owner;
            }
            true
        })
        .collect()
}

/// Look up an issue by number in the repo's fetched issue list.
pub(super) fn find_issue_in_fetch(
    issues: &[(u64, Result<GithubIssue, crate::github_client::GithubError>)],
    number: u64,
) -> Option<&GithubIssue> {
    issues.iter().find_map(|(n, result)| {
        if *n == number {
            result.as_ref().ok()
        } else {
            None
        }
    })
}

/// Check whether the fetcher attempted to look up an issue number.
///
/// Returns true if the issue number appears in the fetched issues list
/// (regardless of whether the result was Ok or Err). When true and
/// `find_issue_in_fetch` returned None, it means the fetch failed
/// (e.g. 404) and `IssueNotFound` is genuine. When false, the fetcher
/// never tried to fetch this issue (e.g. no worktree had a branch
/// matching this number), so we should not emit an error.
pub(super) fn issue_was_attempted(
    issues: &[(u64, Result<GithubIssue, crate::github_client::GithubError>)],
    number: u64,
) -> bool {
    issues.iter().any(|(n, _)| *n == number)
}

/// Collect PRs from all repos whose (`repo_path`, branch) is not already
/// claimed by a work item. All fetched PRs are pre-filtered to the
/// authenticated user via `--author @me` in the gh CLI calls.
pub fn collect_unlinked_prs(
    repo_data: &HashMap<PathBuf, RepoFetchResult>,
    claimed_branches: &HashSet<(PathBuf, String)>,
) -> Vec<UnlinkedPr> {
    let mut unlinked = Vec::new();
    for (repo_path, fetch) in repo_data {
        if let Ok(prs) = &fetch.prs {
            for pr in prs {
                if pr.head_branch.is_empty() {
                    continue;
                }
                // Defensive: the fetcher queries --state open, but stale
                // cached data can contain closed/merged PRs.
                if pr.state != "OPEN" {
                    continue;
                }
                if !claimed_branches.contains(&(repo_path.clone(), pr.head_branch.clone())) {
                    unlinked.push(UnlinkedPr {
                        repo_path: repo_path.clone(),
                        pr: convert_pr(pr),
                        branch: pr.head_branch.clone(),
                    });
                }
            }
        }
    }
    unlinked
}

/// Collect PRs where the authenticated user has been requested as a
/// reviewer. Skips PRs in two cases:
///
/// 1. The PR is already claimed by a work item (imported). The user is
///    tracking it through the normal work-item flow, so it should not
///    also appear as an untracked review request.
/// 2. The PR's review decision is `Approved` or `ChangesRequested`. Both
///    are non-actionable states for the current user - they have already
///    submitted a terminal review on this PR. Only `Pending`
///    (review required, not yet submitted) and `None` (no decision at
///    all) remain visible, which matches "items that still need my
///    action".
pub fn collect_review_requested_prs(
    repo_data: &HashMap<PathBuf, RepoFetchResult>,
    claimed_branches: &HashSet<(PathBuf, String)>,
) -> Vec<ReviewRequestedPr> {
    let mut result = Vec::new();
    for (repo_path, fetch) in repo_data {
        if let Ok(prs) = &fetch.review_requested_prs {
            for pr in prs {
                if pr.head_branch.is_empty() {
                    continue;
                }
                if claimed_branches.contains(&(repo_path.clone(), pr.head_branch.clone())) {
                    continue;
                }
                let decision = convert_review_decision(&pr.review_decision);
                if matches!(
                    decision,
                    ReviewDecision::Approved | ReviewDecision::ChangesRequested
                ) {
                    continue;
                }
                result.push(ReviewRequestedPr {
                    repo_path: repo_path.clone(),
                    pr: convert_pr(pr),
                    branch: pr.head_branch.clone(),
                    requested_reviewer_logins: pr.requested_reviewer_logins.clone(),
                    requested_team_slugs: pr.requested_team_slugs.clone(),
                });
            }
        }
    }
    result
}
