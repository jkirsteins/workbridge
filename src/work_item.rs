use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::sync::atomic::AtomicBool;

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
    GithubProject {
        node_id: String,
    },
}

impl Hash for WorkItemId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Discriminant tag first so different variants with overlapping
        // field values do not collide.
        std::mem::discriminant(self).hash(state);
        match self {
            WorkItemId::LocalFile(path) => path.hash(state),
            WorkItemId::GithubIssue { owner, repo, number } => {
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

/// High-level status of a work item.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum WorkItemStatus {
    Todo,
    InProgress,
}

/// A fully assembled work item with backend data and derived metadata.
pub struct WorkItem {
    pub id: WorkItemId,
    /// Used by assembly tests and future backend-type indicator in UI.
    #[allow(dead_code)]
    pub backend_type: BackendType,
    pub title: String,
    pub status: WorkItemStatus,
    pub repo_associations: Vec<RepoAssociation>,
    pub errors: Vec<WorkItemError>,
}

/// A repo associated with a work item, with derived metadata filled in
/// by the assembly layer.
pub struct RepoAssociation {
    /// Read by assembly tests; will be shown in detail views.
    #[allow(dead_code)]
    pub repo_path: PathBuf,
    /// None = pre-planning state: no worktree, no PR matching.
    /// Read by assembly tests; will be shown in detail views.
    #[allow(dead_code)]
    pub branch: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub pr: Option<PrInfo>,
    /// Read by assembly tests; will be shown in detail views.
    #[allow(dead_code)]
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
    /// Shown in detail views when added; read in tests.
    #[allow(dead_code)]
    pub review_decision: ReviewDecision,
    pub checks: CheckStatus,
    /// Shown in detail views when added; read in tests.
    #[allow(dead_code)]
    pub url: String,
}

/// Derived issue information attached to a repo association.
/// Populated by assembly; fields read in tests. Title used for work item
/// title derivation. Other fields shown in detail views when added.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct IssueInfo {
    pub number: u64,
    pub title: String,
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

/// A session associated with a work item. Replaces the old Tab struct's
/// fields minus `name` (the title comes from the work item).
pub struct SessionEntry {
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub alive: bool,
    pub session: Option<Session>,
}

/// Data fetched per repo by a background thread. Sent through a channel
/// to the main thread for assembly.
pub struct RepoFetchResult {
    pub repo_path: PathBuf,
    /// Read by fetcher tests; retained for stale-data indicators.
    #[allow(dead_code)]
    pub github_remote: Option<(String, String)>,
    pub worktrees: Result<Vec<WorktreeInfo>, WorktreeError>,
    pub prs: Result<Vec<GithubPr>, GithubError>,
    pub issues: Vec<(u64, Result<GithubIssue, GithubError>)>,
}

/// Messages sent from background fetcher threads to the main thread.
pub enum FetchMessage {
    RepoData(RepoFetchResult),
    FetcherError {
        /// Available for per-repo error reporting in future UI enhancements.
        #[allow(dead_code)]
        repo_path: PathBuf,
        error: String,
    },
}

/// Handle to background fetcher threads. Holds join handles and a shared
/// stop flag for clean shutdown.
pub struct FetcherHandle {
    pub threads: Vec<JoinHandle<()>>,
    pub stop: Arc<AtomicBool>,
}
