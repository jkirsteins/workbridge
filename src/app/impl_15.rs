//! Subset of `impl App` methods extracted from `src/app/mod.rs`.
//!
//! The `impl App { ... }` is split across sibling files solely to
//! keep every file within the 700-line ceiling. Methods behave
//! identically to the original single-file layout.

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent_backend::{AgentBackendKind, ReviewGateSpawnConfig};
use crate::work_item::{CheckStatus, WorkItemId};

use super::*;

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
        // silent default. See `docs/harness-contract.md` Change Log
        // 2026-04-16 and the
        // `harness_choice_applied_to_review_gate_spawn` test.
        let Some(agent_backend) = self.backend_for_work_item(wi_id) else {
            return ReviewGateSpawn::Blocked(
                    "Cannot run review gate: no harness chosen for this work item. Press c / x to pick one and re-open the session first.".into(),
                );
        };

        // Resolve per-repo MCP servers up-front (UI thread) and convert
        // them into `McpBridgeSpec` so the headless review gate can pass
        // them through to Codex via per-key `-c` overrides alongside the
        // workbridge bridge. HTTP entries are skipped because Codex's
        // `mcp_servers.<name>` schema requires command + args. R3-F-3:
        // surface the skip via a toast so the user knows why an HTTP
        // MCP server they configured is not visible to the Codex review
        // gate (would otherwise be a silent feature gap vs. Claude).
        let (review_extra_bridges, http_skipped_for_review): (
            Vec<crate::agent_backend::McpBridgeSpec>,
            usize,
        ) = {
            let repo_display = crate::config::collapse_home(&repo_path);
            let entries = self.config.mcp_servers_for_repo(&repo_display);
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
        };
        if agent_backend.kind() == AgentBackendKind::Codex && http_skipped_for_review > 0 {
            self.push_toast(format!(
                "Codex: {http_skipped_for_review} HTTP MCP server(s) skipped (Codex requires stdio)"
            ));
        }

        // Status-bar activity for the review gate. Per `docs/UI.md`
        // "Activity indicator placement", review gates are
        // system-initiated background work and must own a status-bar
        // spinner. The ID lives on `ReviewGateState` so every drop site
        // ends it via `drop_review_gate`.
        let activity = self.start_activity(format!("Running review gate for '{title}'"));
        // Clone the worktree service and backend for the background thread so
        // that `default_branch()`/`github_remote()` (which shell out to git)
        // and `read_plan()` (filesystem read) execute off the main UI thread.
        let ws = Arc::clone(&self.worktree_service);
        let backend = Arc::clone(&self.backend);

        // Spawn the review gate in a background thread with three phases:
        // 1. PR existence check (if GitHub remote exists)
        // 2. CI check wait (if checks are configured)
        // 3. Adversarial code review (headless agent spawn)
        // Unbounded rather than bounded(1): multiple Progress messages may
        // queue before the main thread polls.
        let (tx, rx) = crossbeam_channel::unbounded();
        let wi_id_clone = wi_id.clone();
        let review_skill = self.config.defaults.review_skill.clone();
        let gate_mcp_tx = self.mcp_tx.clone();

        std::thread::spawn(move || {
            // === Phase 0: Read plan, resolve default branch, confirm non-empty diff ===
            //
            // Every step here is blocking I/O (filesystem read or git
            // subprocess) so it MUST run on the background thread. On any
            // failure we send `Blocked` through the channel so the main UI
            // thread can clear the gate state and surface the reason in
            // the status bar.
            let plan = match backend.read_plan(&wi_id_clone) {
                Ok(Some(plan)) if !plan.trim().is_empty() => plan,
                Ok(_) => {
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: "Cannot enter Review: no plan exists".into(),
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: format!("Could not read plan: {e}"),
                    });
                    return;
                }
            };

            let default_branch = ws
                .default_branch(&repo_path)
                .unwrap_or_else(|_| "main".to_string());

            match crate::worktree_service::git_command()
                .arg("-C")
                .arg(&repo_path)
                .args(["diff", &format!("{default_branch}...{branch}")])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let diff = String::from_utf8_lossy(&output.stdout);
                    if diff.trim().is_empty() {
                        let _ = tx.send(ReviewGateMessage::Blocked {
                            work_item_id: wi_id_clone,
                            reason: "Cannot enter Review: no changes on branch".into(),
                        });
                        return;
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: format!("Review gate: git diff failed: {stderr}"),
                    });
                    return;
                }
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Blocked {
                        work_item_id: wi_id_clone,
                        reason: format!("Review gate: could not run git: {e}"),
                    });
                    return;
                }
            }

            // Resolve GitHub remote on this background thread (blocking I/O).
            let (gh_owner, gh_repo, has_github_remote) = match ws.github_remote(&repo_path) {
                Ok(Some((o, r))) => (o, r, true),
                Ok(None) => (String::new(), String::new(), false),
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!("Could not read GitHub remote: {e}"),
                    }));
                    return;
                }
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
                        work_item_id: wi_id_clone,
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
            if let Some(pr_num) = pr_number {
                // Determine whether CI checks are configured.
                let has_checks = match current_check_status.as_ref() {
                    Some(CheckStatus::None) => false,
                    Some(_) => true,
                    None => {
                        // No cached info - query fresh to discover.
                        !Self::fetch_pr_checks(&gh_owner, &gh_repo, pr_num).is_empty()
                    }
                };

                if has_checks {
                    if tx
                        .send(ReviewGateMessage::Progress(
                            "Waiting for CI checks...".into(),
                        ))
                        .is_err()
                    {
                        return;
                    }

                    // 30-minute timeout: 120 iterations * 15s = 1800s.
                    let max_iterations = 120u32;
                    let mut iteration = 0u32;
                    loop {
                        if iteration >= max_iterations {
                            let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                                work_item_id: wi_id_clone,
                                approved: false,
                                detail: "CI checks did not complete within 30 minutes. \
                                         Check the CI system and retry."
                                    .into(),
                            }));
                            return;
                        }
                        let checks = Self::fetch_pr_checks(&gh_owner, &gh_repo, pr_num);

                        if checks.is_empty() {
                            // Checks disappeared (race) - treat as no checks.
                            break;
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
                            let failed_names: Vec<_> =
                                failed.iter().map(|c| c.name.clone()).collect();
                            let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                                work_item_id: wi_id_clone,
                                approved: false,
                                detail: format!(
                                    "CI checks failed: {}. \
                                     Fix failures before requesting review.",
                                    failed_names.join(", ")
                                ),
                            }));
                            return;
                        }

                        if passed == total {
                            // All checks green - proceed to code review.
                            let _ = tx.send(ReviewGateMessage::Progress(format!(
                                "{passed} / {total} CI checks green. Running code review..."
                            )));
                            break;
                        }

                        // Still pending - update progress and poll again.
                        if tx
                            .send(ReviewGateMessage::Progress(format!(
                                "{passed} / {total} CI checks green"
                            )))
                            .is_err()
                        {
                            return; // Receiver dropped - gate cancelled.
                        }
                        iteration += 1;
                        crate::side_effects::clock::sleep(std::time::Duration::from_secs(15));
                    }
                }
            }

            // === Phase 3: Adversarial Code Review ===
            let _ = tx.send(ReviewGateMessage::Progress(
                "Checking implementation against plan.".into(),
            ));

            let repo_path_str = repo_path.display().to_string();
            let mut vars = std::collections::HashMap::new();
            vars.insert("default_branch", default_branch.as_str());
            vars.insert("branch", branch.as_str());
            vars.insert("repo_path", repo_path_str.as_str());
            let system = crate::prompts::render("review_gate", &vars).unwrap_or_else(|| {
                "Compare plan to diff. Respond with JSON: {\"approved\": bool, \"detail\": string}"
                    .into()
            });
            let prompt = review_skill;

            // Start a temporary MCP server so `claude --print` can fetch the
            // plan via MCP tools instead of receiving it as a CLI arg
            // (which would hit the OS ARG_MAX limit on large diffs).
            // The LLM gets the diff by running `git diff` itself.
            let gate_context = serde_json::json!({
                "work_item_id": serde_json::to_string(&wi_id_clone).unwrap_or_default(),
                "plan": plan,
            })
            .to_string();

            let gate_socket = crate::mcp::socket_path_for_session();
            let gate_mcp_tx = gate_mcp_tx;
            let gate_server = match crate::mcp::McpSocketServer::start(
                gate_socket.clone(),
                serde_json::to_string(&wi_id_clone).unwrap_or_default(),
                String::new(),
                gate_context,
                None,
                gate_mcp_tx,
                true, // read_only: review gate must not mutate work item state
            ) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(ReviewGateMessage::Result(ReviewGateResult {
                        work_item_id: wi_id_clone,
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
                        work_item_id: wi_id_clone,
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
                    work_item_id: wi_id_clone,
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
                initial_prompt: &prompt,
                json_schema,
                mcp_config_path: &config_path,
                mcp_bridge: &rg_bridge,
                extra_bridges: &review_extra_bridges,
            };
            let rg_argv = agent_backend.build_review_gate_command(&rg_cfg);

            let result = match std::process::Command::new(agent_backend.command_name())
                .args(&rg_argv)
                .output()
            {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout).to_string();
                    let verdict = agent_backend.parse_review_gate_stdout(&text);
                    ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: verdict.approved,
                        detail: verdict.detail,
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    ReviewGateResult {
                        work_item_id: wi_id_clone,
                        approved: false,
                        detail: format!("{}: {stderr}", agent_backend.command_name()),
                    }
                }
                Err(e) => ReviewGateResult {
                    work_item_id: wi_id_clone,
                    approved: false,
                    detail: format!("could not run {}: {e}", agent_backend.command_name()),
                },
            };

            // Clean up temporary MCP server and config file.
            drop(gate_server);
            let _ = std::fs::remove_file(&config_path);

            let _ = tx.send(ReviewGateMessage::Result(result));
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
    pub(super) fn drop_review_gate(&mut self, wi_id: &WorkItemId) {
        if let Some(state) = self.review_gates.remove(wi_id) {
            self.end_activity(state.activity);
        }
    }
}
