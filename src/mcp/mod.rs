//! MCP server communication via Unix domain sockets.
//!
//! Architecture:
//! - The TUI creates a Unix domain socket at startup
//! - A background thread accepts connections, reads Content-Length framed
//!   JSON-RPC messages, sends responses
//! - Tool call results are sent to the main thread via a crossbeam channel
//! - `--mcp-bridge` mode pipes stdin/stdout to/from the Unix socket
//!   (two threads, called by Claude Code as an MCP server)
//!
//! Module layout:
//! - `server`: per-session JSON-RPC request handler (`handle_message`)
//! - `global`: global-assistant JSON-RPC request handler
//!   (`handle_global_message`)
//! - `bridge`: `--mcp-bridge` stdin/stdout pipe, MCP config builder, socket
//!   path helper, and CLI arg parser
//!
//! This file keeps the public surface - `McpEvent`, `McpSocketServer`, and
//! the re-exports from `bridge` - plus the connection plumbing (accept
//! loops, per-connection I/O, and the NDJSON / Content-Length framing).

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use serde_json::Value;

mod bridge;
mod global;
mod server;

#[cfg(test)]
mod tests;

pub use bridge::{BridgeArgs, build_mcp_config, run_bridge, socket_path_for_session};

/// An MCP event sent from the socket handler to the main TUI thread.
#[derive(Clone, Debug)]
pub enum McpEvent {
    /// Claude called `workbridge_set_status`.
    StatusUpdate {
        work_item_id: String,
        status: String,
        reason: String,
    },
    /// Claude called `workbridge_log_event`.
    LogEvent {
        work_item_id: String,
        event_type: String,
        payload: Value,
    },
    /// Claude called `workbridge_set_plan`.
    SetPlan { work_item_id: String, plan: String },
    /// Claude called `workbridge_set_title`.
    SetTitle { work_item_id: String, title: String },
    /// Claude called `workbridge_set_activity`.
    SetActivity { work_item_id: String, working: bool },
    /// Claude called `workbridge_delete`.
    DeleteWorkItem { work_item_id: String },
    /// Claude called `workbridge_approve_review` or `workbridge_request_changes`.
    SubmitReview {
        work_item_id: String,
        action: String,
        comment: String,
    },
    /// Claude called `workbridge_report_progress` during review gate.
    ReviewGateProgress {
        work_item_id: String,
        message: String,
    },
    /// Claude called `workbridge_create_work_item` from the global assistant.
    CreateWorkItem {
        title: String,
        description: String,
        repo_path: String,
    },
}

/// Handle to the MCP socket server. Holds the socket path for cleanup
/// and a stop flag for the accept thread.
pub struct McpSocketServer {
    pub socket_path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl McpSocketServer {
    /// Start a new MCP socket server at the given path.
    /// Returns the server handle and immediately begins accepting connections
    /// on a background thread.
    ///
    /// When `read_only` is true, only read-only tools (`workbridge_get_context`,
    /// `workbridge_get_plan`, `workbridge_query_log`) are exposed. Mutating
    /// tools (`workbridge_set_status`, `workbridge_set_plan`,
    /// `workbridge_set_activity`, `workbridge_log_event`) are hidden from
    /// `tools/list` and rejected at `tools/call`. Use this for sessions that
    /// must not modify work item state (e.g., the adversarial review gate).
    pub fn start(
        socket_path: PathBuf,
        work_item_id: String,
        work_item_kind: String,
        context_json: String,
        activity_log_path: Option<PathBuf>,
        tx: Sender<McpEvent>,
        read_only: bool,
    ) -> io::Result<Self> {
        // Remove stale socket if it exists.
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        // Set non-blocking so the accept loop can check the stop flag.
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let path_clone = socket_path.clone();

        std::thread::spawn(move || {
            let cfg = SessionMcpConfig {
                work_item_id,
                work_item_kind,
                context_json,
                activity_log_path,
                read_only,
            };
            accept_loop(listener, &cfg, &tx, &stop_clone);
            // Clean up the socket file when the accept loop exits.
            let _ = std::fs::remove_file(&path_clone);
        });

        Ok(Self { socket_path, stop })
    }

    /// Start a global assistant MCP server with dynamic context.
    ///
    /// Unlike `start()`, the context is shared via `Arc<Mutex<String>>` so the
    /// main thread can refresh it periodically as repos/work items change.
    pub fn start_global(
        socket_path: PathBuf,
        context: Arc<Mutex<String>>,
        tx: Sender<McpEvent>,
    ) -> io::Result<Self> {
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let path_clone = socket_path.clone();

        std::thread::spawn(move || {
            global_accept_loop(listener, &context, &tx, &stop_clone);
            let _ = std::fs::remove_file(&path_clone);
        });

        Ok(Self { socket_path, stop })
    }

    /// Stop the server. The accept thread will exit on its next iteration.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for McpSocketServer {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Bundles per-session MCP configuration to keep function signatures short.
struct SessionMcpConfig {
    work_item_id: String,
    work_item_kind: String,
    context_json: String,
    activity_log_path: Option<PathBuf>,
    read_only: bool,
}

/// Accept loop: waits for connections and spawns a thread per connection.
/// Each connection is handled independently so that a stale health-check
/// connection cannot block subsequent real connections.
///
/// The `WouldBlock` backoff routes through `side_effects::clock::sleep`
/// rather than a raw stdlib sleep call. In production that wrapper
/// forwards to the real 50ms pause, which is what the loop wants.
/// Under `#[cfg(test)]` the same call becomes a `yield_now` plus a
/// mock-clock advance, so the accept loop spin-yields instead of
/// pausing for 50ms of real time. The socket-server smoke tests
/// (`socket_server_starts_and_stops`, `mcp_tool_call_produces_channel_event`)
/// rely on that behaviour to finish in milliseconds instead of seconds;
/// the tradeoff is that this loop consumes more CPU in tests than it
/// does in production. If that ever becomes a test-flakiness source
/// on loaded CI, replace the polling loop with a proper non-blocking
/// accept driven by a signal/eventfd the stop path writes to.
fn accept_loop(
    listener: UnixListener,
    cfg: &SessionMcpConfig,
    tx: &Sender<McpEvent>,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let wi_id = cfg.work_item_id.clone();
                let wi_kind = cfg.work_item_kind.clone();
                let ctx_json = cfg.context_json.clone();
                let act_path = cfg.activity_log_path.clone();
                let read_only = cfg.read_only;
                let tx = tx.clone();
                std::thread::spawn(move || {
                    handle_connection(
                        stream,
                        &wi_id,
                        &wi_kind,
                        &ctx_json,
                        act_path.as_deref(),
                        &tx,
                        read_only,
                    );
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                crate::side_effects::clock::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                break;
            }
        }
    }
}

/// Accept loop for the global assistant MCP server.
/// Passes the shared context mutex to each connection handler so that
/// tool calls always read the latest context (not a stale snapshot).
fn global_accept_loop(
    listener: UnixListener,
    context: &Arc<Mutex<String>>,
    tx: &Sender<McpEvent>,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let ctx = Arc::clone(context);
                let tx = tx.clone();
                std::thread::spawn(move || {
                    handle_global_connection(stream, &ctx, &tx);
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                crate::side_effects::clock::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                break;
            }
        }
    }
}

/// Handle a single global assistant MCP connection.
///
/// Accepts the shared context mutex so that each tools/call re-reads the
/// latest context rather than using a stale snapshot from connection time.
fn handle_global_connection(
    stream: UnixStream,
    context: &Arc<Mutex<String>>,
    tx: &Sender<McpEvent>,
) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;

    while let Ok(msg) = read_message(&mut reader) {
        // Re-read context on every message so tool calls see fresh data.
        let ctx_snapshot = match context.lock() {
            Ok(guard) => guard.clone(),
            Err(e) => {
                eprintln!("workbridge: global MCP context lock poisoned: {e}");
                "{}".to_string()
            }
        };
        let response = global::handle_global_message(&msg, &ctx_snapshot, tx);
        if let Some(resp) = response
            && write_message(&mut writer, &resp).is_err()
        {
            break;
        }
    }
}

/// Handle a single MCP connection (read messages, send responses).
fn handle_connection(
    stream: UnixStream,
    work_item_id: &str,
    work_item_kind: &str,
    context_json: &str,
    activity_log_path: Option<&Path>,
    tx: &Sender<McpEvent>,
    read_only: bool,
) {
    // Set the stream to blocking mode for this connection.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let Ok(reader_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;

    while let Ok(msg) = read_message(&mut reader) {
        let response = server::handle_message(
            &msg,
            work_item_id,
            work_item_kind,
            context_json,
            activity_log_path,
            tx,
            read_only,
        );
        if let Some(resp) = response
            && write_message(&mut writer, &resp).is_err()
        {
            break;
        }
    }
}

/// Read a JSON-RPC message from a reader.
///
/// Supports two framing formats:
/// - NDJSON: a line starting with `{` is parsed as a complete JSON message.
/// - Content-Length: traditional `Content-Length: N\r\n\r\n{...}` framing.
///
/// NDJSON is tried first (if the first non-empty line starts with `{`),
/// otherwise Content-Length headers are expected.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Value> {
    // Cap allocation to 16 MB to prevent a malicious or buggy
    // Content-Length header from causing an out-of-memory condition.
    const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed",
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        // NDJSON: if the line starts with `{`, treat it as a complete message.
        if trimmed.starts_with('{') {
            return serde_json::from_str(trimmed).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}"))
            });
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:")
            && let Ok(len) = val.trim().parse::<usize>()
        {
            content_length = Some(len);
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;

    if len > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Content-Length {len} exceeds maximum message size of {MAX_MESSAGE_SIZE}"),
        ));
    }

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))
}

/// Write a JSON-RPC message in NDJSON format (one JSON object per line).
pub fn write_message(writer: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_string(msg)?;
    writeln!(writer, "{body}")?;
    writer.flush()
}
