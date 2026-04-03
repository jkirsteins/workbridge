use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use regex::Regex;

use crate::github_client::GithubClient;
use crate::work_item::{FetchMessage, FetcherHandle, RepoFetchResult};
use crate::worktree_service::WorktreeService;

/// Start background fetcher threads for the given repos.
///
/// Spawns one thread per repo that periodically fetches worktree,
/// PR, and issue data and sends results through a channel. Returns
/// the receiver end of the channel and a handle for stopping the
/// threads cleanly.
///
/// `extra_branches` contains additional branch names per repo (e.g.
/// from backend records) whose issue numbers should also be fetched,
/// even if no worktree exists for them.
#[cfg(test)]
pub fn start(
    repos: Vec<PathBuf>,
    worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    github_client: Arc<dyn GithubClient + Send + Sync>,
    issue_pattern: String,
) -> (mpsc::Receiver<FetchMessage>, FetcherHandle) {
    start_with_extra_branches(
        repos,
        worktree_service,
        github_client,
        issue_pattern,
        HashMap::new(),
    )
}

/// Like `start`, but accepts extra branch names per repo path. These
/// branches are included in issue extraction alongside worktree branches.
pub fn start_with_extra_branches(
    repos: Vec<PathBuf>,
    worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    github_client: Arc<dyn GithubClient + Send + Sync>,
    issue_pattern: String,
    extra_branches: HashMap<PathBuf, Vec<String>>,
) -> (mpsc::Receiver<FetchMessage>, FetcherHandle) {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));

    for repo_path in repos {
        let tx = tx.clone();
        let stop = Arc::clone(&stop);
        let ws = Arc::clone(&worktree_service);
        let gc = Arc::clone(&github_client);
        let pattern = issue_pattern.clone();
        let extras = extra_branches.get(&repo_path).cloned().unwrap_or_default();

        // Threads are fully independent - we don't store JoinHandles.
        // They exit on their own when the stop flag is set or when the
        // channel receiver is dropped (send returns Err).
        thread::spawn(move || {
            fetcher_loop(repo_path, tx, stop, ws, gc, &pattern, extras);
        });
    }

    (rx, FetcherHandle { stop })
}

/// Main loop for a single repo fetcher thread.
///
/// Each iteration:
/// 1. Lists worktrees via the worktree service
/// 2. Determines the GitHub remote (owner/repo) for the repo
/// 3. If a GitHub remote exists, fetches open PRs
/// 4. Extracts issue numbers from worktree branch names AND extra
///    branches (from backend records) and fetches each
/// 5. Sends the result through the channel
/// 6. Sleeps for 120 seconds in 1-second increments, checking the stop flag
fn fetcher_loop(
    repo_path: PathBuf,
    tx: mpsc::Sender<FetchMessage>,
    stop: Arc<AtomicBool>,
    worktree_service: Arc<dyn WorktreeService + Send + Sync>,
    github_client: Arc<dyn GithubClient + Send + Sync>,
    issue_pattern: &str,
    extra_branches: Vec<String>,
) {
    let re = match Regex::new(issue_pattern) {
        Ok(r) => r,
        Err(e) => {
            let msg = FetchMessage::FetcherError {
                repo_path: repo_path.clone(),
                error: format!("invalid issue pattern '{}': {}", issue_pattern, e),
            };
            // If the receiver is already gone, just return.
            let _ = tx.send(msg);
            return;
        }
    };

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Step 1: list worktrees
        let worktrees = worktree_service.list_worktrees(&repo_path);

        // Step 2: determine GitHub remote
        let github_remote = match worktree_service.github_remote(&repo_path) {
            Ok(remote) => remote,
            Err(e) => {
                let msg = FetchMessage::FetcherError {
                    repo_path: repo_path.clone(),
                    error: format!("failed to determine GitHub remote: {}", e),
                };
                if tx.send(msg).is_err() {
                    break;
                }
                // Sleep and retry next iteration
                if !interruptible_sleep(&stop, 120) {
                    break;
                }
                continue;
            }
        };

        // Step 3: fetch open PRs if we have a GitHub remote
        let prs = match &github_remote {
            Some((owner, repo)) => github_client.list_open_prs(owner, repo),
            None => Ok(Vec::new()),
        };

        // Step 4: extract issue numbers from worktree branch names AND
        // extra branches (backend records without worktrees) and fetch each
        let mut issues = Vec::new();
        if let Some((owner, repo)) = &github_remote {
            let mut seen = HashSet::new();

            // Collect branches from worktrees.
            if let Ok(wts) = &worktrees {
                for wt in wts {
                    if let Some(ref branch) = wt.branch {
                        for cap in re.captures_iter(branch) {
                            if let Some(m) = cap.get(1)
                                && let Ok(num) = m.as_str().parse::<u64>()
                                && seen.insert(num)
                            {
                                let result = github_client.get_issue(owner, repo, num);
                                issues.push((num, result));
                            }
                        }
                    }
                }
            }

            // Also extract issue numbers from extra branches (backend
            // records that have a branch but no worktree).
            for branch in &extra_branches {
                for cap in re.captures_iter(branch) {
                    if let Some(m) = cap.get(1)
                        && let Ok(num) = m.as_str().parse::<u64>()
                        && seen.insert(num)
                    {
                        let result = github_client.get_issue(owner, repo, num);
                        issues.push((num, result));
                    }
                }
            }
        }

        // Step 5: send the result
        let result = RepoFetchResult {
            repo_path: repo_path.clone(),
            github_remote,
            worktrees,
            prs,
            issues,
        };

        if tx.send(FetchMessage::RepoData(result)).is_err() {
            // Receiver dropped - main thread no longer listening
            break;
        }

        // Step 6: sleep 120s in 1s increments, checking stop flag
        if !interruptible_sleep(&stop, 120) {
            break;
        }
    }
}

/// Sleep for `seconds` in 1-second increments, checking the stop flag each
/// time. Returns true if the full sleep completed, false if the stop flag
/// was set (meaning the caller should exit).
fn interruptible_sleep(stop: &AtomicBool, seconds: u64) -> bool {
    for _ in 0..seconds {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_secs(1));
    }
    true
}

impl FetcherHandle {
    /// Signal all fetcher threads to stop. Does NOT join threads - they
    /// will exit on their own when they check the stop flag (every 1s
    /// during sleep) or when their channel send fails (receiver dropped).
    /// Consumes self to prevent reuse after stopping.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        // Drop impl also sets the flag, but setting it here explicitly
        // makes the intent clear and is a no-op for Drop.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::github_client::{GithubIssue, GithubPr, MockGithubClient};
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    /// Mock worktree service for fetcher tests.
    struct MockWorktreeService {
        worktrees: Vec<WorktreeInfo>,
        github_remote: Option<(String, String)>,
    }

    impl MockWorktreeService {
        fn new() -> Self {
            Self {
                worktrees: Vec::new(),
                github_remote: None,
            }
        }
    }

    impl WorktreeService for MockWorktreeService {
        fn list_worktrees(&self, _repo_path: &Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            Ok(self.worktrees.clone())
        }

        fn create_worktree(
            &self,
            _repo_path: &Path,
            _branch: &str,
            _target_dir: &Path,
        ) -> Result<WorktreeInfo, WorktreeError> {
            Err(WorktreeError::GitError(
                "create_worktree not implemented in mock".to_string(),
            ))
        }

        fn remove_worktree(
            &self,
            _repo_path: &Path,
            _worktree_path: &Path,
            _delete_branch: bool,
        ) -> Result<(), WorktreeError> {
            Err(WorktreeError::GitError(
                "remove_worktree not implemented in mock".to_string(),
            ))
        }

        fn default_branch(&self, _repo_path: &Path) -> Result<String, WorktreeError> {
            Ok("main".to_string())
        }

        fn github_remote(
            &self,
            _repo_path: &Path,
        ) -> Result<Option<(String, String)>, WorktreeError> {
            Ok(self.github_remote.clone())
        }

        fn fetch_branch(&self, _repo_path: &Path, _branch: &str) -> Result<(), WorktreeError> {
            Ok(())
        }
    }

    #[test]
    fn fetcher_sends_results() {
        let ws = Arc::new(MockWorktreeService {
            worktrees: vec![WorktreeInfo {
                path: PathBuf::from("/tmp/wt-feature"),
                branch: Some("42-fix-bug".to_string()),
                is_main: false,
            }],
            github_remote: Some(("owner".to_string(), "repo".to_string())),
        });

        let gc = Arc::new(MockGithubClient {
            prs: vec![GithubPr {
                number: 10,
                title: "A PR".into(),
                state: "OPEN".into(),
                is_draft: false,
                head_branch: "42-fix-bug".into(),
                url: "https://github.com/owner/repo/pull/10".into(),
                review_decision: String::new(),
                status_check_rollup: String::new(),
                head_repo_owner: None,
            }],
            issues: vec![GithubIssue {
                number: 42,
                title: "Fix the bug".into(),
                state: "OPEN".into(),
                labels: vec!["bug".into()],
            }],
            error: None,
        });

        let (rx, handle) = start(
            vec![PathBuf::from("/tmp/test-repo")],
            ws,
            gc,
            r"^(\d+)-".to_string(),
        );

        // Wait for a message (with timeout to avoid hanging)
        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("should receive a FetchMessage within 5 seconds");

        match msg {
            FetchMessage::RepoData(result) => {
                assert_eq!(result.repo_path, PathBuf::from("/tmp/test-repo"));
                assert_eq!(
                    result.github_remote,
                    Some(("owner".to_string(), "repo".to_string())),
                );

                let worktrees = result.worktrees.expect("worktrees should be Ok");
                assert_eq!(worktrees.len(), 1);
                assert_eq!(worktrees[0].branch, Some("42-fix-bug".to_string()),);

                let prs = result.prs.expect("prs should be Ok");
                assert_eq!(prs.len(), 1);
                assert_eq!(prs[0].number, 10);

                assert_eq!(result.issues.len(), 1);
                assert_eq!(result.issues[0].0, 42);
                assert!(result.issues[0].1.is_ok());
                assert_eq!(result.issues[0].1.as_ref().unwrap().title, "Fix the bug",);
            }
            FetchMessage::FetcherError { error, .. } => {
                panic!("unexpected FetcherError: {error}");
            }
        }

        handle.stop();
    }

    #[test]
    fn fetcher_stops_cleanly() {
        let ws = Arc::new(MockWorktreeService::new());
        let gc = Arc::new(MockGithubClient::new());

        let (_rx, handle) = start(
            vec![PathBuf::from("/tmp/test-repo")],
            ws,
            gc,
            r"^(\d+)-".to_string(),
        );

        // Immediately stop - threads should join without hanging.
        handle.stop();
    }

    #[test]
    fn fetcher_handles_no_github_remote() {
        let ws = Arc::new(MockWorktreeService {
            worktrees: vec![WorktreeInfo {
                path: PathBuf::from("/tmp/wt-local"),
                branch: Some("local-branch".to_string()),
                is_main: false,
            }],
            github_remote: None,
        });

        let gc = Arc::new(MockGithubClient::new());

        let (rx, handle) = start(
            vec![PathBuf::from("/tmp/no-github-repo")],
            ws,
            gc,
            r"^(\d+)-".to_string(),
        );

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("should receive a FetchMessage within 5 seconds");

        match msg {
            FetchMessage::RepoData(result) => {
                assert_eq!(result.repo_path, PathBuf::from("/tmp/no-github-repo"),);
                assert_eq!(result.github_remote, None);

                let prs = result.prs.expect("prs should be Ok");
                assert!(prs.is_empty(), "prs should be empty without GitHub remote");

                assert!(
                    result.issues.is_empty(),
                    "issues should be empty without GitHub remote",
                );
            }
            FetchMessage::FetcherError { error, .. } => {
                panic!("unexpected FetcherError: {error}");
            }
        }

        handle.stop();
    }

    #[test]
    fn fetcher_extracts_issues_from_extra_branches() {
        // F-4 regression: backend-recorded branches without worktrees should
        // still get their issue numbers extracted and fetched.
        let ws = Arc::new(MockWorktreeService {
            worktrees: vec![], // no worktrees at all
            github_remote: Some(("owner".to_string(), "repo".to_string())),
        });

        let gc = Arc::new(MockGithubClient {
            prs: vec![],
            issues: vec![GithubIssue {
                number: 55,
                title: "Backend-only issue".into(),
                state: "OPEN".into(),
                labels: vec![],
            }],
            error: None,
        });

        let repo_path = PathBuf::from("/tmp/test-extra-branches");
        let mut extra = std::collections::HashMap::new();
        extra.insert(repo_path.clone(), vec!["55-fix-thing".to_string()]);

        let (rx, handle) = start_with_extra_branches(
            vec![repo_path.clone()],
            ws,
            gc,
            r"^(\d+)-".to_string(),
            extra,
        );

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("should receive a FetchMessage within 5 seconds");

        match msg {
            FetchMessage::RepoData(result) => {
                assert_eq!(result.repo_path, repo_path);
                // Issue 55 should have been fetched from the extra branch,
                // even though there is no worktree for it.
                assert_eq!(
                    result.issues.len(),
                    1,
                    "should have fetched issue from extra branch"
                );
                assert_eq!(result.issues[0].0, 55);
                assert!(result.issues[0].1.is_ok());
                assert_eq!(
                    result.issues[0].1.as_ref().unwrap().title,
                    "Backend-only issue"
                );
            }
            FetchMessage::FetcherError { error, .. } => {
                panic!("unexpected FetcherError: {error}");
            }
        }

        handle.stop();
    }
}
