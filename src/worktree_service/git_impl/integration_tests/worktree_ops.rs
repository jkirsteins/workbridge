//! Integration tests covering worktree lifecycle operations:
//! list / create / remove, plus the "invalid repo" error path.

use std::fs;

use super::super::GitWorktreeService;
use super::{git_cmd, run_in, setup_git_repo};
use crate::worktree_service::WorktreeService;

#[test]
fn list_worktrees_returns_non_default_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    // Create a secondary worktree on a feature branch.
    let wt_dir = tmp.path().join("wt-feature");
    run_in(
        &repo_dir,
        &[
            "git",
            "worktree",
            "add",
            wt_dir.to_str().unwrap(),
            "-b",
            "feature-branch",
        ],
    );

    let svc = GitWorktreeService;
    let worktrees = svc.list_worktrees(&repo_dir).unwrap();

    // The main worktree is on "master" (git init default). With the
    // improved fallback, default_branch detects the local "master"
    // branch, so the main worktree IS filtered out. Only the feature
    // worktree should remain.
    let branches: Vec<Option<&str>> = worktrees.iter().map(|w| w.branch.as_deref()).collect();
    assert!(
        branches.contains(&Some("feature-branch")),
        "feature-branch should be listed, got: {branches:?}",
    );
    assert!(
        !branches.contains(&Some("master")),
        "main worktree on 'master' should be filtered out, got: {branches:?}",
    );
}

#[test]
fn list_worktrees_filters_main_on_default_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    // Rename the branch to "main" so it matches the fallback default.
    run_in(&repo_dir, &["git", "branch", "-m", "main"]);

    let svc = GitWorktreeService;
    let worktrees = svc.list_worktrees(&repo_dir).unwrap();

    // The main worktree is on "main" which matches the default, so it
    // should be filtered out. No other worktrees exist.
    assert!(
        worktrees.is_empty(),
        "main worktree on default branch should be filtered, got: {worktrees:?}",
    );
}

#[test]
fn create_and_remove_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    let svc = GitWorktreeService;
    let wt_dir = tmp.path().join("new-worktree");

    // Create a worktree with a new branch.
    let info = svc
        .create_worktree(&repo_dir, "test-branch", &wt_dir)
        .unwrap();
    assert_eq!(info.path, wt_dir);
    assert_eq!(info.branch, Some("test-branch".to_string()));
    assert!(!info.is_main);
    assert!(wt_dir.exists(), "worktree directory should exist on disk");

    // Remove the worktree (with branch deletion).
    svc.remove_worktree(&repo_dir, &wt_dir, true, false)
        .unwrap();
    assert!(
        !wt_dir.exists(),
        "worktree directory should be removed from disk"
    );

    // Verify the branch was deleted.
    let branch_check = git_cmd(&repo_dir)
        .args(["rev-parse", "--verify", "refs/heads/test-branch"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        !branch_check.status.success(),
        "branch should have been deleted"
    );
}

#[test]
fn create_worktree_with_existing_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    // Create a branch first without a worktree.
    run_in(&repo_dir, &["git", "branch", "existing-branch"]);

    let svc = GitWorktreeService;
    let wt_dir = tmp.path().join("existing-wt");

    let info = svc
        .create_worktree(&repo_dir, "existing-branch", &wt_dir)
        .unwrap();
    assert_eq!(info.branch, Some("existing-branch".to_string()));
    assert!(wt_dir.exists());
}

#[test]
fn invalid_repo_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let not_a_repo = tmp.path().join("not-a-repo");
    fs::create_dir_all(&not_a_repo).unwrap();

    let svc = GitWorktreeService;
    let result = svc.list_worktrees(&not_a_repo);
    assert!(result.is_err());
}

#[test]
fn remove_worktree_force_removes_dirty() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    let svc = GitWorktreeService;
    let wt_dir = tmp.path().join("wt-force");

    svc.create_worktree(&repo_dir, "force-branch", &wt_dir)
        .unwrap();

    // Dirty the worktree.
    fs::write(wt_dir.join("README"), "dirty").unwrap();

    // Force remove should succeed even though the worktree is dirty.
    svc.remove_worktree(&repo_dir, &wt_dir, true, true).unwrap();
    assert!(
        !wt_dir.exists(),
        "worktree directory should be removed from disk",
    );
}

#[test]
fn remove_worktree_non_force_fails_dirty() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    let svc = GitWorktreeService;
    let wt_dir = tmp.path().join("wt-noforce");

    svc.create_worktree(&repo_dir, "noforce-branch", &wt_dir)
        .unwrap();

    // Dirty the worktree.
    fs::write(wt_dir.join("README"), "dirty").unwrap();

    // Non-force remove should fail for a dirty worktree.
    let result = svc.remove_worktree(&repo_dir, &wt_dir, false, false);
    assert!(
        result.is_err(),
        "non-force remove of dirty worktree should fail, got: {result:?}",
    );
}
