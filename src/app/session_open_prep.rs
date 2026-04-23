//! Phase 1 session-open prep worker, extracted from `session_spawn`
//! so that file stays within the 700-line ceiling.
//!
//! The entry point `run_session_open_prep_worker` is invoked from
//! `App::begin_session_open` (in the sibling `session_spawn` module).
//! All helpers here run on the spawned worker thread: they read the
//! plan via the `WorkItemBackend`, start the MCP socket server, and
//! write any backend side-car plus the temp `--mcp-config` file.
//! The result ships back to the UI thread as a
//! `SessionOpenPlanResult` via `tx`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::SessionOpenPlanResult;
use crate::mcp::McpSocketServer;
use crate::work_item::WorkItemId;

/// Inputs captured on the UI thread for the Phase 1 session-open
/// background worker. Kept as a struct so `run_session_open_prep_worker`
/// has a stable call shape as the prep pipeline evolves.
pub(super) struct SessionOpenPrepArgs {
    pub backend: Arc<dyn crate::work_item_backend::WorkItemBackend>,
    pub wi_id: WorkItemId,
    pub cwd: PathBuf,
    pub worker_cancelled: Arc<AtomicBool>,
    pub socket_path: PathBuf,
    pub wi_id_str: String,
    pub wi_kind: String,
    pub context_json: String,
    pub activity_log_path: Option<PathBuf>,
    pub mcp_tx: crossbeam_channel::Sender<crate::mcp::McpEvent>,
    pub agent_backend: Arc<dyn crate::agent_backend::AgentBackend>,
    pub repo_mcp_servers: Vec<crate::config::McpServerEntry>,
    pub worker_committed_files: Arc<Mutex<Vec<PathBuf>>>,
    pub worker_mcp_config_path: PathBuf,
}

/// Body of the Phase 1 background thread spawned by
/// `begin_session_open`. Reads the plan, starts the MCP socket
/// server, writes any backend side-car files plus the temp
/// `--mcp-config` file, and ships the result back through `tx`. Every
/// blocking step honours `worker_cancelled` so a rapid deletion /
/// drawer close does not orphan files or sockets.
pub(super) fn run_session_open_prep_worker(
    prep: SessionOpenPrepArgs,
    tx: &crossbeam_channel::Sender<SessionOpenPlanResult>,
) {
    let SessionOpenPrepArgs {
        backend,
        wi_id,
        cwd,
        worker_cancelled,
        socket_path,
        wi_id_str,
        wi_kind,
        context_json,
        activity_log_path,
        mcp_tx,
        agent_backend,
        repo_mcp_servers,
        worker_committed_files,
        worker_mcp_config_path,
    } = prep;

    // Phase A: plan read. Must stay first so the existing
    // `begin_session_open_defers_backend_read_plan_to_background_thread`
    // regression guard continues to pass (it holds a gate
    // that parks the worker until the test releases it).
    let (plan_text, read_error) = match backend.read_plan(&wi_id) {
        Ok(Some(plan)) => (plan, None),
        Ok(None) => (String::new(), None),
        Err(e) => (String::new(), Some(format!("Could not read plan: {e}"))),
    };

    // Cancellation check before any filesystem side effect.
    if worker_cancelled.load(Ordering::Acquire) {
        return;
    }

    // Phase B: start MCP socket server.
    let (server, server_error) = match McpSocketServer::start(
        socket_path,
        wi_id_str,
        wi_kind,
        context_json,
        activity_log_path,
        mcp_tx,
        false, // read_only: interactive sessions need full tool access
    ) {
        Ok(s) => (Some(s), None),
        Err(e) => (
            None,
            Some(format!(
                "MCP unavailable: failed to start socket server: {e}"
            )),
        ),
    };

    // Convert each per-repo `McpServerEntry` into an
    // `McpBridgeSpec` so Codex can emit one `-c
    // mcp_servers.<name>.*` pair per entry. Skip HTTP-transport
    // entries: Codex's `mcp_servers.<name>` schema requires
    // command + args (no `url` sub-field), so an HTTP entry
    // would produce a malformed override. Claude still sees
    // HTTP entries via the JSON written into `mcp_config_path`.
    // Skip stdio entries with no `command` (defensive against
    // hand-edited config); they cannot spawn anything.
    let extra_mcp_bridges: Vec<crate::agent_backend::McpBridgeSpec> = repo_mcp_servers
        .iter()
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

    // Phase C: write backend side-car files and the temp `--mcp-config`.
    let (written_files, mcp_config_path_out, mcp_bridge_out, mcp_config_error) =
        write_session_open_files(WriteSessionOpenFilesArgs {
            server: server.as_ref(),
            worker_cancelled: &worker_cancelled,
            agent_backend: &agent_backend,
            cwd: &cwd,
            repo_mcp_servers: &repo_mcp_servers,
            worker_committed_files: &worker_committed_files,
            worker_mcp_config_path: &worker_mcp_config_path,
        });

    let result = SessionOpenPlanResult {
        wi_id,
        cwd,
        plan_text,
        read_error,
        server,
        server_error,
        written_files,
        mcp_config_path: mcp_config_path_out,
        mcp_bridge: mcp_bridge_out,
        extra_mcp_bridges,
        mcp_config_error,
    };
    if let Err(crossbeam_channel::SendError(result)) = tx.send(result) {
        // Receiver was dropped (work item deleted or app shutting
        // down). The main thread's cancellation cleanup may have
        // run before we wrote the config, so the file might still
        // be on disk. Clean up directly since we're already on a
        // background thread.
        for path in &result.written_files {
            let _ = std::fs::remove_file(path);
        }
        if let Some(path) = &result.mcp_config_path {
            let _ = std::fs::remove_file(path);
        }
        // MCP server Drop runs here (background thread).
    }
}

/// Inputs for `write_session_open_files`. Bundled to stay under
/// clippy's `too_many_arguments` threshold.
#[derive(Clone, Copy)]
struct WriteSessionOpenFilesArgs<'a> {
    server: Option<&'a McpSocketServer>,
    worker_cancelled: &'a AtomicBool,
    agent_backend: &'a Arc<dyn crate::agent_backend::AgentBackend>,
    cwd: &'a std::path::Path,
    repo_mcp_servers: &'a [crate::config::McpServerEntry],
    worker_committed_files: &'a Arc<Mutex<Vec<PathBuf>>>,
    worker_mcp_config_path: &'a std::path::Path,
}

/// Phase C of `run_session_open_prep_worker`: resolve the
/// current-exe path, build the MCP config bytes, write any
/// backend side-car files (e.g. Claude's temp config), and write
/// the primary `--mcp-config` file at the UI-thread-committed
/// path. Returns `(written_files, mcp_config_path_out,
/// mcp_bridge_out, mcp_config_error)`.
fn write_session_open_files(
    args: WriteSessionOpenFilesArgs<'_>,
) -> (
    Vec<PathBuf>,
    Option<PathBuf>,
    Option<crate::agent_backend::McpBridgeSpec>,
    Option<String>,
) {
    let WriteSessionOpenFilesArgs {
        server,
        worker_cancelled,
        agent_backend,
        cwd,
        repo_mcp_servers,
        worker_committed_files,
        worker_mcp_config_path,
    } = args;
    let mut written_files: Vec<PathBuf> = Vec::new();
    let mut mcp_config_path_out: Option<PathBuf> = None;
    let mut mcp_bridge_out: Option<crate::agent_backend::McpBridgeSpec> = None;
    let mut mcp_config_error: Option<String> = None;
    let Some(server) = server else {
        return (
            written_files,
            mcp_config_path_out,
            mcp_bridge_out,
            mcp_config_error,
        );
    };
    if worker_cancelled.load(Ordering::Acquire) {
        return (
            written_files,
            mcp_config_path_out,
            mcp_bridge_out,
            mcp_config_error,
        );
    }
    match std::env::current_exe() {
        Ok(exe) => {
            let mcp_config =
                crate::mcp::build_mcp_config(&exe, &server.socket_path, repo_mcp_servers);
            mcp_bridge_out = Some(crate::agent_backend::McpBridgeSpec {
                name: "workbridge".to_string(),
                command: exe,
                args: vec![
                    "--mcp-bridge".to_string(),
                    "--socket".to_string(),
                    server.socket_path.to_string_lossy().into_owned(),
                ],
            });

            match agent_backend.write_session_files(cwd, &mcp_config) {
                Ok(paths) => {
                    if !paths.is_empty()
                        && let Ok(mut guard) = worker_committed_files.lock()
                    {
                        guard.extend(paths.iter().cloned());
                    }
                    written_files.extend(paths);
                }
                Err(e) => {
                    mcp_config_error = Some(format!("MCP config write error: {e}"));
                }
            }

            if !worker_cancelled.load(Ordering::Acquire) {
                match std::fs::write(worker_mcp_config_path, &mcp_config) {
                    Ok(()) => {
                        written_files.push(worker_mcp_config_path.to_path_buf());
                        mcp_config_path_out = Some(worker_mcp_config_path.to_path_buf());
                    }
                    Err(e) => {
                        if mcp_config_error.is_none() {
                            mcp_config_error = Some(format!("MCP config write error: {e}"));
                        }
                    }
                }
            }
        }
        Err(e) => {
            mcp_config_error = Some(format!("Cannot resolve executable path: {e}"));
        }
    }
    (
        written_files,
        mcp_config_path_out,
        mcp_bridge_out,
        mcp_config_error,
    )
}
