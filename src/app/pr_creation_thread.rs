//! PR-creation background thread body, extracted from `pr_creation`
//! so that file stays within the 700-line ceiling.
//!
//! The entry point `run_pr_creation_thread` is invoked from
//! `App::spawn_pr_creation` (in the sibling `pr_creation` module).
//! Every helper here runs on the spawned worker thread: they
//! shell out to `gh` / `git`, read the plan via the `WorkItemBackend`,
//! and ship a `PrCreateResult` back to the UI thread through `tx`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::work_item::WorkItemId;

/// Inputs captured on the UI thread before spawning the PR-creation
/// background worker. Kept as a struct so `run_pr_creation_thread`
/// has a stable call shape.
pub(super) struct PrCreationArgs {
    pub backend: Arc<dyn crate::work_item_backend::WorkItemBackend>,
    pub ws: Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
    pub wi_id: WorkItemId,
    pub title: String,
    pub branch: String,
    pub repo_path: PathBuf,
    pub owner_repo: String,
    pub tx: crossbeam_channel::Sender<super::PrCreateResult>,
}

/// Body of the background thread spawned by `spawn_pr_creation`.
/// Reads the plan body, resolves the default branch, checks for an
/// existing PR, pushes the branch, and (on success) creates the PR
/// via `gh pr create`. Ships a `PrCreateResult` back through `tx`.
pub(super) fn run_pr_creation_thread(args: PrCreationArgs) {
    let PrCreationArgs {
        backend,
        ws,
        wi_id,
        title,
        branch,
        repo_path,
        owner_repo,
        tx,
    } = args;

    // Blocking reads run on the background thread. `read_plan` hits
    // the filesystem; `default_branch` shells out to git. Both are
    // cheap per-call but absolutely prohibited on the UI thread.
    let body = match backend.read_plan(&wi_id) {
        Ok(Some(plan)) if !plan.trim().is_empty() => plan,
        _ => String::new(),
    };
    let default_branch = ws
        .default_branch(&repo_path)
        .unwrap_or_else(|_| "main".to_string());

    // Check if a PR already exists for this branch.
    if let Some(result) = check_existing_pr(&branch, &owner_repo, &wi_id) {
        let _ = tx.send(result);
        return;
    }

    // Ensure the branch is pushed to the remote before creating the PR.
    if let Some(result) = push_branch_for_pr(&branch, &repo_path, &wi_id) {
        let _ = tx.send(result);
        return;
    }

    // Create the PR.
    let result = run_gh_pr_create(&title, &body, &branch, &default_branch, &owner_repo, wi_id);
    let _ = tx.send(result);
}

/// Call `gh pr list --head ...` to check whether a PR already exists
/// for this branch. Returns `Some(result)` to short-circuit the
/// caller (either the PR already exists or the check itself failed)
/// or `None` to continue with the create path.
fn check_existing_pr(
    branch: &str,
    owner_repo: &str,
    wi_id: &WorkItemId,
) -> Option<super::PrCreateResult> {
    let check_output = std::process::Command::new("gh")
        .args([
            "pr", "list", "--head", branch, "--json", "number", "--repo", owner_repo,
        ])
        .output();

    match check_output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(arr) = serde_json::from_str::<serde_json::Value>(stdout.trim())
                && arr.as_array().is_some_and(|a| !a.is_empty())
            {
                // PR already exists - nothing to do.
                return Some(super::PrCreateResult {
                    wi_id: wi_id.clone(),
                    info: None,
                    error: None,
                    url: None,
                });
            }
            None
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Some(super::PrCreateResult {
                wi_id: wi_id.clone(),
                info: None,
                error: Some(format!("PR check failed (continuing): {stderr}")),
                url: None,
            })
        }
        Err(e) => Some(super::PrCreateResult {
            wi_id: wi_id.clone(),
            info: None,
            error: Some(format!("PR check failed (continuing): {e}")),
            url: None,
        }),
    }
}

/// Push the branch to origin before PR creation. Returns
/// `Some(result)` on push failure (so the caller can ship the error
/// back through the result channel), `None` on success.
fn push_branch_for_pr(
    branch: &str,
    repo_path: &std::path::Path,
    wi_id: &WorkItemId,
) -> Option<super::PrCreateResult> {
    let push_output = crate::worktree_service::git_command()
        .args(["push", "-u", "origin", branch])
        .current_dir(repo_path)
        .output();
    match push_output {
        Ok(output) if !output.status.success() => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Some(super::PrCreateResult {
                wi_id: wi_id.clone(),
                info: None,
                error: Some(format!("git push failed: {stderr}")),
                url: None,
            })
        }
        Err(e) => Some(super::PrCreateResult {
            wi_id: wi_id.clone(),
            info: None,
            error: Some(format!("git push failed: {e}")),
            url: None,
        }),
        _ => None, // push succeeded
    }
}

/// Execute `gh pr create` and turn its outcome into a
/// `PrCreateResult`. Handles success, non-zero exit, and spawn error
/// uniformly.
fn run_gh_pr_create(
    title: &str,
    body: &str,
    branch: &str,
    default_branch: &str,
    owner_repo: &str,
    wi_id: WorkItemId,
) -> super::PrCreateResult {
    let create_result = std::process::Command::new("gh")
        .args([
            "pr",
            "create",
            "--title",
            title,
            "--body",
            body,
            "--head",
            branch,
            "--base",
            default_branch,
            "--repo",
            owner_repo,
        ])
        .output();

    match create_result {
        Ok(output) if output.status.success() => {
            let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let info = format!("PR created: {url}");
            super::PrCreateResult {
                wi_id,
                info: Some(info),
                error: None,
                url: Some(url),
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            super::PrCreateResult {
                wi_id,
                info: None,
                error: Some(format!("PR creation failed (continuing): {stderr}")),
                url: None,
            }
        }
        Err(e) => super::PrCreateResult {
            wi_id,
            info: None,
            error: Some(format!("PR creation failed (continuing): {e}")),
            url: None,
        },
    }
}
