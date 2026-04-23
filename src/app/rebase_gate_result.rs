//! Rebase-gate harness output -> `RebaseResult` conversion, extracted
//! from `rebase_gate_compute` so that file stays within the 700-line
//! ceiling. The entry point `build_rebase_result_from_output` is
//! invoked from `rebase_gate_compute::spawn_and_collect_harness_result`
//! once the harness sub-thread exits.
//!
//! Parsing + verification only - no subprocess spawning beyond the
//! read-only `git merge-base --is-ancestor` / `git rev-parse
//! REBASE_HEAD` probes used to validate a claimed success.

use super::RebaseResult;

/// Convert the harness sub-thread's stdout/exit into a
/// `RebaseResult`. Parses the `{success, conflicts_resolved, detail}`
/// envelope, and on a success claim verifies the worktree's HEAD
/// actually has `origin/<base_branch>` as an ancestor and that no
/// rebase is mid-flight (`REBASE_HEAD` missing). Hallucinated
/// successes are turned into concrete `RebaseResult::Failure`
/// variants so the caller can audit them.
pub(super) fn build_rebase_result_from_output(
    harness_output: Result<std::process::Output, crossbeam_channel::RecvError>,
    worktree_path: &std::path::Path,
    base_branch: &str,
    conflicts_attempted_observed: bool,
) -> RebaseResult {
    match harness_output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            match serde_json::from_str::<serde_json::Value>(&stdout) {
                Ok(envelope) => build_rebase_result_from_envelope(
                    &envelope,
                    worktree_path,
                    base_branch,
                    conflicts_attempted_observed,
                ),
                Err(e) => RebaseResult::Failure {
                    base_branch: base_branch.to_string(),
                    reason: format!("rebase gate: invalid JSON envelope: {e}"),
                    conflicts_attempted: conflicts_attempted_observed,
                    activity_log_error: None,
                },
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            RebaseResult::Failure {
                base_branch: base_branch.to_string(),
                reason: format!("harness exited with error: {}", stderr.trim()),
                conflicts_attempted: conflicts_attempted_observed,
                activity_log_error: None,
            }
        }
        Err(e) => RebaseResult::Failure {
            base_branch: base_branch.to_string(),
            reason: format!("rebase gate: harness thread disconnected: {e}"),
            conflicts_attempted: conflicts_attempted_observed,
            activity_log_error: None,
        },
    }
}

/// Inspect the harness's `{success, conflicts_resolved, detail}`
/// envelope and build a `RebaseResult`. When the harness claims
/// success, this helper verifies both `origin/<base>` is an ancestor
/// of HEAD and that no rebase is in progress - the
/// user-facing-claim rule in CLAUDE.md requires verifiable
/// assertions to be verified before rendering. Hallucinated
/// successes become `RebaseResult::Failure` with a concrete reason.
fn build_rebase_result_from_envelope(
    envelope: &serde_json::Value,
    worktree_path: &std::path::Path,
    base_branch: &str,
    conflicts_attempted_observed: bool,
) -> RebaseResult {
    let structured = &envelope["structured_output"];
    let success = structured["success"].as_bool().unwrap_or(false);
    let conflicts_resolved = structured["conflicts_resolved"].as_bool().unwrap_or(false);
    let detail = structured["detail"].as_str().unwrap_or("").to_string();
    if !success {
        return RebaseResult::Failure {
            base_branch: base_branch.to_string(),
            reason: if detail.is_empty() {
                "harness reported failure".into()
            } else {
                detail
            },
            conflicts_attempted: conflicts_resolved || conflicts_attempted_observed,
            activity_log_error: None,
        };
    }
    let ancestry_ok = match crate::worktree_service::git_command()
        .arg("-C")
        .arg(worktree_path)
        .args([
            "merge-base",
            "--is-ancestor",
            &format!("origin/{base_branch}"),
            "HEAD",
        ])
        .output()
    {
        Ok(o) => o.status.success(),
        Err(_) => false,
    };
    // During a conflicted rebase HEAD has already advanced past
    // origin/<base> so the ancestry check passes, but `REBASE_HEAD`
    // exists while git is waiting for conflict resolution. If the
    // harness hallucinated success while leaving the worktree
    // mid-rebase, this catches it.
    let rebase_in_progress = crate::worktree_service::git_command()
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-parse", "--verify", "--quiet", "REBASE_HEAD"])
        .output()
        .is_ok_and(|o| o.status.success());
    if ancestry_ok && !rebase_in_progress {
        RebaseResult::Success {
            base_branch: base_branch.to_string(),
            conflicts_resolved,
            activity_log_error: None,
        }
    } else if !ancestry_ok {
        RebaseResult::Failure {
            base_branch: base_branch.to_string(),
            reason: format!(
                "harness reported success but origin/{base_branch} is not an ancestor of HEAD"
            ),
            conflicts_attempted: conflicts_resolved || conflicts_attempted_observed,
            activity_log_error: None,
        }
    } else {
        RebaseResult::Failure {
            base_branch: base_branch.to_string(),
            reason:
                "harness reported success but a rebase is still in progress (REBASE_HEAD exists)"
                    .into(),
            conflicts_attempted: true,
            activity_log_error: None,
        }
    }
}
