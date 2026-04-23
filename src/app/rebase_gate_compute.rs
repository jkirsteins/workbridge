//! Rebase-gate compute phase, extracted from `spawn_rebase_gate`'s
//! background thread body (`App::spawn_rebase_gate` in the sibling
//! `rebase_gate_spawn` module) so each file stays within the
//! 700-line ceiling. The labeled block `'compute: { ... }` is
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

#[derive(Clone, Copy)]
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
        match fetch_base_branch(tx, child_pid, cancelled, worktree_path, &base_branch) {
            FetchOutcome::Ok => {}
            FetchOutcome::Cancelled => break 'compute None,
            FetchOutcome::Failure(result) => break 'compute Some(result),
        }

        // `git fetch` is the longest blocking step in the pre-spawn
        // window; bail here if the gate was cancelled during it.
        if cancelled.load(Ordering::SeqCst) {
            break 'compute None;
        }

        let _ = tx.send(RebaseGateMessage::Progress(
            "Fetched. Asking the assistant to rebase...".into(),
        ));

        // === Phase 3: launch headless harness with workbridge MCP ===
        let (gate_mcp_tx, gate_mcp_rx) = crossbeam_channel::unbounded::<McpEvent>();
        let gate_socket = crate::mcp::socket_path_for_session();
        let rebase_bridge = match setup_rebase_gate_server_and_config(
            &gate_socket,
            wi_id_clone,
            worktree_path,
            branch,
            &base_branch,
            gate_mcp_tx,
            cancelled,
        ) {
            SetupOutcome::Ok {
                server,
                path,
                bridge,
            } => {
                gate_server = Some(server);
                config_path = Some(path);
                bridge
            }
            SetupOutcome::Cancelled => break 'compute None,
            SetupOutcome::Failed { server, reason } => {
                gate_server = server;
                break 'compute Some(RebaseResult::Failure {
                    base_branch,
                    reason,
                    conflicts_attempted: false,
                    activity_log_error: None,
                });
            }
        };

        // Last cheap cancellation point before `Command::spawn`.
        if cancelled.load(Ordering::SeqCst) {
            break 'compute None;
        }

        let Some(owned_config_path) = config_path.clone() else {
            break 'compute Some(RebaseResult::Failure {
                base_branch,
                reason: "rebase gate: config path missing".into(),
                conflicts_attempted: false,
                activity_log_error: None,
            });
        };
        match spawn_and_collect_harness_result(SpawnAndCollectArgs {
            owned_config_path,
            worktree_path,
            child_pid,
            cancelled,
            agent_backend,
            rebase_bridge,
            rebase_extra_bridges,
            branch,
            base_branch: &base_branch,
            gate_mcp_rx: &gate_mcp_rx,
            tx,
            conflicts_attempted_observed: &mut conflicts_attempted_observed,
        }) {
            Some(result) => Some(result),
            None => break 'compute None,
        }
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

/// Inputs for `spawn_and_collect_harness_result`. Bundled so the
/// helper stays under clippy's `too_many_arguments` threshold.
struct SpawnAndCollectArgs<'a> {
    owned_config_path: PathBuf,
    worktree_path: &'a PathBuf,
    child_pid: &'a Arc<std::sync::Mutex<Option<u32>>>,
    cancelled: &'a Arc<std::sync::atomic::AtomicBool>,
    agent_backend: &'a Arc<dyn crate::agent_backend::AgentBackend>,
    rebase_bridge: crate::agent_backend::McpBridgeSpec,
    rebase_extra_bridges: &'a [crate::agent_backend::McpBridgeSpec],
    branch: &'a str,
    base_branch: &'a str,
    gate_mcp_rx: &'a crossbeam_channel::Receiver<McpEvent>,
    tx: &'a crossbeam_channel::Sender<RebaseGateMessage>,
    conflicts_attempted_observed: &'a mut bool,
}

/// Spawn the harness sub-thread, drain progress events while it
/// runs, and build the final `RebaseResult` from its output. Returns
/// `None` when the gate was cancelled mid-harness (the caller should
/// break with `None` to honour the C10 cancellation contract).
fn spawn_and_collect_harness_result(args: SpawnAndCollectArgs<'_>) -> Option<RebaseResult> {
    let SpawnAndCollectArgs {
        owned_config_path,
        worktree_path,
        child_pid,
        cancelled,
        agent_backend,
        rebase_bridge,
        rebase_extra_bridges,
        branch,
        base_branch,
        gate_mcp_rx,
        tx,
        conflicts_attempted_observed,
    } = args;
    let (output_tx, output_rx) = crossbeam_channel::bounded::<SubprocessOutcome>(1);
    spawn_rebase_harness_child(RebaseHarnessSpawnArgs {
        output_tx,
        config_path: owned_config_path,
        worktree_path: worktree_path.clone(),
        child_pid: Arc::clone(child_pid),
        cancelled: Arc::clone(cancelled),
        agent_backend: Arc::clone(agent_backend),
        bridge: rebase_bridge,
        extra_bridges: rebase_extra_bridges.to_vec(),
        prompt: rebase_gate_harness_prompt(branch, base_branch),
        json_schema: r#"{"type":"object","properties":{"success":{"type":"boolean"},"conflicts_resolved":{"type":"boolean"},"detail":{"type":"string"}},"required":["success","detail"]}"#,
    });

    let final_output = drain_gate_mcp_until_harness_exits(
        gate_mcp_rx,
        &output_rx,
        tx,
        conflicts_attempted_observed,
    );

    let harness_output = match final_output {
        Ok(SubprocessOutcome::Cancelled) => return None,
        Ok(SubprocessOutcome::Completed(output)) => Ok(output),
        Err(e) => Err(e),
    };

    Some(super::rebase_gate_result::build_rebase_result_from_output(
        harness_output,
        worktree_path,
        base_branch,
        *conflicts_attempted_observed,
    ))
}

/// Outcome of `setup_rebase_gate_server_and_config`.
enum SetupOutcome {
    /// Server running, temp config written, bridge spec ready.
    Ok {
        server: McpSocketServer,
        path: PathBuf,
        bridge: crate::agent_backend::McpBridgeSpec,
    },
    /// Gate cancelled between phases.
    Cancelled,
    /// Server or config setup failed. `server` may still hold the
    /// running server (the caller is responsible for dropping it in
    /// the post-block cleanup) when the failure happened AFTER the
    /// server started.
    Failed {
        server: Option<McpSocketServer>,
        reason: String,
    },
}

/// Start the rebase-gate MCP server, check for cancellation, then
/// prepare the temp `--mcp-config` file and bridge spec. Bundles the
/// three steps so the caller's `'compute` block has one
/// `break`-worthy branch instead of three.
fn setup_rebase_gate_server_and_config(
    gate_socket: &std::path::Path,
    wi_id_clone: &crate::work_item::WorkItemId,
    worktree_path: &std::path::Path,
    branch: &str,
    base_branch: &str,
    gate_mcp_tx: crossbeam_channel::Sender<McpEvent>,
    cancelled: &std::sync::atomic::AtomicBool,
) -> SetupOutcome {
    let server = match start_rebase_gate_mcp_server(
        gate_socket,
        wi_id_clone,
        worktree_path,
        branch,
        base_branch,
        gate_mcp_tx,
    ) {
        Ok(s) => s,
        Err(reason) => {
            return SetupOutcome::Failed {
                server: None,
                reason,
            };
        }
    };

    if cancelled.load(Ordering::SeqCst) {
        drop(server);
        return SetupOutcome::Cancelled;
    }

    match prepare_rebase_gate_config(gate_socket) {
        Ok((_exe, path, bridge)) => SetupOutcome::Ok {
            server,
            path,
            bridge,
        },
        Err(reason) => SetupOutcome::Failed {
            server: Some(server),
            reason,
        },
    }
}

/// Inputs for `spawn_rebase_harness_child`. Bundled so the helper
/// stays under clippy's `too_many_arguments` threshold and so the
/// compute thread can hand off ownership in one move.
struct RebaseHarnessSpawnArgs {
    output_tx: crossbeam_channel::Sender<SubprocessOutcome>,
    config_path: PathBuf,
    worktree_path: PathBuf,
    child_pid: Arc<std::sync::Mutex<Option<u32>>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    agent_backend: Arc<dyn crate::agent_backend::AgentBackend>,
    bridge: crate::agent_backend::McpBridgeSpec,
    extra_bridges: Vec<crate::agent_backend::McpBridgeSpec>,
    prompt: String,
    json_schema: &'static str,
}

/// Spawn the rebase-gate harness child in a sub-thread so the outer
/// compute thread can keep draining `gate_mcp_rx` for live progress
/// events while waiting for the child to exit. The `current_dir`
/// MUST be the work item's worktree path (each git worktree has its
/// own HEAD). Routes through `run_cancellable` which handles
/// process-group isolation, PID stashing, and the "stash first,
/// check second" ordering contract.
fn spawn_rebase_harness_child(args: RebaseHarnessSpawnArgs) {
    let RebaseHarnessSpawnArgs {
        output_tx,
        config_path,
        worktree_path,
        child_pid,
        cancelled,
        agent_backend,
        bridge,
        extra_bridges,
        prompt,
        json_schema,
    } = args;
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
                // Spawn or wait failed; wrap in the Completed
                // variant with a failed output so the outer thread
                // sees it as a harness error.
                let _ = output_tx.send(SubprocessOutcome::Completed(std::process::Output {
                    status: std::process::ExitStatus::default(),
                    stdout: Vec::new(),
                    stderr: format!("could not run {}: {e}", agent_backend.command_name())
                        .into_bytes(),
                }));
            }
        }
    });
}

/// Drain `gate_mcp_rx` for live progress events while waiting for the
/// harness child to exit on `output_rx`. Forwards `ReviewGateProgress`
/// / `rebase_progress` `LogEvent`s to `tx`, flips
/// `conflicts_attempted_observed` when a progress message mentions a
/// conflict, and silently ignores other MCP events so a misbehaving
/// harness cannot rename the work item or overwrite its plan via a
/// stray call.
fn drain_gate_mcp_until_harness_exits(
    gate_mcp_rx: &crossbeam_channel::Receiver<McpEvent>,
    output_rx: &crossbeam_channel::Receiver<SubprocessOutcome>,
    tx: &crossbeam_channel::Sender<RebaseGateMessage>,
    conflicts_attempted_observed: &mut bool,
) -> Result<SubprocessOutcome, crossbeam_channel::RecvError> {
    loop {
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
                                *conflicts_attempted_observed = true;
                            }
                            let _ = tx.send(RebaseGateMessage::Progress(msg));
                        }
                    }
                    Ok(_) | Err(_) => {
                        // `Ok(_)`: other MCP events (StatusUpdate, SetPlan,
                        // SetTitle, ...) are intentionally ignored. The
                        // rebase gate writes its own activity log entry from
                        // `poll_rebase_gate` after the harness exits, so the
                        // prompt does not ask the harness to call
                        // `workbridge_set_status` and we do not forward
                        // stray events here. Forwarding would let a
                        // misbehaving harness rename the work item or
                        // overwrite its plan as a side effect of running a
                        // rebase.
                        //
                        // `Err(_)`: channel disconnected - server gone.
                        // Continue waiting for the child to exit; the
                        // output_rx arm below will fire shortly.
                    }
                }
            }
            recv(output_rx) -> output_result => {
                break output_result;
            }
        }
    }
}

/// Start the MCP socket server the rebase-gate harness talks to.
/// The server gets its OWN local sender so the compute thread can
/// drain `workbridge_log_event` / `workbridge_report_progress`
/// calls in real time and translate them into
/// `RebaseGateMessage::Progress`. The server's tx is intentionally
/// NOT `self.mcp_tx`: routing the rebase gate's progress through
/// the main dispatch loop would mix it with unrelated events and
/// require new branches in the main `McpEvent` handler.
fn start_rebase_gate_mcp_server(
    gate_socket: &std::path::Path,
    wi_id_clone: &crate::work_item::WorkItemId,
    worktree_path: &std::path::Path,
    branch: &str,
    base_branch: &str,
    gate_mcp_tx: crossbeam_channel::Sender<McpEvent>,
) -> Result<McpSocketServer, String> {
    crate::mcp::McpSocketServer::start(
        gate_socket.to_path_buf(),
        serde_json::to_string(wi_id_clone).unwrap_or_default(),
        String::new(),
        serde_json::json!({
            "work_item_id": serde_json::to_string(wi_id_clone).unwrap_or_default(),
            "repo_path": worktree_path.display().to_string(),
            "branch": branch,
            "base_branch": base_branch,
        })
        .to_string(),
        None,
        gate_mcp_tx,
        false, // read_only=false: harness must call workbridge_log_event for live progress
    )
    .map_err(|e| format!("rebase gate: could not start MCP server: {e}"))
}

/// Build the MCP config JSON, write it to a per-call tempfile under
/// `$TMPDIR`, and return `(exe_path, config_path, bridge_spec)`.
/// The bridge spec is used by Codex's per-field `-c mcp_servers.*`
/// overrides; Claude reads the same data via the JSON file.
fn prepare_rebase_gate_config(
    gate_socket: &std::path::Path,
) -> Result<(PathBuf, PathBuf, crate::agent_backend::McpBridgeSpec), String> {
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("rebase gate: could not resolve exe path: {e}"))?;
    let mcp_config = crate::mcp::build_mcp_config(&exe_path, gate_socket, &[]);
    let path = crate::side_effects::paths::temp_dir().join(format!(
        "workbridge-rebase-mcp-{}.json",
        uuid::Uuid::new_v4()
    ));
    if let Err(e) = std::fs::write(&path, &mcp_config) {
        return Err(format!("rebase gate: could not write MCP config: {e}"));
    }
    let rebase_bridge = crate::agent_backend::McpBridgeSpec {
        name: "workbridge".to_string(),
        command: exe_path.clone(),
        args: vec![
            "--mcp-bridge".to_string(),
            "--socket".to_string(),
            gate_socket.to_string_lossy().into_owned(),
        ],
    };
    Ok((exe_path, path, rebase_bridge))
}

/// Build the headless-harness prompt for the rebase gate. The prompt
/// tells the agent to rebase `branch` onto `origin/base_branch`,
/// resolve conflicts in place, abort on give-up, and never push;
/// finally emit a single JSON envelope
/// `{success, conflicts_resolved, detail}` on stdout.
fn rebase_gate_harness_prompt(branch: &str, base_branch: &str) -> String {
    format!(
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
    )
}

/// Outcome of the Phase 2 git-fetch step in `do_compute`.
enum FetchOutcome {
    /// Fetch succeeded and the compute pipeline should continue.
    Ok,
    /// Gate was cancelled mid-fetch. The caller should break with
    /// `None` so the cancellation contract (no audit, no send) holds.
    Cancelled,
    /// Fetch failed in a non-cancellation way. The embedded
    /// `RebaseResult::Failure` gets broken with `Some(...)` so the
    /// caller still appends the audit log entry.
    Failure(RebaseResult),
}

/// Phase 2 of `do_compute`: `git fetch origin +<base>:refs/remotes/origin/<base>`.
///
/// The explicit refspec (leading `+`, full destination ref) is
/// preferred over the shorthand `git fetch origin <base>` so the
/// fetch is guaranteed to update the remote-tracking ref the harness
/// and the post-rebase verification both consult. The shorthand
/// form relies on git's "opportunistic remote-tracking branch
/// update", which only fires when the remote's configured fetch
/// refspec covers `<base>`; in repos cloned with `--single-branch`
/// of a different branch, or with a customised
/// `[remote "origin"] fetch` refspec that omits `<base>`, the
/// shorthand would only update `FETCH_HEAD` and `origin/<base>` could
/// stay stale, producing a false success even though the rebase
/// landed on an old tip. The leading `+` enables non-fast-forward
/// updates so a force-pushed base branch is also handled correctly.
///
/// The fetch goes through `run_cancellable` so it runs in its own
/// process group and the PID slot is managed with the correct
/// "stash first, check second" ordering.
fn fetch_base_branch(
    tx: &crossbeam_channel::Sender<RebaseGateMessage>,
    child_pid: &Arc<std::sync::Mutex<Option<u32>>>,
    cancelled: &std::sync::atomic::AtomicBool,
    worktree_path: &std::path::Path,
    base_branch: &str,
) -> FetchOutcome {
    let refspec = format!("+{base_branch}:refs/remotes/origin/{base_branch}");
    let _ = tx.send(RebaseGateMessage::Progress(format!(
        "Fetching origin/{base_branch}..."
    )));
    match run_cancellable(
        crate::worktree_service::git_command()
            .arg("-C")
            .arg(worktree_path)
            .args(["fetch", "origin", &refspec]),
        child_pid,
        cancelled,
    ) {
        Ok(SubprocessOutcome::Completed(out)) if out.status.success() => FetchOutcome::Ok,
        Ok(SubprocessOutcome::Completed(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            FetchOutcome::Failure(RebaseResult::Failure {
                base_branch: base_branch.to_string(),
                reason: format!("git fetch failed: {}", stderr.trim()),
                conflicts_attempted: false,
                activity_log_error: None,
            })
        }
        Ok(SubprocessOutcome::Cancelled) => FetchOutcome::Cancelled,
        Err(e) => FetchOutcome::Failure(RebaseResult::Failure {
            base_branch: base_branch.to_string(),
            reason: format!("git fetch could not run: {e}"),
            conflicts_attempted: false,
            activity_log_error: None,
        }),
    }
}
