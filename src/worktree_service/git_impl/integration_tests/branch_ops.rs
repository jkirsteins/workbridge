//! Integration tests covering branch / fetch / default-branch / github-remote
//! operations. These shell out to real git (bare remotes, local clones,
//! etc.) and are therefore gated by the `integration` feature along with
//! the rest of this test tree.

use std::fs;

use super::super::GitWorktreeService;
use super::{commit_in, git_cmd, run_in, setup_git_repo};
use crate::worktree_service::WorktreeService;

#[test]
fn default_branch_falls_back_to_main() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);
    // Rename to "main" so the local branch check finds it.
    run_in(&repo_dir, &["git", "branch", "-m", "main"]);

    let svc = GitWorktreeService;
    // No remote configured, so symbolic-ref will fail. Should find
    // local "main" branch and fall back to it.
    let branch = svc.default_branch(&repo_dir).unwrap();
    assert_eq!(branch, "main");
}

#[test]
fn default_branch_falls_back_to_master() {
    // F-2 regression: repos whose trunk is "master" should get "master"
    // as the default branch, not "main".
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);
    // Explicitly rename to "master" to test the fallback, since modern
    // git may create "main" by default depending on configuration.
    run_in(&repo_dir, &["git", "branch", "-m", "master"]);

    let svc = GitWorktreeService;
    let branch = svc.default_branch(&repo_dir).unwrap();
    assert_eq!(
        branch, "master",
        "should detect 'master' when no origin/HEAD and 'main' branch does not exist"
    );
}

#[test]
fn default_branch_reads_from_remote_head() {
    let tmp = tempfile::tempdir().unwrap();

    // Create a bare "remote" repo.
    let remote_dir = tmp.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_in(&remote_dir, &["git", "init", "--bare"]);

    // Create a local repo and push to the remote.
    let repo_dir = tmp.path().join("local");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);
    run_in(
        &repo_dir,
        &[
            "git",
            "remote",
            "add",
            "origin",
            remote_dir.to_str().unwrap(),
        ],
    );
    // Rename the default branch to "develop" to test non-standard names.
    run_in(&repo_dir, &["git", "branch", "-m", "develop"]);
    run_in(&repo_dir, &["git", "push", "-u", "origin", "develop"]);
    // Set the remote HEAD.
    run_in(
        &repo_dir,
        &[
            "git",
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/develop",
        ],
    );

    let svc = GitWorktreeService;
    let branch = svc.default_branch(&repo_dir).unwrap();
    assert_eq!(branch, "develop");
}

#[test]
fn github_remote_parses_url() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    // Add a GitHub-style remote.
    run_in(
        &repo_dir,
        &[
            "git",
            "remote",
            "add",
            "origin",
            "git@github.com:myorg/myrepo.git",
        ],
    );

    let svc = GitWorktreeService;
    let result = svc.github_remote(&repo_dir).unwrap();
    assert_eq!(result, Some(("myorg".to_string(), "myrepo".to_string())));
}

#[test]
fn github_remote_returns_none_for_non_github() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    run_in(
        &repo_dir,
        &[
            "git",
            "remote",
            "add",
            "origin",
            "git@gitlab.com:myorg/myrepo.git",
        ],
    );

    let svc = GitWorktreeService;
    let result = svc.github_remote(&repo_dir).unwrap();
    assert_eq!(result, None);
}

#[test]
fn github_remote_returns_none_when_no_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    // No remote configured at all.
    let svc = GitWorktreeService;
    let result = svc.github_remote(&repo_dir).unwrap();
    assert_eq!(result, None);
}

/// F-2 regression: `github_remote()` must propagate git errors that are
/// NOT "no such remote". Only the specific "no such remote" error
/// should map to Ok(None); other failures (corruption, permissions)
/// must surface as Err.
#[test]
fn github_remote_propagates_non_no_such_remote_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    // Add a valid remote so "no such remote" is NOT the error.
    run_in(
        &repo_dir,
        &["git", "remote", "add", "origin", "git@github.com:o/r.git"],
    );

    // Corrupt the git config to make `git remote get-url origin` fail
    // with an error that is NOT "no such remote".
    let config_path = repo_dir.join(".git/config");
    fs::write(&config_path, "this is not valid git config").unwrap();

    let svc = GitWorktreeService;
    let result = svc.github_remote(&repo_dir);

    assert!(
        result.is_err(),
        "github_remote should propagate non-'no such remote' git errors, got: {result:?}",
    );
}

/// F-1 regression: `fetch_branch` should fetch a branch from origin so
/// the local ref points at the correct commit, and fail when the
/// branch does not exist on origin.
#[test]
fn fetch_branch_fetches_from_origin() {
    let tmp = tempfile::tempdir().unwrap();

    // Create a bare "remote" repo.
    let remote_dir = tmp.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_in(&remote_dir, &["git", "init", "--bare"]);

    // Create a "source" repo, push a branch to the remote.
    let source_dir = tmp.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();
    setup_git_repo(&source_dir);
    // Normalize to "main" so the test is portable across git configs.
    run_in(&source_dir, &["git", "branch", "-m", "main"]);
    run_in(
        &source_dir,
        &[
            "git",
            "remote",
            "add",
            "origin",
            remote_dir.to_str().unwrap(),
        ],
    );
    run_in(&source_dir, &["git", "push", "-u", "origin", "main"]);
    // Create a feature branch with a unique commit.
    run_in(&source_dir, &["git", "checkout", "-b", "pr-branch"]);
    let pr_file = source_dir.join("pr-change.txt");
    fs::write(&pr_file, "PR content").unwrap();
    run_in(&source_dir, &["git", "add", "pr-change.txt"]);
    commit_in(&source_dir, "PR commit");
    run_in(&source_dir, &["git", "push", "origin", "pr-branch"]);

    // Get the commit SHA on pr-branch in source.
    let expected_sha = git_cmd(&source_dir)
        .args(["rev-parse", "pr-branch"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    let expected_sha = String::from_utf8(expected_sha.stdout)
        .unwrap()
        .trim()
        .to_string();

    // Create a "local" clone that does NOT have pr-branch locally yet.
    let local_dir = tmp.path().join("local");
    fs::create_dir_all(&local_dir).unwrap();
    setup_git_repo(&local_dir);
    run_in(&local_dir, &["git", "branch", "-m", "main"]);
    run_in(
        &local_dir,
        &[
            "git",
            "remote",
            "add",
            "origin",
            remote_dir.to_str().unwrap(),
        ],
    );

    let svc = GitWorktreeService;

    // Before fetch: pr-branch should not exist locally.
    let before = GitWorktreeService::run_git(
        &local_dir,
        &["rev-parse", "--verify", "refs/heads/pr-branch"],
    );
    assert!(
        before.is_err(),
        "pr-branch should not exist locally before fetch",
    );

    // Fetch the branch.
    svc.fetch_branch(&local_dir, "pr-branch").unwrap();

    // After fetch: pr-branch should exist locally at the correct SHA.
    let actual_sha =
        GitWorktreeService::run_git(&local_dir, &["rev-parse", "refs/heads/pr-branch"])
            .unwrap()
            .trim()
            .to_string();
    assert_eq!(
        actual_sha, expected_sha,
        "local pr-branch should point at the same commit as origin",
    );
}

/// F-1 regression: `fetch_branch` should fail when the branch does not
/// exist on origin (e.g. fork PR branch).
#[test]
fn fetch_branch_fails_for_nonexistent_branch() {
    let tmp = tempfile::tempdir().unwrap();

    // Create a bare "remote" repo.
    let remote_dir = tmp.path().join("remote.git");
    fs::create_dir_all(&remote_dir).unwrap();
    run_in(&remote_dir, &["git", "init", "--bare"]);

    // Create a local repo with origin pointing to the bare remote.
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);
    // Normalize to "main" so the test is portable across git configs.
    run_in(&repo_dir, &["git", "branch", "-m", "main"]);
    run_in(
        &repo_dir,
        &[
            "git",
            "remote",
            "add",
            "origin",
            remote_dir.to_str().unwrap(),
        ],
    );
    run_in(&repo_dir, &["git", "push", "-u", "origin", "main"]);

    let svc = GitWorktreeService;

    // Try to fetch a branch that does not exist on origin.
    let result = svc.fetch_branch(&repo_dir, "nonexistent-branch");
    assert!(
        result.is_err(),
        "fetch_branch should fail for a branch not on origin, got: {result:?}",
    );
}

#[test]
fn create_branch_creates_from_default() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);
    // Normalize to "master" (git init default).
    run_in(&repo_dir, &["git", "branch", "-m", "master"]);

    // Advance HEAD away from master so we can verify the new branch
    // starts from master (default branch), not from HEAD.
    run_in(&repo_dir, &["git", "checkout", "-b", "detour"]);
    let detour_file = repo_dir.join("detour.txt");
    fs::write(&detour_file, "detour content").unwrap();
    run_in(&repo_dir, &["git", "add", "detour.txt"]);
    commit_in(&repo_dir, "detour commit");

    let master_sha = GitWorktreeService::run_git(&repo_dir, &["rev-parse", "refs/heads/master"])
        .unwrap()
        .trim()
        .to_string();
    let head_sha = GitWorktreeService::run_git(&repo_dir, &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    // HEAD should differ from master.
    assert_ne!(master_sha, head_sha, "HEAD should differ from master");

    let svc = GitWorktreeService;

    // Verify the branch does not exist yet.
    let before = GitWorktreeService::run_git(
        &repo_dir,
        &["rev-parse", "--verify", "refs/heads/my-feature"],
    );
    assert!(
        before.is_err(),
        "my-feature should not exist before create_branch",
    );

    // Create the branch - should be based on master, not HEAD.
    svc.create_branch(&repo_dir, "my-feature").unwrap();

    // Verify it now exists.
    let feature_sha =
        GitWorktreeService::run_git(&repo_dir, &["rev-parse", "refs/heads/my-feature"])
            .unwrap()
            .trim()
            .to_string();
    assert_eq!(
        feature_sha, master_sha,
        "new branch should start from default branch (master), not HEAD",
    );
}

#[test]
fn create_branch_noop_if_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);

    // Create the branch first.
    run_in(&repo_dir, &["git", "branch", "existing-branch"]);

    let svc = GitWorktreeService;
    // Should succeed without error (no-op).
    svc.create_branch(&repo_dir, "existing-branch").unwrap();

    // Branch should still exist.
    let check = GitWorktreeService::run_git(
        &repo_dir,
        &["rev-parse", "--verify", "refs/heads/existing-branch"],
    );
    assert!(check.is_ok(), "branch should still exist");
}

#[test]
fn delete_branch_non_force_fails_unmerged() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);
    // Normalize to a known branch name.
    run_in(&repo_dir, &["git", "branch", "-m", "main"]);

    // Create a branch with an unmerged commit.
    run_in(&repo_dir, &["git", "checkout", "-b", "unmerged-branch"]);
    let new_file = repo_dir.join("unmerged.txt");
    fs::write(&new_file, "unmerged content").unwrap();
    run_in(&repo_dir, &["git", "add", "unmerged.txt"]);
    commit_in(&repo_dir, "unmerged commit");
    // Switch back to the original branch so we can delete the other one.
    run_in(&repo_dir, &["git", "checkout", "main"]);

    let svc = GitWorktreeService;
    let result = svc.delete_branch(&repo_dir, "unmerged-branch", false);
    assert!(
        result.is_err(),
        "non-force delete of unmerged branch should fail, got: {result:?}",
    );
}

#[test]
fn delete_branch_force_succeeds_unmerged() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    fs::create_dir_all(&repo_dir).unwrap();
    setup_git_repo(&repo_dir);
    // Normalize to a known branch name.
    run_in(&repo_dir, &["git", "branch", "-m", "main"]);

    // Create a branch with an unmerged commit.
    run_in(&repo_dir, &["git", "checkout", "-b", "unmerged-branch"]);
    let new_file = repo_dir.join("unmerged.txt");
    fs::write(&new_file, "unmerged content").unwrap();
    run_in(&repo_dir, &["git", "add", "unmerged.txt"]);
    commit_in(&repo_dir, "unmerged commit");
    // Switch back to the original branch.
    run_in(&repo_dir, &["git", "checkout", "main"]);

    let svc = GitWorktreeService;
    svc.delete_branch(&repo_dir, "unmerged-branch", true)
        .unwrap();

    // Verify the branch no longer exists.
    let check = GitWorktreeService::run_git(
        &repo_dir,
        &["rev-parse", "--verify", "refs/heads/unmerged-branch"],
    );
    assert!(check.is_err(), "branch should have been force-deleted");
}
