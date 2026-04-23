//! Integration tests that shell out to real git. Gated behind the
//! `integration` feature so they don't run on every `cargo test`.
//! Run with: `cargo test --features integration`
//!
//! These tests use environment variables (`GIT_AUTHOR_EMAIL`, etc.)
//! instead of `git config` to avoid writing to any git config file.
//! This prevents worktree config writes from poisoning the parent
//! repo's .git/config (the root cause of the core.bare corruption).
//!
//! The test suite is split across two submodules that share the small
//! set of helpers defined below:
//!
//! - `worktree_ops` - worktree list / create / remove lifecycle tests
//! - `branch_ops` - branch / fetch / default-branch / github-remote tests

use std::fs;
use std::path::Path;
use std::process::Command;

mod branch_ops;
mod worktree_ops;

/// Build a Command with git environment variables cleared so
/// child git processes operate on `dir` instead of inheriting
/// the parent worktree's `GIT_DIR/GIT_WORK_TREE`.
fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR");
    cmd
}

/// Create a temporary git repo with an initial commit.
/// Uses env vars for author identity - NEVER calls `git config`.
fn setup_git_repo(dir: &Path) {
    run_in(dir, &["git", "init"]);
    let file_path = dir.join("README");
    fs::write(&file_path, "init").unwrap();
    run_in(dir, &["git", "add", "README"]);
    // Use -c flags for author identity instead of git config.
    let output = git_cmd(dir)
        .args([
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            "initial commit",
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("git commit failed in {}:\n{stderr}", dir.display());
    }
}

fn run_in(dir: &Path, args: &[&str]) {
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..]).current_dir(dir);
    // Clear git env vars so child processes use `dir` as their repo,
    // not the parent worktree.
    if args[0] == "git" {
        cmd.env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR");
    }
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to run {args:?}: {e}"));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("command {:?} failed in {}:\n{stderr}", args, dir.display());
    }
}

/// Helper for git commits that need author identity without git config.
fn commit_in(dir: &Path, message: &str) {
    let output = git_cmd(dir)
        .args([
            "-c",
            "user.email=test@test.com",
            "-c",
            "user.name=Test",
            "commit",
            "-m",
            message,
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("git commit failed in {}:\n{stderr}", dir.display());
    }
}
