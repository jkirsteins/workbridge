use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use crate::github_client::GithubClient;
use crate::work_item::{FetchMessage, FetcherHandle};
use crate::worktree_service::WorktreeService;

mod loop_impl;

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
    worktree_service: &Arc<dyn WorktreeService + Send + Sync>,
    github_client: &Arc<dyn GithubClient + Send + Sync>,
    issue_pattern: &str,
) -> (mpsc::Receiver<FetchMessage>, FetcherHandle) {
    start_with_extra_branches(
        repos,
        worktree_service,
        github_client,
        issue_pattern,
        &HashMap::new(),
    )
}

/// Like `start`, but accepts extra branch names per repo path. These
/// branches are included in issue extraction alongside worktree branches.
pub fn start_with_extra_branches(
    repos: Vec<PathBuf>,
    worktree_service: &Arc<dyn WorktreeService + Send + Sync>,
    github_client: &Arc<dyn GithubClient + Send + Sync>,
    issue_pattern: &str,
    extra_branches: &HashMap<PathBuf, Vec<String>>,
) -> (mpsc::Receiver<FetchMessage>, FetcherHandle) {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));

    for repo_path in repos {
        let tx = tx.clone();
        let stop = Arc::clone(&stop);
        let ws = Arc::clone(worktree_service);
        let gc = Arc::clone(github_client);
        let pattern = issue_pattern.to_string();
        let extras = extra_branches.get(&repo_path).cloned().unwrap_or_default();

        // Threads are fully independent - we don't store JoinHandles.
        // They exit on their own when the stop flag is set or when the
        // channel receiver is dropped (send returns Err).
        std::thread::spawn(move || {
            loop_impl::fetcher_loop(repo_path, &tx, &stop, &ws, &gc, &pattern, &extras);
        });
    }

    (rx, FetcherHandle { stop })
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
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::{FetchMessage, start, start_with_extra_branches};
    use crate::github_client::{GithubIssue, GithubPr, MockGithubClient};
    use crate::worktree_service::{WorktreeError, WorktreeInfo, WorktreeService};

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
            _force: bool,
        ) -> Result<(), WorktreeError> {
            Err(WorktreeError::GitError(
                "remove_worktree not implemented in mock".to_string(),
            ))
        }

        fn delete_branch(
            &self,
            _repo_path: &Path,
            _branch: &str,
            _force: bool,
        ) -> Result<(), WorktreeError> {
            Err(WorktreeError::GitError(
                "delete_branch not implemented in mock".to_string(),
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

        fn create_branch(&self, _repo_path: &Path, _branch: &str) -> Result<(), WorktreeError> {
            Ok(())
        }

        fn prune_worktrees(&self, _repo_path: &Path) -> Result<(), WorktreeError> {
            Ok(())
        }
    }

    /// Helper: receive the next non-FetchStarted message from the channel,
    /// asserting that any preceding messages are `FetchStarted`.
    fn recv_data(rx: &mpsc::Receiver<FetchMessage>) -> FetchMessage {
        loop {
            let msg = crate::side_effects::clock::bounded_recv(rx, "fetcher recv_data");
            if !matches!(msg, FetchMessage::FetchStarted) {
                return msg;
            }
        }
    }

    #[test]
    fn fetcher_sends_fetch_started_before_data() {
        let ws: Arc<dyn WorktreeService + Send + Sync> = Arc::new(MockWorktreeService {
            worktrees: vec![],
            github_remote: Some(("owner".to_string(), "repo".to_string())),
        });
        let gc: Arc<dyn crate::github_client::GithubClient + Send + Sync> = Arc::new(MockGithubClient::new());

        let (rx, handle) = start(vec![PathBuf::from("/tmp/test-repo")], &ws, &gc, r"^(\d+)-");

        // First message must be FetchStarted.
        let first = crate::side_effects::clock::bounded_recv(&rx, "fetcher first-message waiter");
        assert!(
            matches!(first, FetchMessage::FetchStarted),
            "first message should be FetchStarted, got RepoData/FetcherError",
        );

        // Second message should be RepoData.
        let second = crate::side_effects::clock::bounded_recv(&rx, "fetcher second-message waiter");
        assert!(
            matches!(second, FetchMessage::RepoData(_)),
            "second message should be RepoData",
        );

        handle.stop();
    }

    #[test]
    fn fetcher_sends_results() {
        let ws: Arc<dyn WorktreeService + Send + Sync> = Arc::new(MockWorktreeService {
            worktrees: vec![WorktreeInfo {
                path: PathBuf::from("/tmp/wt-feature"),
                branch: Some("42-fix-bug".to_string()),
                is_main: false,
                ..WorktreeInfo::default()
            }],
            github_remote: Some(("owner".to_string(), "repo".to_string())),
        });

        let gc: Arc<dyn crate::github_client::GithubClient + Send + Sync> = Arc::new(MockGithubClient {
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
                author: None,
                mergeable: String::new(),
                requested_reviewer_logins: Vec::new(),
                requested_team_slugs: Vec::new(),
            }],
            issues: vec![GithubIssue {
                number: 42,
                title: "Fix the bug".into(),
                state: "OPEN".into(),
                labels: vec!["bug".into()],
            }],
            review_requested_prs: vec![],

            error: None,
            live_pr_state: None,
        });

        let (rx, handle) = start(vec![PathBuf::from("/tmp/test-repo")], &ws, &gc, r"^(\d+)-");

        let msg = recv_data(&rx);

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
            FetchMessage::FetchStarted => {
                panic!("unexpected FetchStarted after recv_data");
            }
        }

        handle.stop();
    }

    #[test]
    fn fetcher_stops_cleanly() {
        let ws: Arc<dyn WorktreeService + Send + Sync> = Arc::new(MockWorktreeService::new());
        let gc: Arc<dyn crate::github_client::GithubClient + Send + Sync> = Arc::new(MockGithubClient::new());

        let (_rx, handle) = start(vec![PathBuf::from("/tmp/test-repo")], &ws, &gc, r"^(\d+)-");

        // Immediately stop - threads should join without hanging.
        handle.stop();
    }

    #[test]
    fn fetcher_handles_no_github_remote() {
        let ws: Arc<dyn WorktreeService + Send + Sync> = Arc::new(MockWorktreeService {
            worktrees: vec![WorktreeInfo {
                path: PathBuf::from("/tmp/wt-local"),
                branch: Some("local-branch".to_string()),
                is_main: false,
                ..WorktreeInfo::default()
            }],
            github_remote: None,
        });

        let gc: Arc<dyn crate::github_client::GithubClient + Send + Sync> = Arc::new(MockGithubClient::new());

        let (rx, handle) = start(
            vec![PathBuf::from("/tmp/no-github-repo")],
            &ws,
            &gc,
            r"^(\d+)-",
        );

        let msg = recv_data(&rx);

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
            FetchMessage::FetchStarted => {
                panic!("unexpected FetchStarted after recv_data");
            }
        }

        handle.stop();
    }

    #[test]
    fn fetcher_extracts_issues_from_extra_branches() {
        // F-4 regression: backend-recorded branches without worktrees should
        // still get their issue numbers extracted and fetched.
        let ws: Arc<dyn WorktreeService + Send + Sync> = Arc::new(MockWorktreeService {
            worktrees: vec![], // no worktrees at all
            github_remote: Some(("owner".to_string(), "repo".to_string())),
        });

        let gc: Arc<dyn crate::github_client::GithubClient + Send + Sync> = Arc::new(MockGithubClient {
            prs: vec![],
            review_requested_prs: vec![],

            issues: vec![GithubIssue {
                number: 55,
                title: "Backend-only issue".into(),
                state: "OPEN".into(),
                labels: vec![],
            }],
            error: None,
            live_pr_state: None,
        });

        let repo_path = PathBuf::from("/tmp/test-extra-branches");
        let mut extra = std::collections::HashMap::new();
        extra.insert(repo_path.clone(), vec!["55-fix-thing".to_string()]);

        let (rx, handle) =
            start_with_extra_branches(vec![repo_path.clone()], &ws, &gc, r"^(\d+)-", &extra);

        let msg = recv_data(&rx);

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
            FetchMessage::FetchStarted => {
                panic!("unexpected FetchStarted after recv_data");
            }
        }

        handle.stop();
    }

    /// The fetcher must populate `current_user_login` from the
    /// github client's lookup on every successful tick, so the UI
    /// can classify review-request rows as direct-to-you vs. team.
    #[test]
    fn fetcher_populates_current_user_login() {
        let ws: Arc<dyn WorktreeService + Send + Sync> = Arc::new(MockWorktreeService {
            worktrees: vec![],
            github_remote: Some(("owner".to_string(), "repo".to_string())),
        });
        let gc: Arc<dyn crate::github_client::GithubClient + Send + Sync> = Arc::new(MockGithubClient::new());

        let (rx, handle) = start(vec![PathBuf::from("/tmp/test-repo")], &ws, &gc, r"^(\d+)-");

        match recv_data(&rx) {
            FetchMessage::RepoData(result) => {
                assert_eq!(
                    result.current_user_login.as_deref(),
                    Some("mock-user"),
                    "fetcher should carry the resolved login through to RepoData",
                );
            }
            FetchMessage::FetcherError { error, .. } => {
                panic!("unexpected FetcherError: {error}");
            }
            FetchMessage::FetchStarted => {
                panic!("unexpected FetchStarted after recv_data");
            }
        }

        handle.stop();
    }

    /// Regression for a silent-failure bug: when the login lookup
    /// fails the fetcher must emit a `FetcherError` so the status
    /// bar surfaces the problem, instead of silently swallowing the
    /// error with `.ok()` and degrading every review-request row to
    /// "team" with no user-visible indication. The fetch cycle must
    /// still complete (`RepoData` is sent) because worktrees, PRs,
    /// and issues are independent of the login lookup.
    #[test]
    fn fetcher_emits_error_when_current_user_login_fails() {
        let ws: Arc<dyn WorktreeService + Send + Sync> = Arc::new(MockWorktreeService {
            worktrees: vec![],
            github_remote: Some(("owner".to_string(), "repo".to_string())),
        });
        let gc: Arc<dyn crate::github_client::GithubClient + Send + Sync> = Arc::new(MockGithubClient {
            prs: vec![],
            review_requested_prs: vec![],
            issues: vec![],
            error: Some(crate::github_client::GithubError::ApiError(
                "simulated login failure".into(),
            )),
            live_pr_state: None,
        });

        let (rx, handle) = start(vec![PathBuf::from("/tmp/test-repo")], &ws, &gc, r"^(\d+)-");

        // Collect every message from the first tick until we have
        // seen both the login FetcherError and the RepoData, or the
        // channel goes quiet. A single tick can emit multiple
        // FetcherError messages (one per failed sub-step) so we keep
        // draining instead of returning on the first match.
        // Iteration cap matches `side_effects::clock::bounded_recv`
        // (6000) rather than the historical 1000. On Ubuntu CI the
        // mock-clock `sleep` is pure `yield_now`, and 1000 yields is
        // not always enough scheduler runtime for the fetcher worker
        // to emit both the login FetcherError and the RepoData on a
        // heavily-loaded runner; 6000 absorbs the jitter while still
        // bounding a genuine livelock.
        let mut saw_login_error = false;
        let mut saw_repo_data = false;
        for _ in 0..6_000 {
            if saw_login_error && saw_repo_data {
                break;
            }
            match rx.try_recv() {
                Ok(FetchMessage::FetchStarted) => {}
                Ok(FetchMessage::FetcherError { error, .. }) => {
                    if error.contains("failed to look up current user login") {
                        saw_login_error = true;
                    }
                }
                Ok(FetchMessage::RepoData(result)) => {
                    saw_repo_data = true;
                    assert!(
                        result.current_user_login.is_none(),
                        "login should be None when the lookup failed",
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {
                    crate::side_effects::clock::sleep(Duration::from_millis(1));
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        assert!(
            saw_login_error,
            "fetcher must emit a FetcherError mentioning the login lookup failure",
        );
        assert!(
            saw_repo_data,
            "fetch cycle must still send RepoData despite the login failure",
        );

        handle.stop();
    }
}
