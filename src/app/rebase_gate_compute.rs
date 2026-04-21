//! Rebase-gate compute phase, extracted from `spawn_rebase_gate`'s
//! background thread body in `impl_16.rs` so each file stays within
//! the 700-line ceiling. The labeled block `'compute: { ... }` is
//! wrapped in a function. Local mutable state (`gate_server`,
//! `config_path`, `conflicts_attempted_observed`) is owned by the
//! function and returned in `CompDone` so the calling thread body
//! can run its post-compute cleanup with those values.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use super::{RebaseGateMessage, RebaseResult, SubprocessOutcome, run_cancellable};
use crate::agent_backend::{AgentBackend, McpBridgeSpec};
use crate::mcp::{McpEvent, McpSocketServer};
use crate::work_item::WorkItemId;

pub(super) struct ComputeInputs<'a> {
    pub tx: &'a Sender<RebaseGateMessage>,
    pub wi_id_clone: &'a WorkItemId,
    pub child_pid: &'a Arc<Mutex<Option<u32>>>,
    pub cancelled: &'a Arc<AtomicBool>,
    pub agent_backend: &'a Arc<dyn AgentBackend>,
    pub rebase_extra_bridges: &'a Vec<McpBridgeSpec>,
    pub worktree_path: &'a PathBuf,
    pub branch: &'a str,
    pub base_branch: &'a str,
}

pub(super) struct CompDone {
    pub computed: Option<RebaseResult>,
    pub gate_server: Option<McpSocketServer>,
    pub config_path: Option<PathBuf>,
}

pub(super) fn do_compute(inputs: ComputeInputs<'_>) -> CompDone {
    let ComputeInputs {
        tx,
        wi_id_clone,
        child_pid,
        cancelled,
        agent_backend,
        rebase_extra_bridges,
        worktree_path,
        branch,
        base_branch: base_branch_ref,
    } = inputs;
    let base_branch: String = base_branch_ref.to_string();

    let mut gate_server: Option<McpSocketServer> = None;
    let mut config_path: Option<PathBuf> = None;
    let mut conflicts_attempted_observed = false;

    let computed: Option<RebaseResult> = 'compute: {
        // === Phase 2: git fetch origin <base> ===
        //
        // We use the explicit refspec
        // `+<base>:refs/remotes/origin/<base>` instead of the
        // shorthand `git fetch origin <base>` so the fetch is
        // guaranteed to update the remote-tracking ref the
        // harness and the verification below both consult. The
        // shorthand form relies on git's "opportunistic
        // remote-tracking branch update", which only fires when
        // the remote's configured fetch refspec covers `<base>`;
        // in repos cloned with `--single-branch` of a different
        // branch, or with a customised `[remote "origin"] fetch`
        // refspec that omits `<base>`, the shorthand would only
        // update FETCH_HEAD and `origin/<base>` could stay
        // stale, producing a false "Rebased onto origin/<base>"
        // success even though the rebase landed on an old tip.
        // The leading `+` enables non-fast-forward updates so a
        // force-pushed base branch is also handled correctly.
        let refspec = format!("+{base_branch}:refs/remotes/origin/{base_branch}");
        let _ = tx.send(RebaseGateMessage::Progress(format!(
            "Fetching origin/{base_branch}..."
        )));
        // The fetch goes through `run_cancellable` so it
        // runs in its own process group and the PID slot is
        // managed with the correct "stash first, check
        // second" ordering. See `run_cancellable` for the
        // contract and why the ordering matters.
        match run_cancellable(
            crate::worktree_service::git_command()
                .arg("-C")
                .arg(worktree_path)
                .args(["fetch", "origin", &refspec]),
            child_pid,
            cancelled,
        ) {
            Ok(SubprocessOutcome::Completed(out)) if out.status.success() => {}
            Ok(SubprocessOutcome::Completed(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                break 'compute Some(RebaseResult::Failure {
                    base_branch,
                    reason: format!("git fetch failed: {}", stderr.trim()),
                    conflicts_attempted: false,
                    activity_log_error: None,
                });
            }
            Ok(SubprocessOutcome::Cancelled) => {
                break 'compute None;
            }
            Err(e) => {
                break 'compute Some(RebaseResult::Failure {
                    base_branch,
                    reason: format!("git fetch could not run: {e}"),
                    conflicts_attempted: false,
                    activity_log_error: None,
                });
            }
        }

        // Cancellation check between phase 2 and phase 3:
        // `git fetch` is the longest blocking step in the
        // pre-spawn window, so the gate may have been cancelled
        // while we were waiting on the network. Bailing here
        // avoids starting the MCP server, writing the temp
        // config, and spawning the harness child for nothing.
        if cancelled.load(Ordering::SeqCst) {
            break 'compute None;
        }

        let _ = tx.send(RebaseGateMessage::Progress(
            "Fetched. Asking the assistant to rebase...".into(),
        ));

        // === Phase 3: launch headless harness with workbridge MCP ===
        //
        // The MCP server gets its OWN local sender/receiver pair so
        // the spawning thread can drain `workbridge_log_event` /
        // `workbridge_report_progress` calls in real time and
        // translate them into `RebaseGateMessage::Progress`. The
        // server's tx is intentionally NOT `self.mcp_tx` because
        // routing the rebase gate's progress through the main
        // dispatch loop would mix it with unrelated events and
        // require new branches in the main `McpEvent` handler.
        let (gate_mcp_tx, gate_mcp_rx) = crossbeam_channel::unbounded::<McpEvent>();
        let gate_socket = crate::mcp::socket_path_for_session();
        match crate::mcp::McpSocketServer::start(
            gate_socket.clone(),
            serde_json::to_string(&wi_id_clone).unwrap_or_default(),
            String::new(),
            serde_json::json!({
                "work_item_id": serde_json::to_string(&wi_id_clone).unwrap_or_default(),
                "repo_path": worktree_path.display().to_string(),
                "branch": branch,
                "base_branch": base_branch,
            })
            .to_string(),
            None,
            gate_mcp_tx,
            false, // read_only=false: harness must call workbridge_log_event for live progress
        ) {
            Ok(s) => {
                gate_server = Some(s);
            }
            Err(e) => {
                break 'compute Some(RebaseResult::Failure {
                    base_branch,
                    reason: format!("rebase gate: could not start MCP server: {e}"),
                    conflicts_attempted: false,
                    activity_log_error: None,
                });
            }
        }

        // Cancellation check after starting the MCP server.
        // The post-block cleanup will drop `gate_server`.
        if cancelled.load(Ordering::SeqCst) {
            break 'compute None;
        }

        let exe_path = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                break 'compute Some(RebaseResult::Failure {
                    base_branch,
                    reason: format!("rebase gate: could not resolve exe path: {e}"),
                    conflicts_attempted: false,
                    activity_log_error: None,
                });
            }
        };
        let mcp_config = crate::mcp::build_mcp_config(&exe_path, &gate_socket, &[]);
        let path = crate::side_effects::paths::temp_dir().join(format!(
            "workbridge-rebase-mcp-{}.json",
            uuid::Uuid::new_v4()
        ));
        if let Err(e) = std::fs::write(&path, &mcp_config) {
            break 'compute Some(RebaseResult::Failure {
                base_branch,
                reason: format!("rebase gate: could not write MCP config: {e}"),
                conflicts_attempted: false,
                activity_log_error: None,
            });
        }
        config_path = Some(path);
        // Structured bridge spec for Codex's per-field `-c`
        // overrides. Claude ignores it; see
        // `agent_backend::McpBridgeSpec`.
        let rebase_bridge = crate::agent_backend::McpBridgeSpec {
            name: "workbridge".to_string(),
            command: exe_path,
            args: vec![
                "--mcp-bridge".to_string(),
                "--socket".to_string(),
                gate_socket.to_string_lossy().into_owned(),
            ],
        };

        // Cancellation check immediately before spawning the
        // harness sub-thread. This is the last cheap point
        // where we can avoid spawning the harness child
        // entirely; once the sub-thread runs `Command::spawn`,
        // the harness is alive and the kill must go through
        // `child_pid`. The post-block cleanup handles
        // dropping `gate_server` and removing `config_path`.
        if cancelled.load(Ordering::SeqCst) {
            break 'compute None;
        }

        let prompt = format!(
            "You are running inside a workbridge rebase gate. Your job is to rebase \
             the current branch (`{branch}`) onto `origin/{base_branch}` in this \
             working directory and resolve any conflicts that arise.\n\n\
             Steps:\n\
             1. Run `git rebase origin/{base_branch}`.\n\
             2. If conflicts appear, inspect the conflicted files, resolve them \
                in place (preferring the semantics of `{branch}` while keeping \
                upstream changes intact), `git add` the resolved files, and run \
                `git rebase --continue`. Repeat until the rebase completes.\n\
             3. If you cannot resolve the conflicts, run `git rebase --abort` so \
                the worktree is left clean.\n\
             4. Do NOT run `git push` under any circumstances. The user will \
                push manually.\n\n\
             As you work, call the `workbridge_log_event` MCP tool with \
             `event_type='rebase_progress'` and a `payload` object containing a \
             `message` field describing what you are about to do. This streams \
             progress to the workbridge UI.\n\n\
             When you finish, respond with a single JSON object on stdout (no \
             prose) of the shape:\n\
             {{\"success\": <bool>, \"conflicts_resolved\": <bool>, \"detail\": \
             <string>}}\n\n\
             - `success` = true if the branch is now rebased onto \
             `origin/{base_branch}`.\n\
             - `conflicts_resolved` = true if you had to resolve at least one \
             conflict before finishing.\n\
             - `detail` = a human-readable one-line summary.\n\n\
             Workbridge writes its own activity log entry for the rebase \
             outcome (success or failure) after this process exits, so do NOT \
             call `workbridge_set_status` to leave a record - the work item is \
             already in `Implementing` and the activity log entry below is the \
             audit trail."
        );

        let json_schema = r#"{"type":"object","properties":{"success":{"type":"boolean"},"conflicts_resolved":{"type":"boolean"},"detail":{"type":"string"}},"required":["success","detail"]}"#;

        // Spawn the harness child in a sub-thread so we can
        // drain gate_mcp_rx for live progress events while
        // waiting for the child to exit. The `current_dir`
        // MUST be the work item's worktree path (each git
        // worktree has its own HEAD). The sub-thread uses
        // `run_cancellable` which handles process-group
        // isolation, PID stashing, and the "stash first,
        // check second" ordering contract; see the helper's
        // doc comment for the full rationale.
        let (output_tx, output_rx) = crossbeam_channel::bounded::<SubprocessOutcome>(1);
        {
            // `config_path` is unconditionally set by the
            // `config_path = Some(path)` assignment a few
            // lines above, before this block runs; the
            // `as_ref()? ... .clone()` dance lets the code
            // avoid a restriction-lint `expect()` without
            // changing behaviour (on the impossible None
            // path we just skip the spawn with an error).
            let Some(config_path) = config_path.clone() else {
                break 'compute Some(RebaseResult::Failure {
                    base_branch,
                    reason: "rebase gate: config path missing".into(),
                    conflicts_attempted: false,
                    activity_log_error: None,
                });
            };
            let worktree_path = worktree_path.clone();
            let child_pid = Arc::clone(child_pid);
            let cancelled = Arc::clone(cancelled);
            let agent_backend = Arc::clone(agent_backend);
            let bridge = rebase_bridge;
            let extra_bridges = rebase_extra_bridges.clone();
            std::thread::spawn(move || {
                let rw_cfg = crate::agent_backend::ReviewGateSpawnConfig {
                    system_prompt: "",
                    initial_prompt: &prompt,
                    json_schema,
                    mcp_config_path: &config_path,
                    mcp_bridge: &bridge,
                    extra_bridges: &extra_bridges,
                };
                let argv = agent_backend.build_headless_rw_command(&rw_cfg);
                let mut cmd = std::process::Command::new(agent_backend.command_name());
                cmd.args(&argv)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .current_dir(&worktree_path);

                match run_cancellable(&mut cmd, &child_pid, &cancelled) {
                    Ok(outcome) => {
                        let _ = output_tx.send(outcome);
                    }
                    Err(e) => {
                        // Spawn or wait failed; wrap in the
                        // Completed variant with a failed output
                        // so the outer thread sees it as a
                        // harness error.
                        let _ =
                            output_tx.send(SubprocessOutcome::Completed(std::process::Output {
                                status: std::process::ExitStatus::default(),
                                stdout: Vec::new(),
                                stderr: format!(
                                    "could not run {}: {e}",
                                    agent_backend.command_name()
                                )
                                .into_bytes(),
                            }));
                    }
                }
            });
        }

        // `conflicts_attempted_observed` is declared OUTSIDE the
        // labeled block (before the block starts) so this select-
        // loop can mutate it while still being inside the block.
        // Reset to a known state here (in case any future caller
        // factors out the block - currently this is the only
        // place that mutates it).
        let final_output = loop {
            crossbeam_channel::select! {
                recv(gate_mcp_rx) -> evt => {
                    match evt {
                        Ok(McpEvent::ReviewGateProgress { message, .. }) => {
                            let _ = tx.send(RebaseGateMessage::Progress(message));
                        }
                        Ok(McpEvent::LogEvent { event_type, payload, .. }) => {
                            if event_type == "rebase_progress" {
                                let msg = payload
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("...")
                                    .to_string();
                                if msg.to_lowercase().contains("conflict") {
                                    conflicts_attempted_observed = true;
                                }
                                let _ = tx.send(RebaseGateMessage::Progress(msg));
                            }
                        }
                        Ok(_) | Err(_) => {
                            // `Ok(_)`: other MCP events (StatusUpdate,
                            // SetPlan, SetTitle, ...) are intentionally
                            // ignored. The rebase gate writes its own
                            // activity log entry from `poll_rebase_gate`
                            // after the harness exits, so the prompt does
                            // not ask the harness to call
                            // `workbridge_set_status` and we do not
                            // forward stray events here. Forwarding would
                            // let a misbehaving harness rename the work
                            // item or overwrite its plan as a side effect
                            // of running a rebase.
                            //
                            // `Err(_)`: channel disconnected - server
                            // gone. Continue waiting for the child to
                            // exit; the output_rx arm below will fire
                            // shortly.
                        }
                    }
                }
                recv(output_rx) -> output_result => {
                    break output_result;
                }
            }
        };

        // === Phase 5: build result from harness output ===
        //
        // This is the final break of the 'compute block on
        // the harness happy path. Pre-harness early failures
        // have already broken with their own `Some(Failure)`
        // values above; cancellation paths break with `None`.
        // Cleanup of `gate_server` and `config_path` happens
        // AFTER the block, uniformly for every break path.
        // Handle the Cancelled variant from the harness
        // sub-thread: if the gate was torn down while the
        // harness was running, `run_cancellable` already
        // killed the process group and returned Cancelled.
        // Break with None so the post-block code skips the
        // audit append and result send.
        let harness_output = match final_output {
            Ok(SubprocessOutcome::Cancelled) => break 'compute None,
            Ok(SubprocessOutcome::Completed(output)) => Ok(output),
            Err(e) => Err(e),
        };

        Some(match harness_output {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                match serde_json::from_str::<serde_json::Value>(&stdout) {
                    Ok(envelope) => {
                        let structured = &envelope["structured_output"];
                        let success = structured["success"].as_bool().unwrap_or(false);
                        let conflicts_resolved =
                            structured["conflicts_resolved"].as_bool().unwrap_or(false);
                        let detail = structured["detail"].as_str().unwrap_or("").to_string();
                        if success {
                            // Verify the harness's success claim
                            // against local git state before
                            // surfacing it to the user. The harness
                            // can hallucinate, run the wrong
                            // command, or emit a stale envelope; in
                            // any of those cases the worktree's
                            // HEAD will not actually contain
                            // `origin/<base_branch>`. The
                            // user-facing-claim rule in CLAUDE.md
                            // requires that any "it happened"
                            // status that the code can verify
                            // locally MUST be verified before
                            // rendering. `git merge-base
                            // --is-ancestor A B` exits 0 iff A is
                            // an ancestor of B and 1 otherwise; any
                            // other exit is an error and is also
                            // treated as "did not land".
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
                            // Also check that no rebase is
                            // still in progress. During a
                            // conflicted rebase HEAD has
                            // already advanced past origin/
                            // <base> so the ancestry check
                            // passes, but REBASE_HEAD exists
                            // while git is waiting for
                            // conflict resolution. If the
                            // harness hallucinated success
                            // while leaving the worktree
                            // mid-rebase, this catches it.
                            let rebase_in_progress = crate::worktree_service::git_command()
                                .arg("-C")
                                .arg(worktree_path)
                                .args(["rev-parse", "--verify", "--quiet", "REBASE_HEAD"])
                                .output()
                                .is_ok_and(|o| o.status.success());
                            if ancestry_ok && !rebase_in_progress {
                                RebaseResult::Success {
                                    base_branch,
                                    conflicts_resolved,
                                    activity_log_error: None,
                                }
                            } else if !ancestry_ok {
                                RebaseResult::Failure {
                                    base_branch: base_branch.clone(),
                                    reason: format!(
                                        "harness reported success but origin/{base_branch} is not an ancestor of HEAD"
                                    ),
                                    conflicts_attempted: conflicts_resolved
                                        || conflicts_attempted_observed,
                                    activity_log_error: None,
                                }
                            } else {
                                // ancestry_ok but rebase_in_progress:
                                // REBASE_HEAD exists, meaning git is
                                // waiting for conflict resolution.
                                // The harness left the worktree
                                // mid-rebase.
                                RebaseResult::Failure {
                                        base_branch,
                                        reason: "harness reported success but a rebase is still in progress (REBASE_HEAD exists)".into(),
                                        conflicts_attempted: true,
                                        activity_log_error: None,
                                    }
                            }
                        } else {
                            RebaseResult::Failure {
                                base_branch,
                                reason: if detail.is_empty() {
                                    "harness reported failure".into()
                                } else {
                                    detail
                                },
                                conflicts_attempted: conflicts_resolved
                                    || conflicts_attempted_observed,
                                activity_log_error: None,
                            }
                        }
                    }
                    Err(e) => RebaseResult::Failure {
                        base_branch,
                        reason: format!("rebase gate: invalid JSON envelope: {e}"),
                        conflicts_attempted: conflicts_attempted_observed,
                        activity_log_error: None,
                    },
                }
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                RebaseResult::Failure {
                    base_branch,
                    reason: format!("harness exited with error: {}", stderr.trim()),
                    conflicts_attempted: conflicts_attempted_observed,
                    activity_log_error: None,
                }
            }
            Err(e) => RebaseResult::Failure {
                base_branch,
                reason: format!("rebase gate: harness thread disconnected: {e}"),
                conflicts_attempted: conflicts_attempted_observed,
                activity_log_error: None,
            },
        })
    };

    // `conflicts_attempted_observed` is an internal bookkeeping flag
    // used only inside the `'compute` block to tag the result when a
    // conflict rerun happened; the caller has no use for it after the
    // block resolves, so we do NOT return it via `CompDone`.
    let _ = conflicts_attempted_observed;

    CompDone {
        computed,
        gate_server,
        config_path,
    }
}
