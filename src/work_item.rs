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
    /// Monotonic counter bumped on every successful stage transition in
    /// the backend. Passed through from `WorkItemRecord` unchanged by
    /// the assembly layer and folded into the deterministic Claude Code
    /// session UUID derivation so that cycling back to a previously-
    /// used stage does not resume the prior transcript. See
    /// `src/session_id.rs` for the derivation rule and
    /// `docs/work-items.md` "Session identity and resumption" for the
    /// cross-stage isolation argument. Defaults to `0` for records that
    /// predate the field.
    pub stage_transition_count: u64,
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
    /// Read by assembly tests; will be shown in detail views.
    #[allow(dead_code)]
    pub git_state: Option<GitState>,
}

/// Local git state for a worktree.
/// Populated by assembly; fields read in tests. Will be shown in work
/// item detail views (e.g., "3 ahead, 1 behind, dirty").
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct GitState {
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    pub detached: bool,
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
    /// Path to the temp `--mcp-config` file workbridge wrote for
    /// this session under `std::env::temp_dir()`. `None` for test
    /// stubs and for sessions spawned without an MCP config.
    /// Tracked here so every teardown path (normal death, stage
    /// change, stale spawn result, work-item delete, cleanup-all)
    /// can unlink the file when the session is dropped, so the
    /// secret-bearing config does not linger in `/tmp` after
    /// Claude exits. Codex adversarial review flagged the prior
    /// leak as a secrets-on-disk risk even though the file is
    /// mode 0600 - an accumulation of stale MCP configs is
    /// unnecessary disk retention of credentials.
    pub mcp_config_path: Option<PathBuf>,
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
}

/// Messages sent from background fetcher threads to the main thread.
pub enum FetchMessage {
    RepoData(RepoFetchResult),
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
}
