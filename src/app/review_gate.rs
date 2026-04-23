//! Review-gate subsystem - async review-gate spawn.
//!
//! Holds `spawn_review_gate`, which kicks off the background
//! review-gate job for a work item entering Review. Paired with
//! `poll_review_gate` in `gate_polling`. The review-gate is one
//! of the four known harness spawn paths in
//! `docs/harness-contract.md`.

use std::path::PathBuf;
use std::sync::Arc;

use super::{
    ReviewGateMessage, ReviewGateOrigin, ReviewGateResult, ReviewGateSpawn, ReviewGateState,
};
use crate::agent_backend::{AgentBackendKind, ReviewGateSpawnConfig};
use crate::work_item::{CheckStatus, WorkItemId};

impl super::App {
    /// Attempt to spawn the async review gate for the given work item.
    /// Returns `Spawned` if the gate is running (caller should wait),
    /// or `Blocked(reason)` if the transition must not proceed.
    ///
    /// Only synchronous, in-memory pre-conditions (gate already running,
    /// work item not found, no repo association, no branch) return
    /// `Blocked` from the main thread. Every blocking check -
    /// `backend.read_plan` (filesystem), `worktree_service.default_branch`
    /// / `github_remote` / `git diff` (git subprocess) - runs inside the
    /// background closure and reports failure through
    /// `ReviewGateMessage::Blocked` so the main UI thread is never blocked.
    ///
    /// `origin` records who initiated the gate; `poll_review_gate` uses
    /// it to decide whether a `Blocked` outcome should apply the full
    /// rework flow (kill + respawn the session) or just surface the
    /// reason as a status message without touching the session. See
    /// `ReviewGateOrigin`.
    pub(super) fn spawn_review_gate(
        &mut self,
        wi_id: &WorkItemId,
        origin: ReviewGateOrigin,
    ) -> ReviewGateSpawn {
        // Guard: if a review gate is already running for this item, don't spawn a duplicate.
        if self.review_gates.contains_key(wi_id) {
            return ReviewGateSpawn::Blocked("Review gate already running".into());
        }

        // Find the branch for this work item (pure in-memory read).
        // Clone everything off `wi`/`assoc` into owned values up-front so
        // the immutable borrow of `self.work_items` ends before the
        // mutable `start_activity` call below.
        let (title, branch, repo_path, current_pr_number, current_check_status) = {
            let Some(wi) = self.work_items.iter().find(|w| w.id == *wi_id) else {
                return ReviewGateSpawn::Blocked("Work item not found".into());
            };
            let Some(assoc) = wi.repo_associations.first() else {
                return ReviewGateSpawn::Blocked("Cannot enter Review: no repo association".into());
            };
            let branch = match assoc.branch.as_ref() {
                Some(b) => b.clone(),
                None => {
                    return ReviewGateSpawn::Blocked("Cannot enter Review: no branch set".into());
                }
            };
            // Two-level Option semantics:
            // - None = no cached PR data, must query fresh
            // - Some(CheckStatus::None) = PR cached but no CI checks configured, skip
            // - Some(other) = PR cached with CI checks, proceed to wait
            (
                wi.title.clone(),
                branch,
                assoc.repo_path.clone(),
                assoc.pr.as_ref().map(|p| p.number),
                assoc.pr.as_ref().map(|p| p.checks.clone()),
            )
        };

        // Resolve the per-work-item harness BEFORE starting any
        // activity or background work. The plan's Milestone 3
        // acceptance-criteria rule is "abort rather than default to
        // claude" - review gates only run after an interactive session
        // has existed (the c/x entry point records the choice), so a
        // missing `harness_choice` entry is a user-facing error, not a
        // silent default. See CLAUDE.md's `[ABSOLUTE]` silent-fallback
        // rule and the `harness_choice_applied_to_review_gate_spawn`
        // test.
        let Some(agent_backend) = self.backend_for_work_item(wi_id) else {
            return ReviewGateSpawn::Blocked(
                    "Cannot run review gate: no harness chosen for this work item. Press c / x to pick one and re-open the session first.".into(),
                );
        };

        let (review_extra_bridges, http_skipped_for_review) =
            self.collect_repo_mcp_bridges_for_repo(&repo_path);
        if agent_backend.kind() == AgentBackendKind::Codex && http_skipped_for_review > 0 {
            self.toasts.push(format!(
                "Codex: {http_skipped_for_review} HTTP MCP server(s) skipped (Codex requires stdio)"
            ));
        }

        // Status-bar activity for the review gate. Per `docs/UI.md`
        // "Activity indicator placement", review gates are
        // system-initiated background work and must own a status-bar
        // spinner. The ID lives on `ReviewGateState` so every drop site
        // ends it via `drop_review_gate`.
        let activity = self
            .activities
            .start(format!("Running review gate for '{title}'"));
        // Clone the worktree service and backend for the background thread so
        // that `default_branch()`/`github_remote()` (which shell out to git)
        // and `read_plan()` (filesystem read) execute off the main UI thread.
        let ws = Arc::clone(&self.services.worktree_service);
        let backend = Arc::clone(&self.services.backend);

        // Spawn the review gate in a background thread with three phases:
        // 1. PR existence check (if GitHub remote exists)
        // 2. CI check wait (if checks are configured)
        // 3. Adversarial code review (headless agent spawn)
        // Unbounded rather than bounded(1): multiple Progress messages may
        // queue before the main thread polls.
        let (tx, rx) = crossbeam_channel::unbounded();
        let wi_id_clone = wi_id.clone();
        let review_skill = self.services.config.defaults.review_skill.clone();
        let gate_mcp_tx = self.mcp_tx.clone();

        std::thread::spawn(move || {
            Self::run_review_gate_thread(ReviewGateThreadArgs {
                tx,
                wi_id: wi_id_clone,
                backend,
                ws,
                repo_path,
                branch,
                current_pr_number,
                current_check_status,
                review_skill,
                gate_mcp_tx,
                agent_backend,
                review_extra_bridges,
            });
        });

        self.review_gates.insert(
            wi_id.clone(),
            ReviewGateState {
                rx,
                progress: None,
                origin,
                activity,
            },
        );
        ReviewGateSpawn::Spawned
    }

    /// Drop a review gate and end its status-bar activity. Every site
    /// that removes a `review_gates` entry MUST go through this helper:
    /// the activity ID lives inside `ReviewGateState` per
    /// structural-ownership, so dropping the gate without ending the
    /// activity would leak a spinner. See `docs/UI.md` "Activity
    /// indicator placement".
    /// Body of the review-gate background thread. Runs Phase 0
    /// (plan read / diff sanity / github remote resolution), Phase 1
    /// (PR existence check), Phase 2 (CI wait), and Phase 3
    /// (adversarial code review), shipping outcomes back through
    /// `tx` on any bail-out path.
    fn run_review_gate_thread(args: ReviewGateThreadArgs) {
        let ReviewGateThreadArgs {
            tx,
            wi_id,
            backend,
            ws,
            repo_path,
            branch,
            current_pr_number,
            current_check_status,
            review_skill,
            gate_mcp_tx,
            agent_backend,
            review_extra_bridges,
        } = args;

        let Some(plan) = read_plan_or_send_blocked(backend.as_ref(), &wi_id, &tx) else {
            return;
        };

        let default_branch = ws
            .default_branch(&repo_path)
            .unwrap_or_else(|_| "main".to_string());

        if !verify_non_empty_diff(&repo_path, &default_branch, &branch, &wi_id, &tx) {
            return;
        }

        let Some((gh_owner, gh_repo, has_github_remote)) =
            resolve_github_remote(ws.as_ref(), &repo_path, &wi_id, &tx)
        else {
            return;
        };

        // === Phase 1: PR Existence Check ===
        let pr_number = if has_github_remote {
            if tx
                .send(ReviewGateMessage::Progress(
                    "Checking for pull request...".into(),
                ))
                .is_err()
            {
                return; // Receiver dropped - gate cancelled.
            }

            let pr_num = current_pr_number
                .or_else(|| Self::find_pr_for_branch(&gh_owner, &gh_repo, &branch));

            if pr_num.is_none() {
                let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                    work_item_id: wi_id,
                    approved: false,
                    detail: format!(
                        "No pull request found for branch '{branch}'. \
                         Create a PR before requesting review."
                    ),
                }));
                return;
            }
            pr_num
        } else {
            None
        };

        // === Phase 2: CI Check Wait ===
        if let Some(pr_num) = pr_number
            && !Self::wait_for_ci_checks(
                &gh_owner,
                &gh_repo,
                pr_num,
                current_check_status.as_ref(),
                &wi_id,
                &tx,
            )
        {
            return;
        }

        // === Phase 3: Adversarial Code Review ===
        run_review_gate_code_review(ReviewGatePhase3Args {
            tx: &tx,
            wi_id,
            plan: &plan,
            repo_path: &repo_path,
            default_branch: &default_branch,
            branch: &branch,
            review_skill: &review_skill,
            gate_mcp_tx,
            agent_backend: agent_backend.as_ref(),
            review_extra_bridges: &review_extra_bridges,
        });
    }

    /// Phase 2 of the review gate: wait for PR CI checks to settle.
    /// Runs on the review-gate background thread. Returns `true` to
    /// proceed to Phase 3 (code review), `false` when the gate should
    /// abort - either because a check failed, the timeout fired, or
    /// the receiver was dropped (gate cancelled).
    fn wait_for_ci_checks(
        gh_owner: &str,
        gh_repo: &str,
        pr_num: u64,
        current_check_status: Option<&CheckStatus>,
        wi_id: &WorkItemId,
        tx: &crossbeam_channel::Sender<ReviewGateMessage>,
    ) -> bool {
        // Determine whether CI checks are configured.
        let has_checks = match current_check_status {
            Some(CheckStatus::None) => false,
            Some(_) => true,
            None => {
                // No cached info - query fresh to discover.
                !Self::fetch_pr_checks(gh_owner, gh_repo, pr_num).is_empty()
            }
        };
        if !has_checks {
            return true;
        }

        if tx
            .send(ReviewGateMessage::Progress(
                "Waiting for CI checks...".into(),
            ))
            .is_err()
        {
            return false;
        }

        // 30-minute timeout: 120 iterations * 15s = 1800s.
        let max_iterations = 120u32;
        let mut iteration = 0u32;
        loop {
            if iteration >= max_iterations {
                let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                    work_item_id: wi_id.clone(),
                    approved: false,
                    detail: "CI checks did not complete within 30 minutes. \
                             Check the CI system and retry."
                        .into(),
                }));
                return false;
            }
            let checks = Self::fetch_pr_checks(gh_owner, gh_repo, pr_num);

            if checks.is_empty() {
                // Checks disappeared (race) - treat as no checks.
                return true;
            }

            let passed = checks
                .iter()
                .filter(|c| c.bucket == "pass" || c.bucket == "skipping")
                .count();
            let failed: Vec<_> = checks
                .iter()
                .filter(|c| c.bucket == "fail" || c.bucket == "cancel")
                .collect();
            let total = checks.len();

            if !failed.is_empty() {
                let failed_names: Vec<_> = failed.iter().map(|c| c.name.clone()).collect();
                let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                    work_item_id: wi_id.clone(),
                    approved: false,
                    detail: format!(
                        "CI checks failed: {}. Fix failures before requesting review.",
                        failed_names.join(", ")
                    ),
                }));
                return false;
            }

            if passed == total {
                let _ = tx.send(ReviewGateMessage::Progress(format!(
                    "{passed} / {total} CI checks green. Running code review..."
                )));
                return true;
            }

            if tx
                .send(ReviewGateMessage::Progress(format!(
                    "{passed} / {total} CI checks green"
                )))
                .is_err()
            {
                return false;
            }
            iteration += 1;
            crate::side_effects::clock::sleep(std::time::Duration::from_secs(15));
        }
    }

    pub(super) fn drop_review_gate(&mut self, wi_id: &WorkItemId) {
        if let Some(state) = self.review_gates.remove(wi_id) {
            self.activities.end(state.activity);
        }
    }

    /// Resolve per-repo MCP servers for a specific repo path and
    /// convert them into `McpBridgeSpec` entries so a background
    /// harness sub-thread can pass them through to Codex via
    /// per-key `-c` overrides alongside the workbridge bridge.
    /// HTTP entries are skipped because Codex's `mcp_servers.<name>`
    /// schema requires command + args (no `url` sub-field); Claude
    /// still sees HTTP entries via the JSON written into
    /// `mcp_config_path`. Returns `(bridges, http_skipped_count)` so
    /// the caller can emit a toast for the silent-skip case.
    fn collect_repo_mcp_bridges_for_repo(
        &self,
        repo_path: &std::path::Path,
    ) -> (Vec<crate::agent_backend::McpBridgeSpec>, usize) {
        let repo_display = crate::config::collapse_home(repo_path);
        let entries = self.services.config.mcp_servers_for_repo(&repo_display);
        let http_count = entries.iter().filter(|e| e.server_type == "http").count();
        let bridges: Vec<crate::agent_backend::McpBridgeSpec> = entries
            .into_iter()
            .filter(|entry| entry.server_type != "http")
            .filter_map(|entry| {
                entry
                    .command
                    .as_ref()
                    .map(|cmd| crate::agent_backend::McpBridgeSpec {
                        name: entry.name.clone(),
                        command: PathBuf::from(cmd),
                        args: entry.args.clone(),
                    })
            })
            .collect();
        (bridges, http_count)
    }
}

/// Run the headless review-gate harness command and turn its
/// outcome into a `ReviewGateResult`. Parses the structured JSON
/// stdout on success; on non-zero exit or spawn error synthesizes a
/// failure result with the command-name-prefixed stderr / error
/// text.
fn run_review_gate_harness_command(
    agent_backend: &dyn crate::agent_backend::AgentBackend,
    rg_argv: &[String],
    wi_id: WorkItemId,
) -> ReviewGateResult {
    match std::process::Command::new(agent_backend.command_name())
        .args(rg_argv)
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            let verdict = agent_backend.parse_review_gate_stdout(&text);
            ReviewGateResult {
                work_item_id: wi_id,
                approved: verdict.approved,
                detail: verdict.detail,
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            ReviewGateResult {
                work_item_id: wi_id,
                approved: false,
                detail: format!("{}: {stderr}", agent_backend.command_name()),
            }
        }
        Err(e) => ReviewGateResult {
            work_item_id: wi_id,
            approved: false,
            detail: format!("could not run {}: {e}", agent_backend.command_name()),
        },
    }
}

/// Inputs for the review-gate background thread. Bundled so
/// `run_review_gate_thread` has a stable call shape and the
/// spawning code does not trigger clippy's `too_many_arguments`.
struct ReviewGateThreadArgs {
    tx: crossbeam_channel::Sender<ReviewGateMessage>,
    wi_id: WorkItemId,
    backend: Arc<dyn crate::work_item_backend::WorkItemBackend>,
    ws: Arc<dyn crate::worktree_service::WorktreeService + Send + Sync>,
    repo_path: PathBuf,
    branch: String,
    current_pr_number: Option<u64>,
    current_check_status: Option<CheckStatus>,
    review_skill: String,
    gate_mcp_tx: crossbeam_channel::Sender<crate::mcp::McpEvent>,
    agent_backend: Arc<dyn crate::agent_backend::AgentBackend>,
    review_extra_bridges: Vec<crate::agent_backend::McpBridgeSpec>,
}

/// Inputs for `run_review_gate_code_review`. Bundled so the helper
/// stays under clippy's `too_many_arguments` threshold.
struct ReviewGatePhase3Args<'a> {
    tx: &'a crossbeam_channel::Sender<ReviewGateMessage>,
    wi_id: WorkItemId,
    plan: &'a str,
    repo_path: &'a std::path::Path,
    default_branch: &'a str,
    branch: &'a str,
    review_skill: &'a str,
    gate_mcp_tx: crossbeam_channel::Sender<crate::mcp::McpEvent>,
    agent_backend: &'a dyn crate::agent_backend::AgentBackend,
    review_extra_bridges: &'a [crate::agent_backend::McpBridgeSpec],
}

/// Phase 3 of the review gate: start a read-only MCP server, spawn
/// the headless adversarial-review harness with the plan-in-MCP
/// wire-up, parse its verdict, and clean up the temp config file
/// and socket when done.
fn run_review_gate_code_review(args: ReviewGatePhase3Args<'_>) {
    let ReviewGatePhase3Args {
        tx,
        wi_id,
        plan,
        repo_path,
        default_branch,
        branch,
        review_skill,
        gate_mcp_tx,
        agent_backend,
        review_extra_bridges,
    } = args;

    let _ = tx.send(ReviewGateMessage::Progress(
        "Checking implementation against plan.".into(),
    ));

    let repo_path_str = repo_path.display().to_string();
    let mut vars = std::collections::HashMap::new();
    vars.insert("default_branch", default_branch);
    vars.insert("branch", branch);
    vars.insert("repo_path", repo_path_str.as_str());
    let system = crate::prompts::render("review_gate", &vars).unwrap_or_else(|| {
        "Compare plan to diff. Respond with JSON: {\"approved\": bool, \"detail\": string}".into()
    });

    // Start a temporary MCP server so `claude --print` can fetch the
    // plan via MCP tools instead of receiving it as a CLI arg
    // (which would hit the OS ARG_MAX limit on large diffs).
    // The LLM gets the diff by running `git diff` itself.
    let gate_context = serde_json::json!({
        "work_item_id": serde_json::to_string(&wi_id).unwrap_or_default(),
        "plan": plan,
    })
    .to_string();

    let gate_socket = crate::mcp::socket_path_for_session();
    let gate_server = match crate::mcp::McpSocketServer::start(
        gate_socket.clone(),
        serde_json::to_string(&wi_id).unwrap_or_default(),
        String::new(),
        gate_context,
        None,
        gate_mcp_tx,
        true, // read_only: review gate must not mutate work item state
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                work_item_id: wi_id,
                approved: false,
                detail: format!("review gate: could not start MCP server: {e}"),
            }));
            return;
        }
    };

    // Build MCP config file for --mcp-config.
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            drop(gate_server);
            let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                work_item_id: wi_id,
                approved: false,
                detail: format!("review gate: could not resolve exe path: {e}"),
            }));
            return;
        }
    };
    let mcp_config = crate::mcp::build_mcp_config(&exe_path, &gate_socket, &[]);
    let config_path = crate::side_effects::paths::temp_dir()
        .join(format!("workbridge-rg-mcp-{}.json", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&config_path, &mcp_config) {
        drop(gate_server);
        let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
            work_item_id: wi_id,
            approved: false,
            detail: format!("review gate: could not write MCP config: {e}"),
        }));
        return;
    }
    let rg_bridge = crate::agent_backend::McpBridgeSpec {
        name: "workbridge".to_string(),
        command: exe_path,
        args: vec![
            "--mcp-bridge".to_string(),
            "--socket".to_string(),
            gate_socket.to_string_lossy().into_owned(),
        ],
    };

    let json_schema = r#"{"type":"object","properties":{"approved":{"type":"boolean"},"detail":{"type":"string"}},"required":["approved","detail"]}"#;

    // Build the argv for the headless review-gate spawn via the
    // agent backend. See `docs/harness-contract.md` RP2 for the
    // Claude Code reference payload.
    let rg_cfg = ReviewGateSpawnConfig {
        system_prompt: &system,
        initial_prompt: review_skill,
        json_schema,
        mcp_config_path: &config_path,
        mcp_bridge: &rg_bridge,
        extra_bridges: review_extra_bridges,
    };
    let rg_argv = agent_backend.build_review_gate_command(&rg_cfg);

    let result = run_review_gate_harness_command(agent_backend, &rg_argv, wi_id);

    // Clean up temporary MCP server and config file.
    drop(gate_server);
    let _ = std::fs::remove_file(&config_path);

    let _ = tx.send(ReviewGateMessage::Result(result));
}

/// Read the plan text for a work item on the review-gate background
/// thread. Ships a `Blocked` message through `tx` and returns `None`
/// when the plan is missing / empty / unreadable - the outer thread
/// should return immediately in that case.
fn read_plan_or_send_blocked(
    backend: &dyn crate::work_item_backend::WorkItemBackend,
    wi_id: &WorkItemId,
    tx: &crossbeam_channel::Sender<ReviewGateMessage>,
) -> Option<String> {
    match backend.read_plan(wi_id) {
        Ok(Some(plan)) if !plan.trim().is_empty() => Some(plan),
        Ok(_) => {
            let _ = tx.send(ReviewGateMessage::Blocked {
                work_item_id: wi_id.clone(),
                reason: "Cannot enter Review: no plan exists".into(),
            });
            None
        }
        Err(e) => {
            let _ = tx.send(ReviewGateMessage::Blocked {
                work_item_id: wi_id.clone(),
                reason: format!("Could not read plan: {e}"),
            });
            None
        }
    }
}

/// Run `git diff <default>...<branch>` to confirm the feature branch
/// has changes against the base. Sends `Blocked` and returns `false`
/// when the diff is empty, the git invocation fails, or the command
/// itself can't be run.
fn verify_non_empty_diff(
    repo_path: &std::path::Path,
    default_branch: &str,
    branch: &str,
    wi_id: &WorkItemId,
    tx: &crossbeam_channel::Sender<ReviewGateMessage>,
) -> bool {
    match crate::worktree_service::git_command()
        .arg("-C")
        .arg(repo_path)
        .args(["diff", &format!("{default_branch}...{branch}")])
        .output()
    {
        Ok(output) if output.status.success() => {
            let diff = String::from_utf8_lossy(&output.stdout);
            if diff.trim().is_empty() {
                let _ = tx.send(ReviewGateMessage::Blocked {
                    work_item_id: wi_id.clone(),
                    reason: "Cannot enter Review: no changes on branch".into(),
                });
                return false;
            }
            true
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = tx.send(ReviewGateMessage::Blocked {
                work_item_id: wi_id.clone(),
                reason: format!("Review gate: git diff failed: {stderr}"),
            });
            false
        }
        Err(e) => {
            let _ = tx.send(ReviewGateMessage::Blocked {
                work_item_id: wi_id.clone(),
                reason: format!("Review gate: could not run git: {e}"),
            });
            false
        }
    }
}

/// Read the GitHub remote for a repo on the review-gate background
/// thread. Returns `Some((owner, repo, has_github_remote))` - when no
/// remote exists the booleans are `("", "", false)`. On a subprocess
/// failure ships a non-GitHub `Result { approved: false }` through
/// `tx` and returns `None` so the caller exits cleanly.
fn resolve_github_remote(
    ws: &dyn crate::worktree_service::WorktreeService,
    repo_path: &std::path::Path,
    wi_id: &WorkItemId,
    tx: &crossbeam_channel::Sender<ReviewGateMessage>,
) -> Option<(String, String, bool)> {
    match ws.github_remote(repo_path) {
        Ok(Some((o, r))) => Some((o, r, true)),
        Ok(None) => Some((String::new(), String::new(), false)),
        Err(e) => {
            let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                work_item_id: wi_id.clone(),
                approved: false,
                detail: format!("Could not read GitHub remote: {e}"),
            }));
            None
        }
    }
}
