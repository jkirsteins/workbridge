//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    App, Arc, BackendType, Config, CountingPlanBackend, Duration, GithubError,
    MergePreCheckMessage, OrphanWorktree, PathBuf, StubBackend, StubWorktreeService, UserActionKey,
    UserActionPayload, WorkItemBackend, WorkItemId, WorkItemStatus, WorktreeService,
    install_cached_repo_with_cleanliness, push_selected_review_item,
};

/// Regression: `delete_work_item_by_id` (Phase 5 in-flight
/// cleanup) must drop the in-flight precheck receiver in the
/// same step as releasing the slot. Same structural-ownership
/// contract as `retreat_stage_drops_merge_precheck_payload`.
#[test]
fn delete_work_item_drops_merge_precheck_payload() {
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(CountingPlanBackend::default()) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/delete-drops-precheck.json"));
    let repo_path = PathBuf::from("/tmp/delete-drops-precheck-repo");
    let branch_name = "feature/delete-drops-precheck".to_string();

    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "delete-drops-precheck".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path,
            branch: Some(branch_name),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Precheck-phase state: slot admitted, payload swapped.
    app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
        .expect("helper admit should succeed in test setup");
    app.merge_flow.in_progress = true;
    app.merge_wi_id = Some(wi_id.clone());
    let (_tx_keep_alive, rx) = crossbeam_channel::bounded::<MergePreCheckMessage>(1);
    app.attach_user_action_payload(
        &UserActionKey::PrMerge,
        UserActionPayload::PrMergePrecheck { rx },
    );

    let mut warnings: Vec<String> = Vec::new();
    let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
    app.delete_work_item_by_id(&wi_id, &mut warnings, &mut orphan_worktrees)
        .expect("delete must succeed");

    assert!(
        !app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "delete-cleanup must release the PrMerge slot",
    );
    assert!(
        !app.is_merge_precheck_phase(),
        "releasing the slot must structurally drop the precheck payload",
    );
    assert!(!app.merge_flow.in_progress);
}

/// `poll_merge_precheck` must be a no-op when the helper slot
/// has no `PrMergePrecheck` payload - either because no merge
/// is in flight at all, or because a cancel path released the
/// slot. With the structural ownership refactor this is now a
/// trivial early return rather than a defense-in-depth guard,
/// because there is no way for the receiver to outlive the
/// slot. This test pins that the early return holds for the
/// "no slot at all" case.
#[test]
fn poll_merge_precheck_noop_without_slot() {
    let mut app = App::new();

    // No `try_begin_user_action`, no payload attached. Must not
    // panic; must not touch any modal state.
    app.poll_merge_precheck();

    assert!(!app.is_user_action_in_flight(&UserActionKey::PrMerge));
    assert!(!app.is_merge_precheck_phase());
    assert!(app.alert_message.is_none());
}

/// End-to-end: a stale `dirty: true` cache + a clean live
/// `WorktreeService::list_worktrees` response must let the merge
/// proceed past the precheck. Drives the actual background thread
/// via a polling loop with a short timeout.
#[test]
fn execute_merge_through_live_precheck_clears_stale_dirty() {
    use crate::config::InMemoryConfigProvider;
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    struct CleanLiveMock {
        branch: String,
        repo: PathBuf,
    }
    impl WorktreeService for CleanLiveMock {
        fn list_worktrees(
            &self,
            repo_path: &std::path::Path,
        ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            assert_eq!(repo_path, self.repo);
            Ok(vec![WorktreeInfo {
                path: self.repo.join(".worktrees").join(&self.branch),
                branch: Some(self.branch.clone()),
                is_main: false,
                has_commits_ahead: Some(true),
                dirty: Some(false),
                untracked: Some(false),
                unpushed: Some(0),
                behind_remote: Some(0),
            }])
        }
        fn create_worktree(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _target_dir: &std::path::Path,
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

    let repo = PathBuf::from("/tmp/exec-merge-live-clean");
    let branch = "feature/live-clean".to_string();

    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(CleanLiveMock {
            branch: branch.clone(),
            repo: repo.clone(),
        }),
        Box::new(InMemoryConfigProvider::new()),
    );

    // Stale-dirty cache (the bug condition). The cache also carries
    // the github_remote so the synchronous validity check passes.
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        &branch,
        Some(true),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/exec-merge-live-clean.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, &branch);
    app.merge_flow.confirm = true;
    app.merge_wi_id = Some(wi_id.clone());

    app.execute_merge(&wi_id, "squash");

    assert!(app.is_merge_precheck_phase());
    assert!(app.is_user_action_in_flight(&UserActionKey::PrMerge));
    assert!(app.merge_flow.in_progress);

    // Drive the background thread via a polling loop. Bounded by a
    // 2s timeout so a regression cannot wedge CI forever.
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
        "the merge slot must stay reserved across the precheck->merge handoff",
    );
    assert!(app.merge_flow.in_progress);
    assert!(
        app.alert_message.is_none(),
        "live-clean precheck must not surface an alert; got: {:?}",
        app.alert_message,
    );
    assert!(
        app.merge_flow.confirm,
        "merge modal must stay open through the handoff to the merge thread",
    );
}

/// End-to-end: when `WorktreeService::list_worktrees` returns an
/// error, the precheck blocks the merge with a "working-tree check
/// failed" alert and releases the helper slot.
#[test]
fn execute_merge_through_live_precheck_surfaces_error() {
    use crate::config::InMemoryConfigProvider;
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    struct FailingMock;
    impl WorktreeService for FailingMock {
        fn list_worktrees(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            Err(WorktreeError::GitError("simulated git failure".into()))
        }
        fn create_worktree(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _target_dir: &std::path::Path,
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

    let repo = PathBuf::from("/tmp/exec-merge-live-error");
    let branch = "feature/live-error".to_string();

    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(FailingMock),
        Box::new(InMemoryConfigProvider::new()),
    );

    // Cache has clean state, so the cached path would say "go". The
    // live precheck is the only thing that catches the error.
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        &branch,
        Some(false),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/exec-merge-live-error.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, &branch);
    app.merge_flow.confirm = true;
    app.merge_wi_id = Some(wi_id.clone());

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

    assert!(!app.is_merge_precheck_phase());
    assert!(
        !app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "an errored precheck must release the PrMerge slot",
    );
    assert!(!app.merge_flow.in_progress);
    assert!(!app.merge_flow.confirm);
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("working-tree check failed"),
        "alert must contain the precheck error wording; got: {msg}",
    );
}

/// Regression: when `WorktreeService::list_worktrees` returns an
/// OK response with no entry matching the branch, the precheck
/// must fall through to `Ready`, NOT block with "branch not
/// found in worktree list". PR-only / reassembled work items and
/// branches whose worktree was removed after pushing have no
/// local tree to protect, so refusing to merge would make
/// perfectly safe PRs unmergeable from the UI. The cached guard
/// this replaced treated a missing cache entry as `Clean` for
/// the same reason.
#[test]
fn execute_merge_through_live_precheck_allows_no_worktree() {
    use crate::config::InMemoryConfigProvider;
    use crate::worktree_service::{WorktreeError, WorktreeInfo};

    struct EmptyListMock;
    impl WorktreeService for EmptyListMock {
        fn list_worktrees(
            &self,
            _repo_path: &std::path::Path,
        ) -> Result<Vec<WorktreeInfo>, WorktreeError> {
            // No matching worktree at all - the branch is
            // PR-only or its local checkout was removed.
            Ok(Vec::new())
        }
        fn create_worktree(
            &self,
            _repo_path: &std::path::Path,
            _branch: &str,
            _target_dir: &std::path::Path,
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

    let repo = PathBuf::from("/tmp/exec-merge-no-worktree");
    let branch = "feature/no-worktree".to_string();

    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(EmptyListMock),
        Box::new(InMemoryConfigProvider::new()),
    );

    // Cache carries the github_remote so the synchronous validity
    // check passes; the cached worktree entry is clean (the cache
    // is allowed to disagree with reality - the precheck is the
    // authority).
    install_cached_repo_with_cleanliness(
        &mut app,
        &repo,
        &branch,
        Some(false),
        Some(false),
        Some(0),
        Some(0),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/exec-merge-no-worktree.json"));
    push_selected_review_item(&mut app, &wi_id, &repo, &branch);
    app.merge_flow.confirm = true;
    app.merge_wi_id = Some(wi_id.clone());

    app.execute_merge(&wi_id, "squash");

    // Drain the precheck.
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
        app.alert_message.is_none(),
        "no-worktree case must NOT surface a 'branch not found' alert; got: {:?}",
        app.alert_message,
    );
    assert!(
        app.is_user_action_in_flight(&UserActionKey::PrMerge),
        "no-worktree case must hand off to the merge phase, keeping the slot reserved",
    );
    assert!(
        app.merge_flow.confirm,
        "merge modal must stay open through the handoff to the merge thread",
    );
}

// -------------------------------------------------------------------
// Live remote PR precheck: conflict / CI failure / clean / error
// -------------------------------------------------------------------

/// Bundle of knobs consumed by `install_live_pr_precheck_app`.
/// Groups the live-PR state, branch/repo identity, and cleanliness
/// cache seeds so the helper signature stays short.
pub struct LivePrPrecheckSpec<'a> {
    pub live_pr_state: Option<Result<crate::github_client::LivePrState, GithubError>>,
    pub branch: &'a str,
    pub repo: &'a std::path::Path,
    pub cache_dirty: Option<bool>,
    pub cache_untracked: Option<bool>,
    pub cache_unpushed: Option<u32>,
}
