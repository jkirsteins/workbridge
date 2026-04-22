use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use regex::Regex;

use crate::github_client::GithubClient;
use crate::work_item::{FetchMessage, RepoFetchResult};
use crate::worktree_service::WorktreeService;

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
pub(super) fn fetcher_loop(
    repo_path: PathBuf,
    tx: &mpsc::Sender<FetchMessage>,
    stop: &Arc<AtomicBool>,
    worktree_service: &Arc<dyn WorktreeService + Send + Sync>,
    github_client: &Arc<dyn GithubClient + Send + Sync>,
    issue_pattern: &str,
    extra_branches: &[String],
) {
    let re = match Regex::new(issue_pattern) {
        Ok(r) => r,
        Err(e) => {
            let msg = FetchMessage::FetcherError {
                repo_path,
                error: format!("invalid issue pattern '{issue_pattern}': {e}"),
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

        // Step 0: notify the UI that a fetch cycle is starting
        let _ = tx.send(FetchMessage::FetchStarted);

        // Step 1: list worktrees
        let worktrees = worktree_service.list_worktrees(&repo_path);

        // Step 2: determine GitHub remote
        let github_remote = match worktree_service.github_remote(&repo_path) {
            Ok(remote) => remote,
            Err(e) => {
                let msg = FetchMessage::FetcherError {
                    repo_path: repo_path.clone(),
                    error: format!("failed to determine GitHub remote: {e}"),
                };
                if tx.send(msg).is_err() {
                    break;
                }
                // Sleep and retry next iteration
                if !interruptible_sleep(stop, 120) {
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

        // Step 3b: fetch review-requested PRs
        let review_requested_prs = match &github_remote {
            Some((owner, repo)) => github_client.list_review_requested_prs(owner, repo),
            None => Ok(Vec::new()),
        };

        // Step 3c: resolve the current user's GitHub login so the UI
        // can classify review-request rows as direct-to-you vs. team.
        //
        // Why a dedicated call: `list_review_requested_prs` uses
        // `--search review-requested:@me`, which filters server-side
        // by the authenticated user but never echoes the login back
        // in the response. `gh pr list --json` has no field that
        // exposes the viewer's login (the `reviewRequests` array on
        // each PR contains requested reviewer identities, not the
        // caller's), and there is no `gh pr list` flag that adds one.
        // Classifying a row as direct-to-you requires matching the
        // literal login against `requested_reviewer_logins`, so the
        // login has to come from somewhere - hence a dedicated
        // `gh api user` call. `GhCliClient` caches the result after
        // the first successful call, so repeated ticks cost nothing
        // beyond the cache read.
        //
        // Failure is non-fatal for the fetch cycle (we still send the
        // repo data so worktrees, PRs, and issues update on schedule)
        // but it is NOT silent: we emit a `FetcherError` message so
        // the status bar surfaces the problem instead of letting every
        // review-request row degrade to "team" with no indication.
        let current_user_login = match github_client.current_user_login() {
            Ok(login) => Some(login),
            Err(e) => {
                let _ = tx.send(FetchMessage::FetcherError {
                    repo_path: repo_path.clone(),
                    error: format!("failed to look up current user login: {e}"),
                });
                None
            }
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
            for branch in extra_branches {
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
            review_requested_prs,
            issues,
            current_user_login,
        };

        if tx.send(FetchMessage::RepoData(Box::new(result))).is_err() {
            // Receiver dropped - main thread no longer listening
            break;
        }

        // Step 6: sleep 120s in 1s increments, checking stop flag
        if !interruptible_sleep(stop, 120) {
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
        crate::side_effects::clock::sleep(Duration::from_secs(1));
    }
    true
}
