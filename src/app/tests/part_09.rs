//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    App, Arc, BackendType, Config, Duration, LivePrPrecheckSpec, PathBuf, StubBackend,
    UserActionKey, WorkItemId, WorkItemStatus, WorktreeService,
    install_cached_repo_with_cleanliness, push_selected_review_item,
};

/// Helper to build an `App` with both a clean-worktree stub and a
/// configurable `MockGithubClient` driving the
/// `fetch_live_merge_state` return. Used by the remote-precheck
/// tests below.
pub fn install_live_pr_precheck_app(spec: LivePrPrecheckSpec<'_>) -> (App, WorkItemId) {
    use crate::config::InMemoryConfigProvider;
    use crate::github_client::MockGithubClient;
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    struct CleanWorktreeMock {
        branch: String,
        repo: PathBuf,
    }
    impl WorktreeService for CleanWorktreeMock {
        fn list_worktrees(
            &self,
            repo_path: &std::path::Path,
        ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            assert_eq!(repo_path, self.repo);
            Ok(vec![WorktreeInfo {
                path: self.repo.join(".worktrees").join(&self.branch),
                branch: Some(self.branch.clone()),
                is_main: false,
                has_commits_ahead: Some(false),
                dirty: Some(false),
                untracked: Some(false),
                unpushed: Some(0),
                behind_remote: Some(0),
            }])
        }
        fn create_worktree(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &std::path::Path,
        ) -> Result<WorktreeInfo, WorktreeError> {
            Err(WorktreeError::GitError("not used".into()))
        }
        fn remove_worktree(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
            _: bool,
            _: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn delete_branch(
            &self,
            _: &std::path::Path,
            _: &str,
            _: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn default_branch(&self, _: &std::path::Path) -> Result<String, WorktreeError> {
            Ok("main".to_string())
        }
        fn github_remote(
            &self,
            _: &std::path::Path,
        ) -> Result<Option<(String, String)>, WorktreeError> {
            Ok(None)
        }
        fn fetch_branch(&self, _: &std::path::Path, _: &str) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn create_branch(&self, _: &std::path::Path, _: &str) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn prune_worktrees(&self, _: &std::path::Path) -> Result<(), WorktreeError> {
            Ok(())
        }
    }

    let LivePrPrecheckSpec {
        live_pr_state,
        branch,
        repo,
        cache_dirty,
        cache_untracked,
        cache_unpushed,
    } = spec;

    let github = MockGithubClient {
        prs: Vec::new(),
        review_requested_prs: Vec::new(),
        issues: Vec::new(),
        error: None,
        live_pr_state,
    };

    let worktree_service: Arc<dyn WorktreeService + Send + Sync> = Arc::new(CleanWorktreeMock {
        branch: branch.to_string(),
        repo: repo.to_path_buf(),
    });

    let mut app = App::with_config_worktree_and_github(
        Config::default(),
        Arc::new(StubBackend),
        worktree_service,
        Arc::new(github),
        Box::new(InMemoryConfigProvider::new()),
    );

    install_cached_repo_with_cleanliness(
        &mut app,
        repo,
        branch,
        cache_dirty,
        cache_untracked,
        cache_unpushed,
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(repo.join(format!("{}.json", branch.replace('/', "-"))));
    push_selected_review_item(&mut app, &wi_id, repo, branch);
    app.confirm_merge = true;
    app.merge_wi_id = Some(wi_id.clone());

    (app, wi_id)
}

/// Clean worktree + PR with `mergeable == CONFLICTING` => precheck
/// blocks with "PR has conflicts. ..." and releases the slot.
#[test]
fn execute_merge_through_live_precheck_blocks_on_pr_conflict() {
    use crate::github_client::LivePrState;
    use crate::work_item::{CheckStatus, MergeableState};

    let repo = PathBuf::from("/tmp/exec-merge-pr-conflict");
    let branch = "feature/pr-conflict";
    let (mut app, wi_id) = install_live_pr_precheck_app(LivePrPrecheckSpec {
        live_pr_state: Some(Ok(LivePrState {
            mergeable: MergeableState::Conflicting,
            check_rollup: CheckStatus::Passing,
            has_open_pr: true,
        })),
        branch,
        repo: &repo,
        cache_dirty: Some(false),
        cache_untracked: Some(false),
        cache_unpushed: Some(0),
    });

    app.execute_merge(&wi_id, "squash");
    assert!(app.is_merge_precheck_phase());

    let start = crate::side_effects::clock::instant_now();
    // 60s of mock-clock budget (6000 iterations of the 10ms mock
    // `sleep`) to absorb OS-scheduler jitter on loaded CI hosts.
    // `clock::sleep` is pure `yield_now` in tests, so each
    // iteration is only a few hundred microseconds of real time -
    // 6000 yields gives the background precheck thread ample
    // opportunity to finish while the mock clock advances. A true
    // livelock still trips this cap deterministically.
    while app.is_merge_precheck_phase()
        && crate::side_effects::clock::elapsed_since(start) < Duration::from_secs(60)
    {
        app.poll_merge_precheck();
        crate::side_effects::clock::sleep(Duration::from_millis(10));
    }

    assert!(
        !app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "conflicting PR must release the PrMerge slot",
    );
    assert!(!app.merge_in_progress);
    assert!(!app.confirm_merge);
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("PR has conflicts"),
        "alert must surface the PR-conflict wording; got: {msg}",
    );
}

/// Clean worktree + mergeable PR + CI rollup == FAILURE => precheck
/// blocks with "CI failing. ..." and releases the slot.
#[test]
fn execute_merge_through_live_precheck_blocks_on_ci_failure() {
    use crate::github_client::LivePrState;
    use crate::work_item::{CheckStatus, MergeableState};

    let repo = PathBuf::from("/tmp/exec-merge-ci-fail");
    let branch = "feature/ci-fail";
    let (mut app, wi_id) = install_live_pr_precheck_app(LivePrPrecheckSpec {
        live_pr_state: Some(Ok(LivePrState {
            mergeable: MergeableState::Mergeable,
            check_rollup: CheckStatus::Failing,
            has_open_pr: true,
        })),
        branch,
        repo: &repo,
        cache_dirty: Some(false),
        cache_untracked: Some(false),
        cache_unpushed: Some(0),
    });

    app.execute_merge(&wi_id, "squash");

    let start = crate::side_effects::clock::instant_now();
    // 60s of mock-clock budget (6000 iterations of the 10ms mock
    // `sleep`) to absorb OS-scheduler jitter on loaded CI hosts.
    // `clock::sleep` is pure `yield_now` in tests, so each
    // iteration is only a few hundred microseconds of real time -
    // 6000 yields gives the background precheck thread ample
    // opportunity to finish while the mock clock advances. A true
    // livelock still trips this cap deterministically.
    while app.is_merge_precheck_phase()
        && crate::side_effects::clock::elapsed_since(start) < Duration::from_secs(60)
    {
        app.poll_merge_precheck();
        crate::side_effects::clock::sleep(Duration::from_millis(10));
    }

    assert!(
        !app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "CI-failing PR must release the PrMerge slot",
    );
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("CI failing"),
        "alert must surface the CI-failing wording; got: {msg}",
    );
}

/// Clean worktree + no open PR => precheck falls through to the
/// merge phase (the downstream merge thread surfaces the existing
/// `NoPr` outcome).
#[test]
fn execute_merge_through_live_precheck_passes_with_no_open_pr() {
    use crate::github_client::LivePrState;

    let repo = PathBuf::from("/tmp/exec-merge-no-pr");
    let branch = "feature/no-pr";
    let (mut app, wi_id) = install_live_pr_precheck_app(LivePrPrecheckSpec {
        live_pr_state: Some(Ok(LivePrState::no_pr())),
        branch,
        repo: &repo,
        cache_dirty: Some(false),
        cache_untracked: Some(false),
        cache_unpushed: Some(0),
    });

    app.execute_merge(&wi_id, "squash");

    let start = crate::side_effects::clock::instant_now();
    // 60s of mock-clock budget (6000 iterations of the 10ms mock
    // `sleep`) to absorb OS-scheduler jitter on loaded CI hosts.
    // `clock::sleep` is pure `yield_now` in tests, so each
    // iteration is only a few hundred microseconds of real time -
    // 6000 yields gives the background precheck thread ample
    // opportunity to finish while the mock clock advances. A true
    // livelock still trips this cap deterministically.
    while app.is_merge_precheck_phase()
        && crate::side_effects::clock::elapsed_since(start) < Duration::from_secs(60)
    {
        app.poll_merge_precheck();
        crate::side_effects::clock::sleep(Duration::from_millis(10));
    }

    assert!(
        !app.is_merge_precheck_phase(),
        "precheck must drain within 60s",
    );
    assert!(
        app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "no-open-PR case must hand off to the merge phase, keeping the slot reserved",
    );
    assert!(
        app.alert_message.is_none(),
        "no-open-PR must NOT surface a precheck alert; got: {:?}",
        app.alert_message,
    );
    assert!(
        app.confirm_merge,
        "merge modal must stay open through the handoff to the merge thread",
    );
}

/// When `fetch_live_merge_state` returns an error, the precheck
/// blocks the merge with a "remote merge-state check failed" alert
/// and releases the slot. This is the P0 "surface errors, don't
/// auto-fix" posture.
#[test]
fn execute_merge_through_live_precheck_surfaces_remote_error() {
    let repo = PathBuf::from("/tmp/exec-merge-remote-error");
    let branch = "feature/remote-error";
    let (mut app, wi_id) = install_live_pr_precheck_app(LivePrPrecheckSpec {
        live_pr_state: Some(Err(crate::github_client::GithubError::ApiError(
            "simulated gh pr view failure".into(),
        ))),
        branch,
        repo: &repo,
        cache_dirty: Some(false),
        cache_untracked: Some(false),
        cache_unpushed: Some(0),
    });

    app.execute_merge(&wi_id, "squash");

    let start = crate::side_effects::clock::instant_now();
    // 60s of mock-clock budget (6000 iterations of the 10ms mock
    // `sleep`) to absorb OS-scheduler jitter on loaded CI hosts.
    // `clock::sleep` is pure `yield_now` in tests, so each
    // iteration is only a few hundred microseconds of real time -
    // 6000 yields gives the background precheck thread ample
    // opportunity to finish while the mock clock advances. A true
    // livelock still trips this cap deterministically.
    while app.is_merge_precheck_phase()
        && crate::side_effects::clock::elapsed_since(start) < Duration::from_secs(60)
    {
        app.poll_merge_precheck();
        crate::side_effects::clock::sleep(Duration::from_millis(10));
    }

    assert!(
        !app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "errored remote check must release the PrMerge slot",
    );
    assert!(!app.merge_in_progress);
    assert!(!app.confirm_merge);
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("remote merge-state check failed"),
        "alert must surface the remote-check error wording; got: {msg}",
    );
}

/// Priority drill-down: a dirty worktree + a conflicting PR must
/// block with the LOCAL wording ("Uncommitted changes..."), not the
/// remote wording. Drives the actual precheck thread so the
/// classifier's priority order is verified end-to-end.
#[test]
fn execute_merge_through_live_precheck_dirty_wins_over_pr_conflict() {
    use crate::config::InMemoryConfigProvider;
    use crate::github_client::{LivePrState, MockGithubClient};
    use crate::work_item::{CheckStatus, MergeableState};
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    struct DirtyWorktreeMock {
        branch: String,
        repo: PathBuf,
    }
    impl WorktreeService for DirtyWorktreeMock {
        fn list_worktrees(&self, _: &std::path::Path) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            Ok(vec![WorktreeInfo {
                path: self.repo.join(".worktrees").join(&self.branch),
                branch: Some(self.branch.clone()),
                is_main: false,
                has_commits_ahead: Some(false),
                dirty: Some(true),
                untracked: Some(false),
                unpushed: Some(0),
                behind_remote: Some(0),
            }])
        }
        fn create_worktree(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &std::path::Path,
        ) -> Result<WorktreeInfo, WorktreeError> {
            Err(WorktreeError::GitError("not used".into()))
        }
        fn remove_worktree(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
            _: bool,
            _: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn delete_branch(
            &self,
            _: &std::path::Path,
            _: &str,
            _: bool,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn default_branch(&self, _: &std::path::Path) -> Result<String, WorktreeError> {
            Ok("main".to_string())
        }
        fn github_remote(
            &self,
            _: &std::path::Path,
        ) -> Result<Option<(String, String)>, WorktreeError> {
            Ok(None)
        }
        fn fetch_branch(&self, _: &std::path::Path, _: &str) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn create_branch(&self, _: &std::path::Path, _: &str) -> Result<(), WorktreeError> {
            Ok(())
        }
        fn prune_worktrees(&self, _: &std::path::Path) -> Result<(), WorktreeError> {
            Ok(())
        }
    }

    let repo = PathBuf::from("/tmp/exec-merge-dirty-over-conflict");
    let branch = "feature/dirty-over-conflict".to_string();

    let github = MockGithubClient {
        prs: Vec::new(),
        review_requested_prs: Vec::new(),
        issues: Vec::new(),
        error: None,
        live_pr_state: Some(Ok(LivePrState {
            mergeable: MergeableState::Conflicting,
            check_rollup: CheckStatus::Passing,
            has_open_pr: true,
        })),
    };

    let mut app = App::with_config_worktree_and_github(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(DirtyWorktreeMock {
            branch: branch.clone(),
            repo: repo.clone(),
        }),
        Arc::new(github),
        Box::new(InMemoryConfigProvider::new()),
    );

    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        &branch,
        Some(true),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/exec-merge-dirty-over-conflict.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, &branch);
    app.confirm_merge = true;
    app.merge_wi_id = Some(wi_id.clone());

    app.execute_merge(&wi_id, "squash");

    let start = crate::side_effects::clock::instant_now();
    // 60s of mock-clock budget (6000 iterations of the 10ms mock
    // `sleep`) to absorb OS-scheduler jitter on loaded CI hosts.
    // `clock::sleep` is pure `yield_now` in tests, so each
    // iteration is only a few hundred microseconds of real time -
    // 6000 yields gives the background precheck thread ample
    // opportunity to finish while the mock clock advances. A true
    // livelock still trips this cap deterministically.
    while app.is_merge_precheck_phase()
        && crate::side_effects::clock::elapsed_since(start) < Duration::from_secs(60)
    {
        app.poll_merge_precheck();
        crate::side_effects::clock::sleep(Duration::from_millis(10));
    }

    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Uncommitted changes"),
        "dirty worktree must take priority over PR conflict; got: {msg}",
    );
    assert!(
        !msg.contains("PR has conflicts"),
        "must not surface the PR-conflict wording when worktree is dirty; got: {msg}",
    );
}

// -- Regression: execute_merge must not advance to Done without a real merge --

#[test]
fn execute_merge_no_repo_assoc_blocks_done() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-assoc.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "No assoc".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.execute_merge(&wi_id, "squash");
    let status = app
        .work_items
        .iter()
        .find(|w| w.id == wi_id)
        .unwrap()
        .status;
    assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(msg.contains("no repo association"), "got: {msg}");
}

#[test]
fn execute_merge_no_branch_blocks_done() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-branch.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "No branch".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/repo"),
            branch: None,
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
    app.execute_merge(&wi_id, "squash");
    let status = app
        .work_items
        .iter()
        .find(|w| w.id == wi_id)
        .unwrap()
        .status;
    assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(msg.contains("no branch"), "got: {msg}");
}

#[test]
fn execute_merge_no_github_remote_blocks_done() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-remote.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "No remote".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/repo"),
            branch: Some("feature/test".into()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
    // No repo_data entry for /tmp/repo, so the cached github_remote
    // lookup returns None and execute_merge blocks the merge.
    app.execute_merge(&wi_id, "squash");
    let status = app
        .work_items
        .iter()
        .find(|w| w.id == wi_id)
        .unwrap()
        .status;
    assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(msg.contains("GitHub remote not yet cached"), "got: {msg}");
}
