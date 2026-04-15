use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::github_client::{GithubError, GithubIssue, GithubPr};
use crate::session::Session;
use crate::worktree_service::{WorktreeError, WorktreeInfo};

/// Backend-derived identity for a work item.
///
/// Each variant corresponds to a backend type. The id uniquely identifies
/// a work item across sessions and restarts. All variant fields are
/// hashable, so WorkItemId can be used as a HashMap key.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkItemId {
    /// Stored as a JSON file on the local filesystem.
    LocalFile(PathBuf),
    /// Backed by a GitHub issue.
    GithubIssue {
        owner: String,
        repo: String,
        number: u64,
    },
    /// Backed by a GitHub project item (tracked by node id).
    GithubProject { node_id: String },
}

impl Hash for WorkItemId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Discriminant tag first so different variants with overlapping
        // field values do not collide.
        std::mem::discriminant(self).hash(state);
        match self {
            WorkItemId::LocalFile(path) => path.hash(state),
            WorkItemId::GithubIssue {
                owner,
                repo,
                number,
            } => {
                owner.hash(state);
                repo.hash(state);
                number.hash(state);
            }
            WorkItemId::GithubProject { node_id } => node_id.hash(state),
        }
    }
}

/// Which backend anchors this work item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum BackendType {
    LocalFile,
    GithubIssue,
    GithubProject,
}

/// Distinguishes the user's own work items from review requests.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkItemKind {
    /// The user's own work (default for existing items).
    #[default]
    Own,
    /// A PR the user was requested to review.
    ReviewRequest,
}

/// Workflow stage of a work item.
///
/// Progresses: Backlog -> Planning -> Implementing -> Review -> Done.
/// Blocked is a sub-state of Implementing (Claude needs user input).
/// Done can also be derived by the assembly layer when any repo
/// association has a merged PR.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum WorkItemStatus {
    #[serde(alias = "Todo")]
    Backlog,
    Planning,
    #[serde(alias = "InProgress")]
    Implementing,
    Blocked,
    Review,
    Mergequeue,
    Done,
}

impl WorkItemStatus {
    /// The next stage in the workflow, or None if at the terminal stage.
    pub fn next_stage(&self) -> Option<WorkItemStatus> {
        match self {
            Self::Backlog => Some(Self::Planning),
            Self::Planning => Some(Self::Implementing),
            Self::Implementing => Some(Self::Review),
            Self::Blocked => Some(Self::Review),
            Self::Review => Some(Self::Done),
            Self::Mergequeue => Some(Self::Done),
            Self::Done => None,
        }
    }

    /// The previous stage in the workflow, or None if at the first stage.
    pub fn prev_stage(&self) -> Option<WorkItemStatus> {
        match self {
            Self::Backlog => None,
            Self::Planning => Some(Self::Backlog),
            Self::Implementing => Some(Self::Planning),
            Self::Blocked => Some(Self::Implementing),
            Self::Review => Some(Self::Implementing),
            Self::Mergequeue => Some(Self::Review),
            Self::Done => Some(Self::Review),
        }
    }

    /// Short badge text for display in the work item list.
    pub fn badge_text(&self) -> &'static str {
        match self {
            Self::Backlog => "[BL]",
            Self::Planning => "[PL]",
            Self::Implementing => "[IM]",
            Self::Blocked => "[BK]",
            Self::Review => "[RV]",
            Self::Mergequeue => "[MQ]",
            Self::Done => "[DN]",
        }
    }

    /// Workflow-order rank for sorting within a display group.
    ///
    /// Lower values sort first: Planning (0) -> Implementing (1) ->
    /// Review (2) -> Mergequeue (3). Other stages return a high value
    /// so they never displace PL/IM/RV/MQ inside the ACTIVE bucket.
    /// Ties are broken by the caller's existing order (stable sort).
    pub fn active_group_rank(&self) -> u8 {
        match self {
            Self::Planning => 0,
            Self::Implementing => 1,
            Self::Review => 2,
            Self::Mergequeue => 3,
            // Blocked, Backlog, Done don't appear in the ACTIVE bucket
            // but we give them a deterministic high rank for defensive
            // robustness if the caller ever mixes buckets.
            _ => u8::MAX,
        }
    }
}

/// Final path component of a repo path, used as the human-readable repo
/// slug in group headers (`"ACTIVE (workbridge)"`) and in backend-provided
/// display IDs (`"#workbridge-42"`).
///
/// This is the single source of truth for "what do we call this repo in the
/// UI": both the group-header rendering in `App::push_repo_groups` and the
/// ID allocation in `LocalFileBackend::create` route through here so the
/// two displayed forms cannot drift.
///
/// Deliberately NOT lowercased or sanitized - a repo named `My.Repo`
/// yields the slug `My.Repo`, matching what the group header already
/// shows. The `"unknown"` fallback only fires for a path that has no
/// final component (e.g. the filesystem root).
pub fn repo_slug_from_path(repo_path: &Path) -> String {
    repo_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into())
}

/// A fully assembled work item with backend data and derived metadata.
pub struct WorkItem {
    pub id: WorkItemId,
    pub backend_type: BackendType,
    pub kind: WorkItemKind,
    pub title: String,
    /// Backend-provided, human-readable stable identifier for the work
    /// item (e.g. `"workbridge-42"`). Passed through from the backend
    /// record unchanged - the assembly layer does not derive it. `None`
    /// for records created before the feature landed; those items
    /// render in the list without the ID subtitle line.
    pub display_id: Option<String>,
    pub description: Option<String>,
    pub status: WorkItemStatus,
    /// True when the assembly layer derived the status (e.g. merged PR -> Done)
    /// rather than using the backend record's status directly. Stage transitions
    /// are blocked for derived statuses to prevent backend/display divergence.
    pub status_derived: bool,
    pub repo_associations: Vec<RepoAssociation>,
    pub errors: Vec<WorkItemError>,
}

/// A repo associated with a work item, with derived metadata filled in
/// by the assembly layer.
pub struct RepoAssociation {
    pub repo_path: PathBuf,
    /// None = pre-planning state: no worktree, no PR matching.
    pub branch: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub pr: Option<PrInfo>,
    pub issue: Option<IssueInfo>,
    /// Derived local git state for this worktree. Read by the work
    /// item list renderer (`format_work_item_entry`) to decide whether
    /// to show the `!cl` unclean-worktree chip, and by tests. Will
    /// also feed future detail views (e.g., "3 ahead, 1 behind, dirty").
    pub git_state: Option<GitState>,
    /// Set when a detached-HEAD worktree exists at the expected target
    /// path for this branch (e.g. after an interrupted rebase). The
    /// assembly layer detects this proactively so the UI can show
    /// "Press Enter to recover worktree" instead of "start a session".
    pub stale_worktree_path: Option<PathBuf>,
}

/// Local git state for a worktree. Fields are cache-projections of the
/// background-fetcher `WorktreeInfo` values - reading them from the UI
/// thread is always a pure in-memory op and never shells out.
///
/// - `dirty`: union of uncommitted tracked-file changes AND untracked
///   files present in the worktree. The `!cl` chip treats both the
///   same way; callers that need to distinguish them (e.g. the
///   merge-guard alert wording) go through
///   `WorktreeCleanliness::from_worktree_info`, which reads the raw
///   `WorktreeInfo` fields directly.
/// - `ahead`: commits on the local branch that are not yet on its
///   upstream - i.e. unpushed work.
/// - `behind`: commits on the upstream that the local branch does not
///   have. Shown as a soft warning but does not block merges.
///
/// A `detached` flag was intentionally omitted: `assembly::reassemble`
/// only populates `GitState` when a worktree matched by branch name,
/// so detached-HEAD worktrees never produce a `GitState`, and there
/// is nowhere the field could be meaningfully true.
#[derive(Clone, Debug)]
pub struct GitState {
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
}

/// Aggregate CI check status for a PR.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    None,
    Passing,
    Pending,
    Failing,
    Unknown,
}

/// Whether GitHub reports the PR as mergeable against its base branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeableState {
    Unknown,
    Mergeable,
    Conflicting,
}

/// PR review decision.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReviewDecision {
    None,
    Pending,
    Approved,
    ChangesRequested,
}

/// PR open/closed/merged state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// Issue open/closed state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum IssueState {
    Open,
    Closed,
}

/// Derived PR information attached to a repo association.
#[derive(Clone, Debug)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub state: PrState,
    pub is_draft: bool,
    pub review_decision: ReviewDecision,
    pub checks: CheckStatus,
    pub mergeable: MergeableState,
    pub url: String,
}

/// Derived issue information attached to a repo association.
/// Populated by assembly; fields read in tests. Title used for work item
/// title derivation. Other fields shown in detail views when added.
#[derive(Clone, Debug)]
pub struct IssueInfo {
    pub number: u64,
    pub title: String,
    /// Shown in detail views when issue state display is added.
    #[allow(dead_code)]
    pub state: IssueState,
    pub labels: Vec<String>,
}

/// Errors that can occur on a work item. These are orthogonal to status
/// (a Todo item can have errors). Each error is surfaced as a badge or
/// detail in the UI.
#[derive(Clone, Debug)]
pub enum WorkItemError {
    MultiplePrsForBranch {
        repo_path: PathBuf,
        branch: String,
        count: usize,
    },
    /// Kept for display completeness but no longer produced by the assembly
    /// layer. A detached worktree has no branch, so it cannot be matched to
    /// a work item.
    #[allow(dead_code)]
    DetachedHead {
        repo_path: PathBuf,
        worktree_path: PathBuf,
    },
    IssueNotFound {
        repo_path: PathBuf,
        issue_number: u64,
    },
    /// Constructed when backend.list() encounters a parseable but invalid
    /// record. Not triggered in v1 (LocalFileBackend skips corrupt files).
    #[allow(dead_code)]
    CorruptBackendRecord {
        backend: BackendType,
        reason: String,
    },
    /// Constructed when a work item references a worktree path that no
    /// longer exists on disk. Detection deferred to a future assembly pass.
    #[allow(dead_code)]
    WorktreeGone {
        repo_path: PathBuf,
        expected_path: PathBuf,
    },
}

/// A GitHub PR that does not match any work item's repo associations.
/// Shown in the "Unlinked" group and can be imported.
pub struct UnlinkedPr {
    pub repo_path: PathBuf,
    pub pr: PrInfo,
    pub branch: String,
}

/// A GitHub PR where the authenticated user has been requested as a reviewer.
/// Shown in the "Review Requests" group and can be imported as a work item.
pub struct ReviewRequestedPr {
    pub repo_path: PathBuf,
    pub pr: PrInfo,
    pub branch: String,
    /// Logins of individual users explicitly requested for review on this
    /// PR. Used to classify the row as direct-to-you (the current user's
    /// login appears here) vs. team-only. Distinct from `PrInfo` so
    /// unlinked-PR rows do not carry review-specific state.
    pub requested_reviewer_logins: Vec<String>,
    /// Slugs of teams explicitly requested for review on this PR. Used
    /// to build the compact reviewer badge and the authoritative
    /// "Requested from:" detail-panel line.
    pub requested_team_slugs: Vec<String>,
}

impl ReviewRequestedPr {
    /// Classify whether the current user was directly requested as a
    /// reviewer on this PR. Returns false when the login is unknown
    /// (e.g. `gh api user` has not yet succeeded) so the row stays in
    /// the team bucket - a safe default that never falsely promotes a
    /// row to "actionable by you".
    pub fn is_direct_request(&self, current_user_login: Option<&str>) -> bool {
        match current_user_login {
            Some(login) if !login.is_empty() => {
                self.requested_reviewer_logins.iter().any(|r| r == login)
            }
            _ => false,
        }
    }

    /// Build the compact reviewer badge shown at the right edge of the
    /// row in the REVIEW REQUESTS block. Returns None when both reviewer
    /// lists are empty (degenerate data - gh returned a row with no
    /// attached reviewer identity at all) so the caller can skip the
    /// badge slot entirely. The rules:
    ///
    /// - direct request (login in `requested_reviewer_logins`) -> `[you]`
    /// - single team -> `[team-slug]`
    /// - multi team -> `[first-slug +N]` where N = `len() - 1`
    ///
    /// The "direct wins" policy means a PR requesting both the user and
    /// a team still renders as `[you]`; the full identity list is
    /// preserved in the detail panel.
    pub fn reviewer_badge(&self, current_user_login: Option<&str>) -> Option<String> {
        if self.is_direct_request(current_user_login) {
            return Some("[you]".to_string());
        }
        match self.requested_team_slugs.len() {
            0 => None,
            1 => Some(format!("[{}]", self.requested_team_slugs[0])),
            n => Some(format!("[{} +{}]", self.requested_team_slugs[0], n - 1)),
        }
    }
}

/// Mouse text selection state for a terminal session.
pub struct SelectionState {
    /// The anchor point where the selection started (row, col in terminal cell coordinates).
    pub anchor: (u16, u16),
    /// The current/end point of the selection (row, col).
    pub current: (u16, u16),
    /// Whether the user is actively dragging (mouse button held down).
    pub dragging: bool,
}

/// A session associated with a work item. Replaces the old Tab struct's
/// fields minus `name` (the title comes from the work item).
pub struct SessionEntry {
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub alive: bool,
    pub session: Option<Session>,
    /// How many lines into the scrollback history the user has scrolled.
    /// 0 means live view (no scrollback). Positive values shift the
    /// viewport into the past.
    pub scrollback_offset: usize,
    /// Active mouse text selection, if any.
    pub selection: Option<SelectionState>,
    /// Side-car files the agent backend wrote before spawn (e.g. Claude's
    /// worktree `.mcp.json`). Tracked here so the caller can hand them
    /// back to `AgentBackend::cleanup_session_files` when the session
    /// dies or the work item is deleted. See `docs/harness-contract.md`
    /// C4 and the `AgentBackend::write_session_files` doc.
    pub agent_written_files: Vec<std::path::PathBuf>,
}

/// Data fetched per repo by a background thread. Sent through a channel
/// to the main thread for assembly.
pub struct RepoFetchResult {
    pub repo_path: PathBuf,
    /// `(owner, repo)` pair parsed from the `origin` remote URL. Populated
    /// by the background fetcher so UI-thread code that needs the remote
    /// (`spawn_pr_creation`, `execute_merge`, `enter_mergequeue`,
    /// `spawn_review_submission`, `collect_backfill_requests`,
    /// `reconstruct_mergequeue_watches`) can read from the cache instead of
    /// calling `WorktreeService::github_remote` (which shells out to
    /// `git remote get-url`). Also used by fetcher tests to assert the
    /// populated value. `None` means the repo has no GitHub remote (or
    /// the fetcher has not finished its first cycle yet).
    pub github_remote: Option<(String, String)>,
    pub worktrees: Result<Vec<WorktreeInfo>, WorktreeError>,
    pub prs: Result<Vec<GithubPr>, GithubError>,
    /// PRs where the authenticated user has been requested as a reviewer.
    pub review_requested_prs: Result<Vec<GithubPr>, GithubError>,
    pub issues: Vec<(u64, Result<GithubIssue, GithubError>)>,
    /// The GitHub login of the currently authenticated user, resolved
    /// once per fetch tick via `gh api user` (cached inside the
    /// github_client). None when the lookup has not yet succeeded -
    /// e.g. the gh CLI is missing, auth is expired, or the first call
    /// happens to hit a transient error. Lookup failures are NOT
    /// silent: the fetcher emits a `FetchMessage::FetcherError` on
    /// the same channel so the status bar surfaces the problem. The
    /// UI reads this to classify review-request rows as direct-to-you
    /// vs. team-only; a None value degrades gracefully by classifying
    /// every row as team.
    pub current_user_login: Option<String>,
}

/// Messages sent from background fetcher threads to the main thread.
pub enum FetchMessage {
    RepoData(Box<RepoFetchResult>),
    FetcherError {
        repo_path: PathBuf,
        error: String,
    },
    /// Sent at the start of each fetch cycle so the UI can show a spinner.
    FetchStarted,
}

/// Handle to background fetcher threads. Holds a shared stop flag for
/// clean shutdown. Threads are fully independent once spawned - we do
/// not store JoinHandles or join on stop. Threads exit on their own
/// when the stop flag is set or when their channel send fails.
pub struct FetcherHandle {
    pub stop: Arc<AtomicBool>,
}

impl Drop for FetcherHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_stage_progression() {
        assert_eq!(
            WorkItemStatus::Backlog.next_stage(),
            Some(WorkItemStatus::Planning)
        );
        assert_eq!(
            WorkItemStatus::Planning.next_stage(),
            Some(WorkItemStatus::Implementing)
        );
        assert_eq!(
            WorkItemStatus::Implementing.next_stage(),
            Some(WorkItemStatus::Review)
        );
        assert_eq!(
            WorkItemStatus::Blocked.next_stage(),
            Some(WorkItemStatus::Review)
        );
        assert_eq!(
            WorkItemStatus::Review.next_stage(),
            Some(WorkItemStatus::Done)
        );
        assert_eq!(WorkItemStatus::Done.next_stage(), None);
    }

    #[test]
    fn prev_stage_regression() {
        assert_eq!(WorkItemStatus::Backlog.prev_stage(), None);
        assert_eq!(
            WorkItemStatus::Planning.prev_stage(),
            Some(WorkItemStatus::Backlog)
        );
        assert_eq!(
            WorkItemStatus::Implementing.prev_stage(),
            Some(WorkItemStatus::Planning)
        );
        assert_eq!(
            WorkItemStatus::Blocked.prev_stage(),
            Some(WorkItemStatus::Implementing)
        );
        assert_eq!(
            WorkItemStatus::Review.prev_stage(),
            Some(WorkItemStatus::Implementing)
        );
        assert_eq!(
            WorkItemStatus::Done.prev_stage(),
            Some(WorkItemStatus::Review)
        );
    }

    #[test]
    fn badge_text_format() {
        assert_eq!(WorkItemStatus::Backlog.badge_text(), "[BL]");
        assert_eq!(WorkItemStatus::Planning.badge_text(), "[PL]");
        assert_eq!(WorkItemStatus::Implementing.badge_text(), "[IM]");
        assert_eq!(WorkItemStatus::Blocked.badge_text(), "[BK]");
        assert_eq!(WorkItemStatus::Review.badge_text(), "[RV]");
        assert_eq!(WorkItemStatus::Done.badge_text(), "[DN]");
    }

    // ---- ReviewRequestedPr reviewer-identity helpers ----

    fn make_rr(reviewers: &[&str], teams: &[&str]) -> ReviewRequestedPr {
        ReviewRequestedPr {
            repo_path: PathBuf::from("/repo"),
            pr: PrInfo {
                number: 1,
                title: "Example PR".into(),
                state: PrState::Open,
                is_draft: false,
                review_decision: ReviewDecision::Pending,
                checks: CheckStatus::None,
                mergeable: MergeableState::Unknown,
                url: "https://example.com/pr/1".into(),
            },
            branch: "feature".into(),
            requested_reviewer_logins: reviewers.iter().map(|s| (*s).to_string()).collect(),
            requested_team_slugs: teams.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn is_direct_request_matches_current_user_login() {
        let rr = make_rr(&["alice", "bob"], &[]);
        assert!(rr.is_direct_request(Some("alice")));
        assert!(rr.is_direct_request(Some("bob")));
        assert!(!rr.is_direct_request(Some("carol")));
    }

    #[test]
    fn is_direct_request_false_when_login_unknown() {
        let rr = make_rr(&["alice"], &[]);
        // Unknown login (first tick not yet arrived, or auth expired)
        // must classify as team so no row is falsely promoted to "you".
        assert!(!rr.is_direct_request(None));
        // Empty-string login is treated the same as None.
        assert!(!rr.is_direct_request(Some("")));
    }

    #[test]
    fn reviewer_badge_you_wins_over_team() {
        let rr = make_rr(&["alice"], &["core-team"]);
        assert_eq!(rr.reviewer_badge(Some("alice")), Some("[you]".to_string()),);
    }

    #[test]
    fn reviewer_badge_single_team() {
        let rr = make_rr(&[], &["core-team"]);
        assert_eq!(
            rr.reviewer_badge(Some("alice")),
            Some("[core-team]".to_string()),
        );
    }

    #[test]
    fn reviewer_badge_multi_team() {
        let rr = make_rr(&[], &["core-team", "backend", "frontend"]);
        assert_eq!(
            rr.reviewer_badge(Some("alice")),
            Some("[core-team +2]".to_string()),
        );
    }

    #[test]
    fn reviewer_badge_none_when_no_reviewers() {
        let rr = make_rr(&[], &[]);
        assert_eq!(rr.reviewer_badge(Some("alice")), None);
    }

    #[test]
    fn reviewer_badge_team_bucket_when_login_unknown() {
        // Login unknown + direct-user request in data -> falls back to
        // team bucket (no team -> None; single team -> that team).
        let rr_direct_only = make_rr(&["alice"], &[]);
        assert_eq!(rr_direct_only.reviewer_badge(None), None);

        let rr_mixed = make_rr(&["alice"], &["core-team"]);
        assert_eq!(
            rr_mixed.reviewer_badge(None),
            Some("[core-team]".to_string()),
        );
    }

    #[test]
    fn reviewer_badge_long_team_name_not_truncated() {
        let rr = make_rr(&[], &["super-long-team-name-that-stays-intact"]);
        assert_eq!(
            rr.reviewer_badge(Some("alice")),
            Some("[super-long-team-name-that-stays-intact]".to_string()),
        );
    }
}
