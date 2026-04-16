use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::github_client::{GithubIssue, GithubPr};
use crate::work_item::{
    CheckStatus, GitState, IssueInfo, IssueState, MergeableState, PrInfo, PrState, RepoAssociation,
    RepoFetchResult, ReviewDecision, ReviewRequestedPr, UnlinkedPr, WorkItem, WorkItemError,
    WorkItemId, WorkItemKind, WorkItemStatus,
};
use crate::work_item_backend::WorkItemRecord;
use crate::worktree_service::WorktreeInfo;

/// Convert a raw GithubPr into a display-ready PrInfo.
fn convert_pr(pr: &GithubPr) -> PrInfo {
    PrInfo {
        number: pr.number,
        title: pr.title.clone(),
        state: convert_pr_state(&pr.state),
        is_draft: pr.is_draft,
        review_decision: convert_review_decision(&pr.review_decision),
        checks: convert_check_status(&pr.status_check_rollup),
        mergeable: convert_mergeable_state(&pr.mergeable),
        url: pr.url.clone(),
    }
}

/// Convert a raw state string from GitHub into a PrState enum.
fn convert_pr_state(raw: &str) -> PrState {
    match raw.to_uppercase().as_str() {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    }
}

/// Convert a raw review decision string from GitHub into a ReviewDecision enum.
fn convert_review_decision(raw: &str) -> ReviewDecision {
    match raw {
        "APPROVED" => ReviewDecision::Approved,
        "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
        "REVIEW_REQUIRED" => ReviewDecision::Pending,
        _ => ReviewDecision::None,
    }
}

/// Convert a raw status check rollup string into a CheckStatus enum.
fn convert_check_status(raw: &str) -> CheckStatus {
    match raw {
        "SUCCESS" => CheckStatus::Passing,
        "PENDING" => CheckStatus::Pending,
        "FAILURE" => CheckStatus::Failing,
        "" => CheckStatus::None,
        _ => CheckStatus::Unknown,
    }
}

/// Convert a raw mergeable string from GitHub into a MergeableState enum.
fn convert_mergeable_state(raw: &str) -> MergeableState {
    match raw {
        "MERGEABLE" => MergeableState::Mergeable,
        "CONFLICTING" => MergeableState::Conflicting,
        _ => MergeableState::Unknown,
    }
}

/// Convert a raw GithubIssue into a display-ready IssueInfo.
fn convert_issue(issue: &GithubIssue) -> IssueInfo {
    let state = match issue.state.to_uppercase().as_str() {
        "CLOSED" => IssueState::Closed,
        _ => IssueState::Open,
    };
    IssueInfo {
        number: issue.number,
        title: issue.title.clone(),
        state,
        labels: issue.labels.clone(),
    }
}

/// Extract an issue number from a branch name using the given regex pattern.
///
/// The pattern must contain a capture group; the first capture group's content
/// is parsed as a u64 issue number. Returns None if the pattern does not match
/// or the capture is not a valid number.
fn extract_issue_number(branch: &str, pattern: &Regex) -> Option<u64> {
    pattern
        .captures(branch)
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
}

/// Find a worktree matching the given branch in the repo's fetched worktree list.
fn find_worktree_by_branch<'a>(
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
fn worktree_target_path(repo_path: &Path, branch: &str, worktree_dir: &str) -> PathBuf {
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
fn find_prs_by_branch<'a>(
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
fn find_issue_in_fetch(
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
/// (e.g. 404) and IssueNotFound is genuine. When false, the fetcher
/// never tried to fetch this issue (e.g. no worktree had a branch
/// matching this number), so we should not emit an error.
fn issue_was_attempted(
    issues: &[(u64, Result<GithubIssue, crate::github_client::GithubError>)],
    number: u64,
) -> bool {
    issues.iter().any(|(n, _)| *n == number)
}

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

            let branch = match &assoc_record.branch {
                Some(b) => b,
                None => {
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
                }
            };

            if first_branch.is_none() {
                first_branch = Some(branch.clone());
            }

            // Register this branch as claimed.
            claimed_branches.insert((repo_path.clone(), branch.clone()));

            let fetch = repo_data.get(repo_path);

            // --- Worktree matching ---
            let mut worktree_path: Option<PathBuf> = None;
            let mut git_state: Option<GitState> = None;

            // Detached worktrees (branch=None) don't match by branch,
            // but if one sits at the expected target path it is a stale
            // worktree left by an interrupted rebase. We detect this
            // proactively so the UI can show "recover" instead of the
            // normal "start a session" hint.
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
                // `WorktreeCleanliness::from_worktree_info`, which
                // reads the raw `WorktreeInfo` fields directly.
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

            // --- PR matching ---
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
                        repo_path: repo_path.clone(),
                        branch: branch.clone(),
                        count: matching.len(),
                    });
                }
                if let Some(first_pr) = matching.first() {
                    let info = convert_pr(first_pr);
                    if best_pr_title.is_none() {
                        best_pr_title = Some(info.title.clone());
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
                    best_pr_title = Some(info.title.clone());
                }
                pr_info = Some(info);
            }

            // --- Issue matching ---
            let mut issue_info: Option<IssueInfo> = None;

            if let Some(pat) = &pattern
                && let Some(issue_number) = extract_issue_number(branch, pat)
                && let Some(fetch) = fetch
            {
                // Only evaluate IssueNotFound when fetch data exists for
                // this repo. When fetch is None (startup, unfetched repo),
                // we skip - the error will surface once the first fetch
                // completes.
                if let Some(gh_issue) = find_issue_in_fetch(&fetch.issues, issue_number) {
                    let info = convert_issue(gh_issue);
                    if best_issue_title.is_none() {
                        best_issue_title = Some(info.title.clone());
                    }
                    issue_info = Some(info);
                } else if issue_was_attempted(&fetch.issues, issue_number) {
                    // The fetcher tried to fetch this issue but got an error
                    // (e.g. 404). Only emit IssueNotFound when the fetcher
                    // actually attempted the lookup. If the issue number is
                    // absent from fetch.issues entirely, the fetcher never
                    // tried (e.g. no worktree for this branch), so we leave
                    // issue as None without an error.
                    errors.push(WorkItemError::IssueNotFound {
                        repo_path: repo_path.clone(),
                        issue_number,
                    });
                }
            }

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
        let title = if let Some(pr_title) = best_pr_title {
            if !pr_title.is_empty() {
                pr_title
            } else {
                derive_fallback_title(&record.title, &first_branch)
            }
        } else if let Some(issue_title) = best_issue_title {
            if !issue_title.is_empty() {
                issue_title
            } else {
                derive_fallback_title(&record.title, &first_branch)
            }
        } else {
            derive_fallback_title(&record.title, &first_branch)
        };

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
    // Build a set of (repo_path, branch) pairs where a review is currently
    // requested so we can match against Done review request work items.
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
    for wi in &work_items {
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

    // --- Collect unlinked PRs ---
    // All fetched PRs are already filtered to the authenticated user via
    // `--author @me` in the gh CLI calls, so no additional author check needed.
    let unlinked_prs = collect_unlinked_prs(repo_data, &claimed_branches);

    // --- Collect review-requested PRs ---
    let review_requested_prs = collect_review_requested_prs(repo_data, &claimed_branches);

    (work_items, unlinked_prs, review_requested_prs, reopen_ids)
}

/// Derive a fallback title from backend title or branch name.
fn derive_fallback_title(backend_title: &str, first_branch: &Option<String>) -> String {
    if !backend_title.is_empty() {
        backend_title.to_string()
    } else if let Some(branch) = first_branch {
        branch.clone()
    } else {
        "untitled".to_string()
    }
}

/// Derive the BackendType from a WorkItemId.
fn backend_type_from_id(id: &crate::work_item::WorkItemId) -> crate::work_item::BackendType {
    match id {
        crate::work_item::WorkItemId::LocalFile(_) => crate::work_item::BackendType::LocalFile,
        crate::work_item::WorkItemId::GithubIssue { .. } => {
            crate::work_item::BackendType::GithubIssue
        }
        crate::work_item::WorkItemId::GithubProject { .. } => {
            crate::work_item::BackendType::GithubProject
        }
    }
}

/// Collect PRs from all repos whose (repo_path, branch) is not already
/// claimed by a work item. All fetched PRs are pre-filtered to the
/// authenticated user via `--author @me` in the gh CLI calls.
fn collect_unlinked_prs(
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
fn collect_review_requested_prs(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github_client::{GithubError, GithubIssue, GithubPr};
    use crate::work_item::{
        BackendType, CheckStatus, IssueState, MergeableState, PrState, ReviewDecision, WorkItemId,
        WorkItemKind, WorkItemStatus,
    };
    use crate::work_item_backend::{RepoAssociationRecord, WorkItemRecord};
    use crate::worktree_service::WorktreeInfo;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn repo_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/repos/{name}"))
    }

    fn create_mock_record(
        id_suffix: &str,
        title: &str,
        status: WorkItemStatus,
        associations: Vec<RepoAssociationRecord>,
    ) -> WorkItemRecord {
        WorkItemRecord {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from(format!("/data/{id_suffix}.json"))),
            title: title.to_string(),
            description: None,
            status,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: associations,
            plan: None,
            done_at: None,
        }
    }

    fn create_mock_pr(
        number: u64,
        title: &str,
        branch: &str,
        review: &str,
        checks: &str,
    ) -> GithubPr {
        GithubPr {
            number,
            title: title.to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            head_branch: branch.to_string(),
            url: format!("https://github.com/o/r/pull/{number}"),
            review_decision: review.to_string(),
            status_check_rollup: checks.to_string(),
            head_repo_owner: None,
            author: Some("testuser".to_string()),
            mergeable: String::new(),
            requested_reviewer_logins: Vec::new(),
            requested_team_slugs: Vec::new(),
        }
    }

    fn create_mock_pr_with_owner(
        number: u64,
        title: &str,
        branch: &str,
        review: &str,
        checks: &str,
        owner: Option<&str>,
    ) -> GithubPr {
        GithubPr {
            number,
            title: title.to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            head_branch: branch.to_string(),
            url: format!("https://github.com/o/r/pull/{number}"),
            review_decision: review.to_string(),
            status_check_rollup: checks.to_string(),
            head_repo_owner: owner.map(|s| s.to_string()),
            author: Some("testuser".to_string()),
            mergeable: String::new(),
            requested_reviewer_logins: Vec::new(),
            requested_team_slugs: Vec::new(),
        }
    }

    fn create_mock_issue(number: u64, title: &str) -> GithubIssue {
        GithubIssue {
            number,
            title: title.to_string(),
            state: "OPEN".to_string(),
            labels: vec![],
        }
    }

    fn create_mock_worktree(path: &str, branch: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            branch: branch.map(|s| s.to_string()),
            is_main: false,
            has_commits_ahead: None,
            dirty: None,
            untracked: None,
            unpushed: None,
            behind_remote: None,
        }
    }

    fn create_mock_repo_data(
        path: PathBuf,
        worktrees: Vec<WorktreeInfo>,
        prs: Vec<GithubPr>,
        issues: Vec<(u64, Result<GithubIssue, GithubError>)>,
    ) -> (PathBuf, RepoFetchResult) {
        let fetch = RepoFetchResult {
            repo_path: path.clone(),
            github_remote: Some(("owner".to_string(), "repo".to_string())),
            worktrees: Ok(worktrees),
            prs: Ok(prs),
            review_requested_prs: Ok(vec![]),
            current_user_login: None,
            issues,
        };
        (path, fetch)
    }

    const DEFAULT_ISSUE_PATTERN: &str = r"^(\d+)-";

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn assembles_basic_work_item() {
        let rp = repo_path("alpha");
        let branch = "42-fix-bug";

        let record = create_mock_record(
            "wi-1",
            "Backend title",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        let pr = create_mock_pr(10, "Fix the bug", branch, "APPROVED", "SUCCESS");
        let issue = create_mock_issue(42, "Bug report");
        let wt = create_mock_worktree("/worktrees/42-fix-bug", Some(branch));

        let (rp_key, fetch) =
            create_mock_repo_data(rp.clone(), vec![wt], vec![pr], vec![(42, Ok(issue))]);
        let repo_data: HashMap<PathBuf, RepoFetchResult> = HashMap::from([(rp_key, fetch)]);

        let (items, unlinked, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert_eq!(unlinked.len(), 0);

        let item = &items[0];
        // PR title wins over issue title and backend title.
        assert_eq!(item.title, "Fix the bug");
        assert_eq!(item.status, WorkItemStatus::Implementing);
        assert!(item.errors.is_empty());

        let assoc = &item.repo_associations[0];
        assert_eq!(assoc.repo_path, rp);
        assert_eq!(assoc.branch, Some(branch.to_string()));
        assert_eq!(
            assoc.worktree_path,
            Some(PathBuf::from("/worktrees/42-fix-bug"))
        );

        let pr_info = assoc.pr.as_ref().expect("should have PR info");
        assert_eq!(pr_info.number, 10);
        assert_eq!(pr_info.title, "Fix the bug");
        assert_eq!(pr_info.state, PrState::Open);
        assert_eq!(pr_info.review_decision, ReviewDecision::Approved);
        assert_eq!(pr_info.checks, CheckStatus::Passing);

        let issue_info = assoc.issue.as_ref().expect("should have issue info");
        assert_eq!(issue_info.number, 42);
        assert_eq!(issue_info.title, "Bug report");
        assert_eq!(issue_info.state, IssueState::Open);
    }

    #[test]
    fn title_derivation_priority() {
        let rp = repo_path("alpha");
        let branch = "42-fix-bug";

        // Level 1: PR title wins
        {
            let record = create_mock_record(
                "wi-1",
                "Backend title",
                WorkItemStatus::Backlog,
                vec![RepoAssociationRecord {
                    repo_path: rp.clone(),
                    branch: Some(branch.to_string()),
                    pr_identity: None,
                }],
            );
            let pr = create_mock_pr(10, "PR title", branch, "", "");
            let issue = create_mock_issue(42, "Issue title");
            let (rp_key, fetch) =
                create_mock_repo_data(rp.clone(), vec![], vec![pr], vec![(42, Ok(issue))]);
            let repo_data = HashMap::from([(rp_key, fetch)]);
            let (items, _, _, _) =
                reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
            assert_eq!(items[0].title, "PR title");
        }

        // Level 2: Issue title (no PR)
        {
            let record = create_mock_record(
                "wi-2",
                "Backend title",
                WorkItemStatus::Backlog,
                vec![RepoAssociationRecord {
                    repo_path: rp.clone(),
                    branch: Some(branch.to_string()),
                    pr_identity: None,
                }],
            );
            let issue = create_mock_issue(42, "Issue title");
            let (rp_key, fetch) =
                create_mock_repo_data(rp.clone(), vec![], vec![], vec![(42, Ok(issue))]);
            let repo_data = HashMap::from([(rp_key, fetch)]);
            let (items, _, _, _) =
                reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
            assert_eq!(items[0].title, "Issue title");
        }

        // Level 3: Backend title (no PR, no issue)
        {
            let record = create_mock_record(
                "wi-3",
                "Backend title",
                WorkItemStatus::Backlog,
                vec![RepoAssociationRecord {
                    repo_path: rp.clone(),
                    branch: Some(branch.to_string()),
                    pr_identity: None,
                }],
            );
            let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
            let repo_data = HashMap::from([(rp_key, fetch)]);
            let (items, _, _, _) =
                reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
            assert_eq!(items[0].title, "Backend title");
        }

        // Level 4: Branch name (no PR, no issue, empty backend title)
        {
            let record = create_mock_record(
                "wi-4",
                "",
                WorkItemStatus::Backlog,
                vec![RepoAssociationRecord {
                    repo_path: rp.clone(),
                    branch: Some("my-feature".to_string()),
                    pr_identity: None,
                }],
            );
            let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
            let repo_data = HashMap::from([(rp_key, fetch)]);
            // Use a pattern that won't match "my-feature"
            let (items, _, _, _) = reassemble(&[record], &repo_data, r"^(\d+)-", ".worktrees");
            assert_eq!(items[0].title, "my-feature");
        }

        // Level 5: "untitled" (no PR, no issue, empty backend title, no branch)
        {
            let record = create_mock_record(
                "wi-5",
                "",
                WorkItemStatus::Backlog,
                vec![RepoAssociationRecord {
                    repo_path: rp.clone(),
                    branch: None,
                    pr_identity: None,
                }],
            );
            let repo_data = HashMap::new();
            let (items, _, _, _) =
                reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
            assert_eq!(items[0].title, "untitled");
        }
    }

    #[test]
    fn unlinked_pr_detection() {
        let rp = repo_path("alpha");

        // Work item claims branch "feature-a"
        let record = create_mock_record(
            "wi-1",
            "My work",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some("feature-a".to_string()),
                pr_identity: None,
            }],
        );

        // Repo has PRs for "feature-a" (claimed) and "feature-b" (unlinked)
        let pr_a = create_mock_pr(1, "PR A", "feature-a", "", "");
        let pr_b = create_mock_pr(2, "PR B", "feature-b", "APPROVED", "SUCCESS");

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![pr_a, pr_b], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, unlinked, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert_eq!(unlinked.len(), 1);
        assert_eq!(unlinked[0].branch, "feature-b");
        assert_eq!(unlinked[0].pr.number, 2);
        assert_eq!(unlinked[0].pr.title, "PR B");
        assert_eq!(unlinked[0].repo_path, rp);
    }

    #[test]
    fn multi_repo_work_item() {
        let rp_a = repo_path("alpha");
        let rp_b = repo_path("beta");

        let record = create_mock_record(
            "wi-1",
            "Cross-repo work",
            WorkItemStatus::Implementing,
            vec![
                RepoAssociationRecord {
                    repo_path: rp_a.clone(),
                    branch: Some("feature-x".to_string()),
                    pr_identity: None,
                },
                RepoAssociationRecord {
                    repo_path: rp_b.clone(),
                    branch: Some("feature-x".to_string()),
                    pr_identity: None,
                },
            ],
        );

        let pr_a = create_mock_pr(10, "PR in alpha", "feature-x", "APPROVED", "SUCCESS");
        let pr_b = create_mock_pr(20, "PR in beta", "feature-x", "", "PENDING");

        let (key_a, fetch_a) = create_mock_repo_data(rp_a.clone(), vec![], vec![pr_a], vec![]);
        let (key_b, fetch_b) = create_mock_repo_data(rp_b.clone(), vec![], vec![pr_b], vec![]);
        let repo_data = HashMap::from([(key_a, fetch_a), (key_b, fetch_b)]);

        let (items, unlinked, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert_eq!(unlinked.len(), 0);

        let item = &items[0];
        assert_eq!(item.repo_associations.len(), 2);

        // First association's PR title becomes the title.
        assert_eq!(item.title, "PR in alpha");

        // Each association has its own PR info.
        let assoc_a = item
            .repo_associations
            .iter()
            .find(|a| a.repo_path == rp_a)
            .expect("should have alpha association");
        assert_eq!(assoc_a.pr.as_ref().unwrap().number, 10);
        assert_eq!(
            assoc_a.pr.as_ref().unwrap().review_decision,
            ReviewDecision::Approved
        );

        let assoc_b = item
            .repo_associations
            .iter()
            .find(|a| a.repo_path == rp_b)
            .expect("should have beta association");
        assert_eq!(assoc_b.pr.as_ref().unwrap().number, 20);
        assert_eq!(assoc_b.pr.as_ref().unwrap().checks, CheckStatus::Pending);
    }

    #[test]
    fn branch_none_skips_matching() {
        let rp = repo_path("alpha");

        let record = create_mock_record(
            "wi-1",
            "Planning item",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: None,
                pr_identity: None,
            }],
        );

        // Repo has a PR and worktree, but none should match since branch is None.
        let pr = create_mock_pr(1, "Some PR", "some-branch", "", "");
        let wt = create_mock_worktree("/worktrees/some-branch", Some("some-branch"));

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![wt], vec![pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, unlinked, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let assoc = &items[0].repo_associations[0];
        assert_eq!(assoc.branch, None);
        assert_eq!(assoc.worktree_path, None);
        assert!(assoc.pr.is_none());
        assert!(assoc.issue.is_none());
        assert!(assoc.git_state.is_none());

        // The PR on "some-branch" should be unlinked since no work item claims it.
        assert_eq!(unlinked.len(), 1);
        assert_eq!(unlinked[0].branch, "some-branch");
    }

    #[test]
    fn multiple_prs_for_branch() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // Two PRs on the same branch.
        let pr1 = create_mock_pr(1, "PR one", branch, "", "");
        let pr2 = create_mock_pr(2, "PR two", branch, "", "");

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![pr1, pr2], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let item = &items[0];

        // Should have a MultiplePrsForBranch error.
        assert!(
            item.errors.iter().any(|e| matches!(
                e,
                WorkItemError::MultiplePrsForBranch {
                    branch: b,
                    count: 2,
                    ..
                } if b == "feature-x"
            )),
            "expected MultiplePrsForBranch error, got: {:?}",
            item.errors
        );

        // Should still fill the first PR.
        assert!(item.repo_associations[0].pr.is_some());
        assert_eq!(item.repo_associations[0].pr.as_ref().unwrap().number, 1);
    }

    /// A detached worktree (branch=None) does not match any work item and
    /// is not associated with any work item. This is the correct behavior
    /// since there is no branch to match on.
    #[test]
    fn detached_worktree_not_associated_with_work_item() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // Only a detached worktree exists - no worktree on feature-x.
        let wt_detached = create_mock_worktree("/worktrees/some-detached", None);

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![wt_detached], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let item = &items[0];

        // The detached worktree should not be associated with the work item.
        assert!(
            item.repo_associations[0].worktree_path.is_none(),
            "detached worktree should not match work item, got: {:?}",
            item.repo_associations[0].worktree_path,
        );
        assert!(
            item.repo_associations[0].git_state.is_none(),
            "git_state should be None when no worktree matches",
        );
        // No errors should be produced.
        assert!(
            item.errors.is_empty(),
            "no errors expected for detached worktrees, got: {:?}",
            item.errors,
        );
    }

    #[test]
    fn empty_inputs() {
        let repo_data: HashMap<PathBuf, RepoFetchResult> = HashMap::new();
        let (items, unlinked, _, _) =
            reassemble(&[], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
        assert!(items.is_empty());
        assert!(unlinked.is_empty());
    }

    /// Cache-projected cleanliness fields on `WorktreeInfo`
    /// (`dirty` / `untracked` / `unpushed` / `behind_remote`) must
    /// flow into the derived `GitState`. `dirty` on `GitState` is the
    /// union of tracked-dirty and untracked so the UI chip renders
    /// for either - the merge guard separates them via
    /// `WorktreeCleanliness::from_worktree_info`, which reads the
    /// raw fields.
    #[test]
    fn git_state_flows_from_worktree_info_fields() {
        let rp = repo_path("alpha");
        let branch = "feature-dirty";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Review,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        let wt = WorktreeInfo {
            path: PathBuf::from("/worktrees/feature-dirty"),
            branch: Some(branch.to_string()),
            is_main: false,
            dirty: Some(true),
            untracked: Some(false),
            unpushed: Some(2),
            behind_remote: Some(1),
            ..WorktreeInfo::default()
        };
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![wt], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let gs = items[0].repo_associations[0]
            .git_state
            .as_ref()
            .expect("git_state must be populated when a worktree matches");
        assert!(gs.dirty, "dirty=true must flow through");
        assert_eq!(gs.ahead, 2, "unpushed must flow into GitState.ahead");
        assert_eq!(gs.behind, 1, "behind_remote must flow into GitState.behind");
    }

    /// Tracked-dirty=false + untracked=true must still set
    /// `GitState.dirty=true` because the chip treats both the same.
    #[test]
    fn git_state_dirty_is_union_of_tracked_and_untracked() {
        let rp = repo_path("alpha");
        let branch = "feature-untracked";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Review,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        let wt = WorktreeInfo {
            path: PathBuf::from("/worktrees/feature-untracked"),
            branch: Some(branch.to_string()),
            is_main: false,
            dirty: Some(false),
            untracked: Some(true),
            unpushed: Some(0),
            behind_remote: Some(0),
            ..WorktreeInfo::default()
        };
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![wt], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        let gs = items[0].repo_associations[0]
            .git_state
            .as_ref()
            .expect("git_state must be populated");
        assert!(
            gs.dirty,
            "untracked-only must still set GitState.dirty so the chip renders",
        );
        assert_eq!(gs.ahead, 0);
        assert_eq!(gs.behind, 0);
    }

    /// `None` cleanliness fields (fetcher check failed / skipped) must
    /// collapse to the safe default: clean/zero counts.
    #[test]
    fn git_state_none_cleanliness_fields_default_to_clean() {
        let rp = repo_path("alpha");
        let branch = "feature-unknown";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Review,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // All cleanliness fields = None.
        let wt = create_mock_worktree("/worktrees/feature-unknown", Some(branch));
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![wt], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        let gs = items[0].repo_associations[0]
            .git_state
            .as_ref()
            .expect("git_state must be populated when worktree matches");
        assert!(!gs.dirty, "None dirty must default to clean");
        assert_eq!(gs.ahead, 0, "None unpushed must default to 0");
        assert_eq!(gs.behind, 0, "None behind_remote must default to 0");
    }

    #[test]
    fn reassemble_propagates_display_id() {
        // The assembly layer must pass `display_id` through from the
        // backend record unchanged. It is not derived - a legacy
        // record with `None` must produce a `WorkItem` with `None`,
        // and a record with `Some("foo-42")` must produce a
        // `WorkItem` with the same string.
        let rp = repo_path("alpha");
        let mut record = create_mock_record(
            "wi-display",
            "title",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: None,
                pr_identity: None,
            }],
        );
        record.display_id = Some("alpha-42".into());

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].display_id.as_deref(),
            Some("alpha-42"),
            "display_id must be passed through from the record"
        );

        // Legacy record without display_id -> None on the WorkItem.
        let legacy = create_mock_record(
            "wi-legacy",
            "title",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: None,
                pr_identity: None,
            }],
        );
        assert!(legacy.display_id.is_none());
        let (rp_key, fetch) = create_mock_repo_data(rp, vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);
        let (items, _, _, _) =
            reassemble(&[legacy], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");
        assert_eq!(items[0].display_id, None);
    }

    #[test]
    fn issue_extraction_from_branch() {
        let rp = repo_path("alpha");
        let branch = "42-fix-bug";

        let record = create_mock_record(
            "wi-1",
            "",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        let issue = create_mock_issue(42, "Fix the bug");

        let (rp_key, fetch) =
            create_mock_repo_data(rp.clone(), vec![], vec![], vec![(42, Ok(issue))]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) = reassemble(&[record], &repo_data, r"^(\d+)-", ".worktrees");

        assert_eq!(items.len(), 1);
        let assoc = &items[0].repo_associations[0];
        let issue_info = assoc.issue.as_ref().expect("should have issue info");
        assert_eq!(issue_info.number, 42);
        assert_eq!(issue_info.title, "Fix the bug");

        // Title should be the issue title (no PR available, empty backend title).
        assert_eq!(items[0].title, "Fix the bug");
    }

    #[test]
    fn issue_not_found_when_fetcher_attempted() {
        // IssueNotFound should only fire when the fetcher attempted the
        // lookup (issue number present in fetch.issues) but got an error.
        let rp = repo_path("alpha");
        let branch = "99-missing-issue";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // Fetcher attempted issue #99 but got an error (not found).
        let (rp_key, fetch) = create_mock_repo_data(
            rp.clone(),
            vec![],
            vec![],
            vec![(99, Err(GithubError::ApiError("not found".into())))],
        );
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert!(
            items[0].errors.iter().any(|e| matches!(
                e,
                WorkItemError::IssueNotFound {
                    issue_number: 99,
                    ..
                }
            )),
            "expected IssueNotFound error, got: {:?}",
            items[0].errors
        );
    }

    #[test]
    fn no_issue_not_found_when_fetcher_did_not_attempt() {
        // When the issue number is NOT in fetch.issues at all (fetcher
        // never tried, e.g. no worktree for this branch), we should NOT
        // emit IssueNotFound - just leave issue as None.
        let rp = repo_path("alpha");
        let branch = "99-missing-issue";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // Repo data exists but fetcher did not attempt issue #99.
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert!(
            items[0].errors.is_empty(),
            "expected no errors when fetcher did not attempt issue lookup, got: {:?}",
            items[0].errors
        );
        assert!(items[0].repo_associations[0].issue.is_none());
    }

    #[test]
    fn no_issue_not_found_when_fetch_data_absent() {
        // When repo_data has no entry for the repo (e.g. startup, before
        // first fetch), we must NOT produce an IssueNotFound error.
        let rp = repo_path("alpha");
        let branch = "99-missing-issue";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp,
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // Empty repo_data - simulates startup before any fetch completes.
        let repo_data: HashMap<PathBuf, RepoFetchResult> = HashMap::new();
        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert!(
            items[0].errors.is_empty(),
            "expected no errors when fetch data is absent, got: {:?}",
            items[0].errors
        );
        // Issue info should be None (not yet available).
        assert!(items[0].repo_associations[0].issue.is_none());
    }

    #[test]
    fn convert_pr_state_variants() {
        assert_eq!(convert_pr_state("OPEN"), PrState::Open);
        assert_eq!(convert_pr_state("CLOSED"), PrState::Closed);
        assert_eq!(convert_pr_state("MERGED"), PrState::Merged);
        assert_eq!(convert_pr_state("open"), PrState::Open);
        assert_eq!(convert_pr_state("merged"), PrState::Merged);
        assert_eq!(convert_pr_state(""), PrState::Open);
    }

    #[test]
    fn convert_review_decision_variants() {
        assert_eq!(
            convert_review_decision("APPROVED"),
            ReviewDecision::Approved
        );
        assert_eq!(
            convert_review_decision("CHANGES_REQUESTED"),
            ReviewDecision::ChangesRequested
        );
        assert_eq!(
            convert_review_decision("REVIEW_REQUIRED"),
            ReviewDecision::Pending
        );
        assert_eq!(convert_review_decision(""), ReviewDecision::None);
        assert_eq!(convert_review_decision("UNKNOWN"), ReviewDecision::None);
    }

    // -----------------------------------------------------------------------
    // collect_review_requested_prs filter tests
    // -----------------------------------------------------------------------

    /// Build a repo_data map with a single review-requested PR on the given
    /// branch and review decision. Helper for the filter tests below.
    fn repo_data_with_review_request(
        rp: PathBuf,
        branch: &str,
        review_decision: &str,
    ) -> HashMap<PathBuf, RepoFetchResult> {
        let pr = GithubPr {
            number: 1,
            title: "Needs your review".to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            head_branch: branch.to_string(),
            url: "https://github.com/o/r/pull/1".to_string(),
            review_decision: review_decision.to_string(),
            status_check_rollup: "".to_string(),
            head_repo_owner: Some("other".to_string()),
            author: Some("someone-else".to_string()),
            mergeable: String::new(),
            requested_reviewer_logins: Vec::new(),
            requested_team_slugs: Vec::new(),
        };
        let fetch = RepoFetchResult {
            repo_path: rp.clone(),
            github_remote: Some(("owner".to_string(), "repo".to_string())),
            worktrees: Ok(vec![]),
            prs: Ok(vec![]),
            review_requested_prs: Ok(vec![pr]),
            issues: vec![],
            current_user_login: None,
        };
        HashMap::from([(rp, fetch)])
    }

    #[test]
    fn review_requests_hidden_when_approved() {
        let repo_data = repo_data_with_review_request(repo_path("alpha"), "feat-a", "APPROVED");
        let claimed: HashSet<(PathBuf, String)> = HashSet::new();
        let result = collect_review_requested_prs(&repo_data, &claimed);
        assert!(
            result.is_empty(),
            "Approved review requests must not appear in the sidebar, got: {:?}",
            result.iter().map(|r| &r.branch).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn review_requests_hidden_when_changes_requested() {
        let repo_data =
            repo_data_with_review_request(repo_path("alpha"), "feat-a", "CHANGES_REQUESTED");
        let claimed: HashSet<(PathBuf, String)> = HashSet::new();
        let result = collect_review_requested_prs(&repo_data, &claimed);
        assert!(
            result.is_empty(),
            "ChangesRequested review requests must not appear in the sidebar, got: {:?}",
            result.iter().map(|r| &r.branch).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn review_requests_shown_when_pending() {
        let repo_data =
            repo_data_with_review_request(repo_path("alpha"), "feat-a", "REVIEW_REQUIRED");
        let claimed: HashSet<(PathBuf, String)> = HashSet::new();
        let result = collect_review_requested_prs(&repo_data, &claimed);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].branch, "feat-a");
        assert_eq!(result[0].pr.review_decision, ReviewDecision::Pending);
    }

    #[test]
    fn review_requests_shown_when_no_decision() {
        let repo_data = repo_data_with_review_request(repo_path("alpha"), "feat-a", "");
        let claimed: HashSet<(PathBuf, String)> = HashSet::new();
        let result = collect_review_requested_prs(&repo_data, &claimed);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].branch, "feat-a");
        assert_eq!(result[0].pr.review_decision, ReviewDecision::None);
    }

    #[test]
    fn review_requests_hidden_when_claimed_regardless_of_decision() {
        // Regression guard on the existing claim filter: even a
        // genuinely-actionable Pending review request must be hidden
        // when the branch is already tracked by a work item.
        let rp = repo_path("alpha");
        let repo_data = repo_data_with_review_request(rp.clone(), "feat-a", "REVIEW_REQUIRED");
        let mut claimed: HashSet<(PathBuf, String)> = HashSet::new();
        claimed.insert((rp, "feat-a".to_string()));
        let result = collect_review_requested_prs(&repo_data, &claimed);
        assert!(
            result.is_empty(),
            "Claimed review requests must not appear regardless of decision",
        );
    }

    #[test]
    fn convert_check_status_variants() {
        assert_eq!(convert_check_status("SUCCESS"), CheckStatus::Passing);
        assert_eq!(convert_check_status("PENDING"), CheckStatus::Pending);
        assert_eq!(convert_check_status("FAILURE"), CheckStatus::Failing);
        assert_eq!(convert_check_status(""), CheckStatus::None);
        assert_eq!(convert_check_status("SOMETHING"), CheckStatus::Unknown);
    }

    #[test]
    fn convert_mergeable_state_variants() {
        assert_eq!(
            convert_mergeable_state("MERGEABLE"),
            MergeableState::Mergeable
        );
        assert_eq!(
            convert_mergeable_state("CONFLICTING"),
            MergeableState::Conflicting
        );
        assert_eq!(convert_mergeable_state("UNKNOWN"), MergeableState::Unknown);
        assert_eq!(convert_mergeable_state(""), MergeableState::Unknown);
    }

    #[test]
    fn convert_issue_states() {
        let open_issue = GithubIssue {
            number: 1,
            title: "Open".to_string(),
            state: "OPEN".to_string(),
            labels: vec!["bug".to_string()],
        };
        let info = convert_issue(&open_issue);
        assert_eq!(info.state, IssueState::Open);
        assert_eq!(info.labels, vec!["bug"]);

        let closed_issue = GithubIssue {
            number: 2,
            title: "Closed".to_string(),
            state: "CLOSED".to_string(),
            labels: vec![],
        };
        let info = convert_issue(&closed_issue);
        assert_eq!(info.state, IssueState::Closed);
    }

    #[test]
    fn extract_issue_number_various_patterns() {
        let pat = Regex::new(r"^(\d+)-").unwrap();
        assert_eq!(extract_issue_number("42-fix-bug", &pat), Some(42));
        assert_eq!(extract_issue_number("123-add-feature", &pat), Some(123));
        assert_eq!(extract_issue_number("no-number-here", &pat), None);
        assert_eq!(extract_issue_number("", &pat), None);

        // Pattern with different format.
        let pat2 = Regex::new(r"issue-(\d+)").unwrap();
        assert_eq!(extract_issue_number("issue-55-fix", &pat2), Some(55));
        assert_eq!(extract_issue_number("feature-branch", &pat2), None);
    }

    #[test]
    fn invalid_issue_pattern_does_not_panic() {
        let rp = repo_path("alpha");
        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some("42-fix-bug".to_string()),
                pr_identity: None,
            }],
        );
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        // Invalid regex pattern should not panic - just skip issue extraction.
        let (items, _, _, _) = reassemble(&[record], &repo_data, "[invalid(", ".worktrees");
        assert_eq!(items.len(), 1);
        assert!(items[0].repo_associations[0].issue.is_none());
        // No IssueNotFound error either, since extraction was skipped.
        assert!(items[0].errors.is_empty());
    }

    #[test]
    fn unlinked_prs_from_multiple_repos() {
        let rp_a = repo_path("alpha");
        let rp_b = repo_path("beta");

        // No work items at all.
        let records: Vec<WorkItemRecord> = vec![];

        let pr_a = create_mock_pr(1, "PR in alpha", "feature-a", "", "");
        let pr_b = create_mock_pr(2, "PR in beta", "feature-b", "", "");

        let (key_a, fetch_a) = create_mock_repo_data(rp_a.clone(), vec![], vec![pr_a], vec![]);
        let (key_b, fetch_b) = create_mock_repo_data(rp_b.clone(), vec![], vec![pr_b], vec![]);
        let repo_data = HashMap::from([(key_a, fetch_a), (key_b, fetch_b)]);

        let (items, unlinked, _, _) =
            reassemble(&records, &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert!(items.is_empty());
        assert_eq!(unlinked.len(), 2);

        let branches: Vec<&str> = unlinked.iter().map(|u| u.branch.as_str()).collect();
        assert!(branches.contains(&"feature-a"));
        assert!(branches.contains(&"feature-b"));
    }

    #[test]
    fn worktree_not_found_leaves_path_none() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // Repo data has a worktree on a different branch.
        let wt = create_mock_worktree("/worktrees/other-branch", Some("other-branch"));

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![wt], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let assoc = &items[0].repo_associations[0];
        assert_eq!(assoc.worktree_path, None);
        assert!(assoc.git_state.is_none());
        // No WorktreeGone error in v1 - just None.
        assert!(items[0].errors.is_empty());
    }

    #[test]
    fn status_derived_from_backend_record() {
        let rp = repo_path("alpha");

        let todo_record = create_mock_record(
            "wi-1",
            "Todo work",
            WorkItemStatus::Backlog,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: None,
                pr_identity: None,
            }],
        );

        let in_progress_record = create_mock_record(
            "wi-2",
            "In progress work",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: None,
                pr_identity: None,
            }],
        );

        let repo_data = HashMap::new();
        let (items, _, _, _) = reassemble(
            &[todo_record, in_progress_record],
            &repo_data,
            DEFAULT_ISSUE_PATTERN,
            ".worktrees",
        );

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].status, WorkItemStatus::Backlog);
        assert_eq!(items[1].status, WorkItemStatus::Implementing);
    }

    #[test]
    fn backend_type_derived_from_id() {
        let records = vec![
            WorkItemRecord {
                display_id: None,
                id: WorkItemId::LocalFile(PathBuf::from("/data/wi.json")),
                title: "Local".to_string(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: repo_path("alpha"),
                    branch: None,
                    pr_identity: None,
                }],
                plan: None,
                done_at: None,
            },
            WorkItemRecord {
                display_id: None,
                id: WorkItemId::GithubIssue {
                    owner: "o".to_string(),
                    repo: "r".to_string(),
                    number: 1,
                },
                title: "GH Issue".to_string(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: repo_path("alpha"),
                    branch: None,
                    pr_identity: None,
                }],
                plan: None,
                done_at: None,
            },
            WorkItemRecord {
                display_id: None,
                id: WorkItemId::GithubProject {
                    node_id: "node123".to_string(),
                },
                title: "GH Project".to_string(),
                description: None,
                status: WorkItemStatus::Backlog,
                kind: WorkItemKind::Own,
                repo_associations: vec![RepoAssociationRecord {
                    repo_path: repo_path("alpha"),
                    branch: None,
                    pr_identity: None,
                }],
                plan: None,
                done_at: None,
            },
        ];

        let repo_data = HashMap::new();
        let (items, _, _, _) =
            reassemble(&records, &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items[0].backend_type, BackendType::LocalFile);
        assert_eq!(items[1].backend_type, BackendType::GithubIssue);
        assert_eq!(items[2].backend_type, BackendType::GithubProject);
    }

    #[test]
    fn merged_pr_derives_done_status() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        // Backend record says InProgress, but the PR is merged.
        let record = create_mock_record(
            "wi-1",
            "Ship it",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        let mut pr = create_mock_pr(10, "Ship it", branch, "APPROVED", "SUCCESS");
        pr.state = "MERGED".to_string();

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].status,
            WorkItemStatus::Done,
            "merged PR should derive Done status",
        );
    }

    #[test]
    fn open_pr_keeps_backend_status() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        // Backend record says InProgress, PR is open -> stays InProgress.
        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        let pr = create_mock_pr(10, "Work", branch, "", "");

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].status,
            WorkItemStatus::Implementing,
            "open PR should keep backend InProgress status",
        );
    }

    #[test]
    fn multi_repo_one_merged_pr_derives_done() {
        let rp_a = repo_path("alpha");
        let rp_b = repo_path("beta");

        let record = create_mock_record(
            "wi-1",
            "Cross-repo",
            WorkItemStatus::Implementing,
            vec![
                RepoAssociationRecord {
                    repo_path: rp_a.clone(),
                    branch: Some("feature-x".to_string()),
                    pr_identity: None,
                },
                RepoAssociationRecord {
                    repo_path: rp_b.clone(),
                    branch: Some("feature-x".to_string()),
                    pr_identity: None,
                },
            ],
        );

        // alpha PR is open, beta PR is merged -> Done (any merged = Done).
        let pr_a = create_mock_pr(10, "PR alpha", "feature-x", "", "");
        let mut pr_b = create_mock_pr(20, "PR beta", "feature-x", "", "");
        pr_b.state = "MERGED".to_string();

        let (key_a, fetch_a) = create_mock_repo_data(rp_a, vec![], vec![pr_a], vec![]);
        let (key_b, fetch_b) = create_mock_repo_data(rp_b, vec![], vec![pr_b], vec![]);
        let repo_data = HashMap::from([(key_a, fetch_a), (key_b, fetch_b)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].status,
            WorkItemStatus::Done,
            "any merged PR should derive Done status",
        );
    }

    // -- Round 7 regression tests --

    /// F-1: Fork PRs with the same branch name as a local branch must not
    /// be matched. Only same-repo PRs (where head_repo_owner matches the
    /// repo owner) should match.
    #[test]
    fn fork_pr_not_matched_to_local_branch() {
        let rp = repo_path("alpha");
        let branch = "fix-typo";

        let record = create_mock_record(
            "wi-1",
            "Fix typo",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // Same-repo PR (owner matches).
        let same_repo_pr = create_mock_pr_with_owner(
            1,
            "Fix typo (same repo)",
            branch,
            "APPROVED",
            "SUCCESS",
            Some("owner"),
        );
        // Fork PR (different owner, same branch name).
        let fork_pr = create_mock_pr_with_owner(
            2,
            "Fix typo (fork)",
            branch,
            "",
            "PENDING",
            Some("contributor"),
        );

        let (rp_key, fetch) =
            create_mock_repo_data(rp.clone(), vec![], vec![same_repo_pr, fork_pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, unlinked, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let item = &items[0];

        // Only the same-repo PR should be matched.
        let pr_info = item.repo_associations[0]
            .pr
            .as_ref()
            .expect("should have PR info");
        assert_eq!(pr_info.number, 1, "should match same-repo PR, not fork PR");
        assert_eq!(pr_info.title, "Fix typo (same repo)");

        // No MultiplePrsForBranch error since the fork PR is filtered out.
        assert!(
            item.errors.is_empty(),
            "fork PR should be filtered out, no MultiplePrsForBranch: {:?}",
            item.errors,
        );

        // After F-1 fix (Round 10): fork PR whose (repo_path, branch) is
        // already claimed by a work item should NOT re-appear as unlinked.
        // The branch "fix-typo" is claimed by wi-1, so the fork PR is
        // excluded from the unlinked list.
        assert_eq!(
            unlinked.len(),
            0,
            "fork PR with a claimed branch should not appear as unlinked, got {} unlinked",
            unlinked.len(),
        );
    }

    /// F-1: When head_repo_owner is None (gh CLI did not return the field),
    /// PRs are still matched (backwards compatibility).
    #[test]
    fn pr_without_head_repo_owner_still_matches() {
        let rp = repo_path("alpha");
        let branch = "fix-typo";

        let record = create_mock_record(
            "wi-1",
            "Fix typo",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: None,
            }],
        );

        // PR without head_repo_owner (None).
        let pr = create_mock_pr(1, "Fix typo", branch, "", "");

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        assert!(
            items[0].repo_associations[0].pr.is_some(),
            "PR with no head_repo_owner should still match",
        );
    }

    // -- Round 8 regression tests --

    /// F-2 (Round 8) + F-1 fix (Round 10): Fork PRs appear in unlinked_prs
    /// only when their (repo_path, branch) is NOT claimed by a work item.
    /// Once imported, the fork PR's branch is claimed and it disappears
    /// from the unlinked list.
    #[test]
    fn fork_pr_appears_in_unlinked_prs() {
        let rp = repo_path("alpha");

        // Work item claims branch "fix-readme".
        let record = create_mock_record(
            "wi-1",
            "Fix readme",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some("fix-readme".to_string()),
                pr_identity: None,
            }],
        );

        // Same-repo PR on the claimed branch - should NOT be unlinked.
        let same_repo_pr = create_mock_pr_with_owner(
            1,
            "Fix readme (same repo)",
            "fix-readme",
            "APPROVED",
            "SUCCESS",
            Some("owner"),
        );
        // Fork PR on a DIFFERENT branch - should appear as unlinked.
        let fork_pr = create_mock_pr_with_owner(
            2,
            "Fix readme (fork)",
            "fork-fix-readme",
            "",
            "PENDING",
            Some("contributor"),
        );

        let (rp_key, fetch) =
            create_mock_repo_data(rp.clone(), vec![], vec![same_repo_pr, fork_pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, unlinked, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        // The work item should have the same-repo PR matched.
        assert_eq!(items.len(), 1);
        let pr_info = items[0].repo_associations[0]
            .pr
            .as_ref()
            .expect("should have same-repo PR matched");
        assert_eq!(pr_info.number, 1);

        // The fork PR on a different branch should appear as unlinked.
        assert_eq!(
            unlinked.len(),
            1,
            "fork PR on unclaimed branch should appear in unlinked list, got {}",
            unlinked.len(),
        );
        assert_eq!(unlinked[0].pr.number, 2);
        assert_eq!(unlinked[0].pr.title, "Fix readme (fork)");
        assert_eq!(unlinked[0].branch, "fork-fix-readme");
    }

    // -- Round 10 regression tests --

    // -- PR identity fallback tests --

    /// Persisted pr_identity produces a PrInfo when the backend record is
    /// Done and no live PR is found.
    #[test]
    fn pr_identity_fallback_produces_pr_info_for_done_item() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        let record = create_mock_record(
            "wi-1",
            "Shipped feature",
            WorkItemStatus::Done,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: Some(crate::work_item_backend::PrIdentityRecord {
                    number: 42,
                    title: "Ship it".into(),
                    url: "https://github.com/o/r/pull/42".into(),
                }),
            }],
        );

        // No live PRs in repo data.
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.status, WorkItemStatus::Done);
        assert!(
            item.status_derived,
            "status_derived should be true when pr_identity fallback injects PrState::Merged",
        );

        let assoc = &item.repo_associations[0];
        let pr = assoc
            .pr
            .as_ref()
            .expect("pr_identity fallback should produce PrInfo");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Ship it");
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.url, "https://github.com/o/r/pull/42");
    }

    /// A live PR takes precedence over persisted pr_identity.
    #[test]
    fn live_pr_takes_precedence_over_pr_identity() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        let record = create_mock_record(
            "wi-1",
            "Work",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: Some(crate::work_item_backend::PrIdentityRecord {
                    number: 42,
                    title: "Old merged PR".into(),
                    url: "https://github.com/o/r/pull/42".into(),
                }),
            }],
        );

        // A live PR exists on the same branch.
        let live_pr = create_mock_pr(99, "New PR on same branch", branch, "", "PENDING");

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![live_pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let assoc = &items[0].repo_associations[0];
        let pr = assoc.pr.as_ref().expect("should have live PR info");
        assert_eq!(
            pr.number, 99,
            "live PR should take precedence over pr_identity"
        );
        assert_eq!(pr.title, "New PR on same branch");
        assert_eq!(pr.state, PrState::Open);
    }

    /// Persisted pr_identity is ignored when the item is NOT Done.
    /// This prevents an irreversible Done lock when a merge is reverted
    /// and the user moves the item back.
    #[test]
    fn pr_identity_ignored_when_item_not_done() {
        let rp = repo_path("alpha");
        let branch = "feature-x";

        // Backend record is Implementing (user moved it back after merge revert).
        let record = create_mock_record(
            "wi-1",
            "Reverted merge",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(branch.to_string()),
                pr_identity: Some(crate::work_item_backend::PrIdentityRecord {
                    number: 42,
                    title: "Old merged PR".into(),
                    url: "https://github.com/o/r/pull/42".into(),
                }),
            }],
        );

        // No live PRs.
        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (items, _, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        assert_eq!(items.len(), 1);
        let item = &items[0];

        // pr_identity should NOT be used because record.status != Done.
        assert!(
            item.repo_associations[0].pr.is_none(),
            "pr_identity should be ignored for non-Done items, got: {:?}",
            item.repo_associations[0].pr,
        );

        // Status should remain Implementing, NOT be derived to Done.
        assert_eq!(
            item.status,
            WorkItemStatus::Implementing,
            "non-Done item with pr_identity should not be forced to Done",
        );
        assert!(
            !item.status_derived,
            "status_derived should be false for non-Done item with pr_identity",
        );
    }

    /// F-1 regression: After importing a fork PR, reassembling should NOT
    /// show it as unlinked again. The import creates a work item that
    /// claims the (repo_path, branch), so collect_unlinked_prs must
    /// respect that claim for fork PRs too.
    #[test]
    fn imported_fork_pr_not_re_listed_as_unlinked() {
        let rp = repo_path("alpha");
        let fork_branch = "fix-typo";

        // Simulate state AFTER importing the fork PR: a work item now
        // claims (repo_path, fork_branch).
        let record = create_mock_record(
            "wi-imported",
            "Fix typo (fork)",
            WorkItemStatus::Implementing,
            vec![RepoAssociationRecord {
                repo_path: rp.clone(),
                branch: Some(fork_branch.to_string()),
                pr_identity: None,
            }],
        );

        // The fork PR still exists in the fetched PR list.
        let fork_pr = create_mock_pr_with_owner(
            99,
            "Fix typo (fork)",
            fork_branch,
            "",
            "PENDING",
            Some("contributor"),
        );

        let (rp_key, fetch) = create_mock_repo_data(rp.clone(), vec![], vec![fork_pr], vec![]);
        let repo_data = HashMap::from([(rp_key, fetch)]);

        let (_items, unlinked, _, _) =
            reassemble(&[record], &repo_data, DEFAULT_ISSUE_PATTERN, ".worktrees");

        // The fork PR's branch is now claimed by the imported work item,
        // so it must NOT appear as unlinked.
        assert_eq!(
            unlinked.len(),
            0,
            "imported fork PR should not re-appear as unlinked, got {} unlinked: {:?}",
            unlinked.len(),
            unlinked.iter().map(|u| &u.pr.title).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn closed_prs_excluded_from_unlinked() {
        let mut closed_pr = create_mock_pr(10, "Closed PR", "closed-branch", "", "");
        closed_pr.state = "CLOSED".to_string();
        let mut merged_pr = create_mock_pr(11, "Merged PR", "merged-branch", "", "");
        merged_pr.state = "MERGED".to_string();
        let open_pr = create_mock_pr(12, "Open PR", "open-branch", "", "");

        let repo_path = PathBuf::from("/repo");
        let mut repo_data = HashMap::new();
        repo_data.insert(
            repo_path.clone(),
            create_mock_repo_data(
                repo_path,
                vec![],
                vec![closed_pr, merged_pr, open_pr],
                vec![],
            )
            .1,
        );

        let claimed = HashSet::new();
        let unlinked = collect_unlinked_prs(&repo_data, &claimed);

        assert_eq!(unlinked.len(), 1, "only the OPEN PR should be unlinked");
        assert_eq!(unlinked[0].pr.title, "Open PR");
    }
}
