//! MCP server communication via Unix domain sockets.
//!
//! Architecture:
//! - The TUI creates a Unix domain socket at startup
//! - A background thread accepts connections, reads Content-Length framed
//!   JSON-RPC messages, sends responses
//! - Tool call results are sent to the main thread via a crossbeam channel
//! - `--mcp-bridge` mode pipes stdin/stdout to/from the Unix socket
//!   (two threads, called by Claude Code as an MCP server)

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use serde_json::{Value, json};

/// An MCP event sent from the socket handler to the main TUI thread.
#[derive(Clone, Debug)]
pub enum McpEvent {
    /// Claude called workbridge_set_status.
    StatusUpdate {
        work_item_id: String,
        status: String,
        reason: String,
    },
    /// Claude called workbridge_log_event.
    LogEvent {
        work_item_id: String,
        event_type: String,
        payload: Value,
    },
    /// Claude called workbridge_set_plan.
    SetPlan { work_item_id: String, plan: String },
    /// Claude called workbridge_set_activity.
    SetActivity { work_item_id: String, working: bool },
    /// Claude called workbridge_approve_review or workbridge_request_changes.
    SubmitReview {
        work_item_id: String,
        action: String,
        comment: String,
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
    pub fn start_global(socket_path: PathBuf, context: Arc<Mutex<String>>) -> io::Result<Self> {
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let path_clone = socket_path.clone();

        std::thread::spawn(move || {
            global_accept_loop(listener, &context, &stop_clone);
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
                std::thread::sleep(std::time::Duration::from_millis(50));
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
fn global_accept_loop(listener: UnixListener, context: &Arc<Mutex<String>>, stop: &AtomicBool) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let ctx = Arc::clone(context);
                std::thread::spawn(move || {
                    handle_global_connection(stream, &ctx);
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
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
fn handle_global_connection(stream: UnixStream, context: &Arc<Mutex<String>>) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
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
        let response = handle_global_message(&msg, &ctx_snapshot);
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
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;

    while let Ok(msg) = read_message(&mut reader) {
        let response = handle_message(
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
fn read_message(reader: &mut impl BufRead) -> io::Result<Value> {
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

    // Cap allocation to 16 MB to prevent a malicious or buggy
    // Content-Length header from causing an out-of-memory condition.
    const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;
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
fn write_message(writer: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_string(msg)?;
    writeln!(writer, "{body}")?;
    writer.flush()
}

/// Handle an incoming JSON-RPC message and return an optional response.
/// Notifications (no "id" field) return None.
/// Tool call results are sent to the main thread via the crossbeam channel.
fn handle_message(
    msg: &Value,
    work_item_id: &str,
    work_item_kind: &str,
    context_json: &str,
    activity_log_path: Option<&Path>,
    tx: &Sender<McpEvent>,
    read_only: bool,
) -> Option<Value> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id");

    match method {
        "initialize" => {
            let id = id?;
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "workbridge",
                        "version": "0.1.0"
                    }
                }
            }))
        }
        "notifications/initialized" => None,
        "tools/list" => {
            let id = id?;
            let is_review_request = work_item_kind == "ReviewRequest";

            // Read-only tools available for all sessions (including
            // read-only review gate sessions).
            let mut tools = vec![
                json!({
                    "name": "workbridge_get_context",
                    "description": "Get the current context for this work item: stage, title, worktree path.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }),
                json!({
                    "name": "workbridge_query_log",
                    "description": "Read the activity log for this work item.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }),
            ];

            // Read-only sessions (e.g., review gate) get the plan tool
            // in addition to the common read-only tools above, then
            // return early - no mutating tools.
            if read_only {
                tools.push(json!({
                    "name": "workbridge_get_plan",
                    "description": "Get the implementation plan for this work item.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }));
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": tools
                    }
                }));
            }

            // Mutating tools for interactive sessions.
            tools.push(json!({
                "name": "workbridge_log_event",
                "description": "Log an event to the work item's activity log.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "event_type": {
                            "type": "string",
                            "description": "Type of event (e.g., 'note', 'milestone', 'error')"
                        },
                        "payload": {
                            "description": "Arbitrary JSON payload for the event"
                        }
                    },
                    "required": ["event_type"]
                }
            }));
            tools.push(json!({
                "name": "workbridge_set_activity",
                "description": "Signal whether you are actively working or idle. Call with working=true when starting a significant operation (running tests, building, making changes) and working=false when waiting for user input or finished. This controls a visual indicator in the TUI.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "working": {
                            "type": "boolean",
                            "description": "True if actively working, false if idle or waiting for input"
                        }
                    },
                    "required": ["working"]
                }
            }));

            if is_review_request {
                // Review request items get approve/request-changes tools
                // instead of set_status/set_plan.
                tools.push(json!({
                    "name": "workbridge_approve_review",
                    "description": "Approve the PR review. Submits your approval via GitHub and completes this review request work item.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "comment": {
                                "type": "string",
                                "description": "Optional comment to include with the approval"
                            }
                        }
                    }
                }));
                tools.push(json!({
                    "name": "workbridge_request_changes",
                    "description": "Request changes on the PR. Submits your review via GitHub and completes this review request work item.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "comment": {
                                "type": "string",
                                "description": "Comment explaining what changes are needed"
                            }
                        },
                        "required": ["comment"]
                    }
                }));
            } else {
                // Regular work items get set_status and set_plan tools.
                tools.push(json!({
                    "name": "workbridge_set_status",
                    "description": "Request a workflow stage change for the current work item. Call this when you finish implementing to signal readiness for review, or to signal that you are blocked and need user input. Done is not settable via MCP (it requires the merge gate). Note: status changes are validated asynchronously by the TUI - the request may be rejected if the transition is not allowed from the current state.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "status": {
                                "type": "string",
                                "description": "The new workflow stage. One of: Backlog, Planning, Implementing, Blocked, Review",
                                "enum": ["Backlog", "Planning", "Implementing", "Blocked", "Review"]
                            },
                            "reason": {
                                "type": "string",
                                "description": "Optional reason for the status change (shown to the user)"
                            }
                        },
                        "required": ["status"]
                    }
                }));
                tools.push(json!({
                    "name": "workbridge_set_plan",
                    "description": "Set the implementation plan for this work item. Call this when you have finalized the plan during the Planning stage.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "plan_text": {
                                "type": "string",
                                "description": "The full implementation plan text"
                            }
                        },
                        "required": ["plan_text"]
                    }
                }));
            }

            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": tools
                }
            }))
        }
        "tools/call" => {
            let id = id?;
            let params = msg.get("params")?;
            let tool_name = params.get("name")?.as_str()?;
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

            // Reject mutating tool calls in read-only mode. Even if a
            // caller somehow discovers the tool name, the call is blocked.
            if read_only {
                match tool_name {
                    "workbridge_get_context" | "workbridge_get_plan" | "workbridge_query_log" => {
                        // Allowed - fall through to normal handling.
                    }
                    _ => {
                        return Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32601,
                                "message": format!("tool '{tool_name}' is not available in read-only mode")
                            }
                        }));
                    }
                }
            }

            match tool_name {
                "workbridge_set_status" => {
                    let status = arguments
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    let reason = arguments
                        .get("reason")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");

                    let event = McpEvent::StatusUpdate {
                        work_item_id: work_item_id.to_string(),
                        status: status.to_string(),
                        reason: reason.to_string(),
                    };
                    let send_ok = tx.send(event).is_ok();

                    if send_ok {
                        let response_text = if status == "Review" {
                            format!(
                                "Status change to {status} submitted to review gate. \
                                 The status has NOT changed yet - it remains at its current stage \
                                 until the review gate approves or rejects. \
                                 Do NOT tell the user the status changed."
                            )
                        } else {
                            format!(
                                "Status change to {status} requested - pending validation by workbridge"
                            )
                        };
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": response_text
                                }]
                            }
                        }))
                    } else {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Error: TUI channel disconnected"
                                }],
                                "isError": true
                            }
                        }))
                    }
                }
                "workbridge_get_context" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": context_json
                        }]
                    }
                })),
                "workbridge_log_event" => {
                    let event_type = arguments
                        .get("event_type")
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let payload = arguments.get("payload").cloned().unwrap_or(json!(null));

                    let event = McpEvent::LogEvent {
                        work_item_id: work_item_id.to_string(),
                        event_type,
                        payload,
                    };
                    let send_ok = tx.send(event).is_ok();

                    if send_ok {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Event logged"
                                }]
                            }
                        }))
                    } else {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Error: TUI channel disconnected"
                                }],
                                "isError": true
                            }
                        }))
                    }
                }
                "workbridge_query_log" => {
                    let log_text = match activity_log_path {
                        Some(path) if path.exists() => std::fs::read_to_string(path)
                            .unwrap_or_else(|e| format!("Error reading activity log: {e}")),
                        Some(_) => "No activity log entries yet.".to_string(),
                        None => "Activity log path not configured.".to_string(),
                    };
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": log_text
                            }]
                        }
                    }))
                }
                "workbridge_set_plan" => {
                    let plan_text = arguments
                        .get("plan_text")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();

                    // Reject empty or whitespace-only plans to prevent
                    // review gate bypass (clearing plan lets set_status
                    // skip the gate since spawn_review_gate returns false
                    // for empty plans).
                    if plan_text.trim().is_empty() {
                        return Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32602,
                                "message": "plan_text must not be empty or whitespace-only"
                            }
                        }));
                    }

                    let event = McpEvent::SetPlan {
                        work_item_id: work_item_id.to_string(),
                        plan: plan_text,
                    };
                    let send_ok = tx.send(event).is_ok();

                    if send_ok {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Plan saved"
                                }]
                            }
                        }))
                    } else {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Error: TUI channel disconnected"
                                }],
                                "isError": true
                            }
                        }))
                    }
                }
                "workbridge_set_activity" => {
                    let working = arguments
                        .get("working")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);

                    let event = McpEvent::SetActivity {
                        work_item_id: work_item_id.to_string(),
                        working,
                    };
                    let send_ok = tx.send(event).is_ok();

                    if send_ok {
                        let state_text = if working { "working" } else { "idle" };
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": format!("Activity state set to {state_text}")
                                }]
                            }
                        }))
                    } else {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Error: TUI channel disconnected"
                                }],
                                "isError": true
                            }
                        }))
                    }
                }
                "workbridge_get_plan" => {
                    let ctx: Value = serde_json::from_str(context_json).unwrap_or(json!({}));
                    let plan = ctx
                        .get("plan")
                        .and_then(|v| v.as_str())
                        .unwrap_or("No plan available.");
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": plan
                            }]
                        }
                    }))
                }
                "workbridge_approve_review" | "workbridge_request_changes" => {
                    if work_item_kind != "ReviewRequest" {
                        return Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Error: review tools are only available for ReviewRequest work items"
                                }],
                                "isError": true
                            }
                        }));
                    }
                    let action = if tool_name == "workbridge_approve_review" {
                        "approve"
                    } else {
                        "request_changes"
                    };
                    let comment = arguments
                        .get("comment")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();

                    let event = McpEvent::SubmitReview {
                        work_item_id: work_item_id.to_string(),
                        action: action.to_string(),
                        comment,
                    };
                    let send_ok = tx.send(event).is_ok();

                    if send_ok {
                        let verb = if action == "approve" {
                            "Approval"
                        } else {
                            "Changes-requested review"
                        };
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": format!("{verb} submitted - pending GitHub API call. \
                                             The work item will move to Done once the review is posted.")
                                }]
                            }
                        }))
                    } else {
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": "Error: TUI channel disconnected"
                                }],
                                "isError": true
                            }
                        }))
                    }
                }
                _ => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("unknown tool: {tool_name}")
                    }
                })),
            }
        }
        _ => id.map(|id| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("unknown method: {method}")
                }
            })
        }),
    }
}

/// Handle an incoming JSON-RPC message for the global assistant.
/// Only read-only tools are available.
fn handle_global_message(msg: &Value, context_json: &str) -> Option<Value> {
    let method = msg.get("method")?.as_str()?;
    let id = msg.get("id");

    match method {
        "initialize" => {
            let id = id?;
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "workbridge-global",
                        "version": "0.1.0"
                    }
                }
            }))
        }
        "notifications/initialized" => None,
        "tools/list" => {
            let id = id?;
            let tools = vec![
                json!({
                    "name": "workbridge_list_repos",
                    "description": "List all managed repositories with their paths.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }),
                json!({
                    "name": "workbridge_list_work_items",
                    "description": "List all work items with their current status, title, associated repo, branch, and PR info.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }),
                json!({
                    "name": "workbridge_repo_info",
                    "description": "Get detailed info about a specific managed repo: worktrees, branches, open PRs.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "repo_path": {
                                "type": "string",
                                "description": "Absolute path to the repository"
                            }
                        },
                        "required": ["repo_path"]
                    }
                }),
            ];

            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": tools
                }
            }))
        }
        "tools/call" => {
            let id = id?;
            let params = msg.get("params")?;
            let tool_name = params.get("name")?.as_str()?;
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

            // Parse the dynamic context once per tool call.
            let ctx: Value = match serde_json::from_str(context_json) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("workbridge: global MCP context deserialization error: {e}");
                    json!({})
                }
            };

            match tool_name {
                "workbridge_list_repos" => {
                    let repos = ctx.get("repos").cloned().unwrap_or(json!([]));
                    let text = serde_json::to_string_pretty(&repos).unwrap_or_default();
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": text
                            }]
                        }
                    }))
                }
                "workbridge_list_work_items" => {
                    let items = ctx.get("work_items").cloned().unwrap_or(json!([]));
                    let text = serde_json::to_string_pretty(&items).unwrap_or_default();
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": text
                            }]
                        }
                    }))
                }
                "workbridge_repo_info" => {
                    let repo_path = arguments
                        .get("repo_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    // Find the matching repo in context.
                    let repo_info = ctx
                        .get("repos")
                        .and_then(|repos| repos.as_array())
                        .and_then(|arr| {
                            arr.iter().find(|r| {
                                r.get("path")
                                    .and_then(|p| p.as_str())
                                    .is_some_and(|p| p == repo_path)
                            })
                        })
                        .cloned()
                        .unwrap_or(json!({"error": "repo not found in managed repos"}));

                    let text = serde_json::to_string_pretty(&repo_info).unwrap_or_default();
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": text
                            }]
                        }
                    }))
                }
                _ => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("unknown tool: {tool_name}")
                    }
                })),
            }
        }
        _ => id.map(|id| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("unknown method: {method}")
                }
            })
        }),
    }
}

/// Run the MCP bridge mode: pipe stdin/stdout to/from the Unix socket.
/// This is what Claude Code spawns as an MCP server process.
pub fn run_bridge(socket_path: PathBuf) {
    let stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "workbridge: failed to connect to socket {}: {e}",
                socket_path.display()
            );
            std::process::exit(1);
        }
    };

    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("workbridge: failed to clone stream: {e}");
            std::process::exit(1);
        }
    };

    // Thread 1: stdin -> socket
    let mut writer = stream;
    let stdin_thread = std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut buf = [0u8; 8192];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if writer.write_all(&buf[..n]).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    // Thread 2: socket -> stdout
    let stdout_thread = std::thread::spawn(move || {
        let mut reader = BufReader::new(reader_stream);
        let stdout = io::stdout();
        let mut writer = stdout.lock();
        let mut buf = [0u8; 8192];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            if writer.write_all(&buf[..n]).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });

    let _ = stdin_thread.join();
    let _ = stdout_thread.join();
}

/// Build MCP config JSON for passing to the claude CLI.
/// Returns the JSON string for use with --mcp-config or .mcp.json.
///
/// `extra_servers` are per-repo MCP servers from the user's config. The
/// workbridge server is always inserted last so it wins over any user entry
/// with the same name.
pub fn build_mcp_config(
    exe_path: &Path,
    socket_path: &Path,
    extra_servers: &[crate::config::McpServerEntry],
) -> String {
    let mut servers = serde_json::Map::new();

    // Insert user-configured servers first.
    for entry in extra_servers {
        let mut server = serde_json::Map::new();
        if entry.server_type == "http" {
            server.insert("type".to_string(), json!("http"));
            if let Some(ref url) = entry.url {
                server.insert("url".to_string(), json!(url));
            }
        } else {
            if let Some(ref command) = entry.command {
                server.insert("command".to_string(), json!(command));
            }
            if !entry.args.is_empty() {
                server.insert("args".to_string(), json!(entry.args));
            }
            if !entry.env.is_empty() {
                server.insert("env".to_string(), json!(entry.env));
            }
        }
        servers.insert(entry.name.clone(), serde_json::Value::Object(server));
    }

    // Workbridge server is always inserted last so it cannot be overridden.
    servers.insert(
        "workbridge".to_string(),
        json!({
            "command": exe_path.to_string_lossy(),
            "args": [
                "--mcp-bridge",
                "--socket", socket_path.to_string_lossy()
            ]
        }),
    );

    let config = json!({ "mcpServers": servers });
    serde_json::to_string_pretty(&config).unwrap_or_default()
}

/// Generate the socket path for a work item session.
pub fn socket_path_for_session() -> PathBuf {
    let pid = std::process::id();
    let uuid = uuid::Uuid::new_v4();
    PathBuf::from(format!("/tmp/workbridge-mcp-{pid}-{uuid}.sock"))
}

/// Parse MCP bridge arguments from command line.
pub struct BridgeArgs {
    pub socket_path: PathBuf,
}

impl BridgeArgs {
    pub fn parse(args: &[String]) -> Option<Self> {
        let mut socket_path = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--socket" => {
                    socket_path = args.get(i + 1).map(PathBuf::from);
                    i += 2;
                }
                _ => i += 1,
            }
        }
        socket_path.map(|p| Self { socket_path: p })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    fn make_tx() -> Sender<McpEvent> {
        let (tx, _rx) = unbounded();
        tx
    }

    #[test]
    fn handle_initialize() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "clientInfo": {"name": "claude", "version": "1.0"}
            }
        });
        let resp = handle_message(&msg, "test-id", "", "{}", None, &tx, false).unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "workbridge");
    }

    #[test]
    fn handle_tools_list_non_gate_session() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let resp = handle_message(&msg, "test-id", "", "{}", None, &tx, false).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"workbridge_set_status"));
        assert!(names.contains(&"workbridge_get_context"));
        assert!(names.contains(&"workbridge_log_event"));
        assert!(names.contains(&"workbridge_query_log"));
        assert!(names.contains(&"workbridge_set_plan"));
        assert!(names.contains(&"workbridge_set_activity"));
        assert!(!names.contains(&"workbridge_get_plan"));
        assert!(
            !names.contains(&"workbridge_review_gate_result"),
            "review gate tool was removed"
        );
    }

    #[test]
    fn read_only_mode_exposes_only_read_tools() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let resp = handle_message(&msg, "test-id", "", "{}", None, &tx, true).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"workbridge_get_context"));
        assert!(names.contains(&"workbridge_query_log"));
        assert!(names.contains(&"workbridge_get_plan"));
        assert!(!names.contains(&"workbridge_set_status"));
        assert!(!names.contains(&"workbridge_set_plan"));
        assert!(!names.contains(&"workbridge_set_activity"));
        assert!(!names.contains(&"workbridge_log_event"));
    }

    #[test]
    fn read_only_mode_rejects_mutating_tool_calls() {
        let (tx, rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "workbridge_set_status",
                "arguments": {"status": "Review"}
            }
        });
        let resp = handle_message(&msg, "wi-ro", "", "{}", None, &tx, true).unwrap();
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not available in read-only mode")
        );
        assert!(
            rx.try_recv().is_err(),
            "no channel event should be sent in read-only mode"
        );
    }

    #[test]
    fn read_only_mode_allows_get_plan() {
        let tx = make_tx();
        let context = json!({"plan": "test plan"}).to_string();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "workbridge_get_plan",
                "arguments": {}
            }
        });
        let resp = handle_message(&msg, "wi-ro", "", &context, None, &tx, true).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "test plan");
    }

    #[test]
    fn handle_set_status_sends_channel_event() {
        let (tx, rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "workbridge_set_status",
                "arguments": {
                    "status": "Review",
                    "reason": "Implementation complete"
                }
            }
        });
        let resp = handle_message(&msg, "wi-123", "", "{}", None, &tx, false).unwrap();
        assert!(
            resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Review")
        );

        // Verify the event was sent via the channel.
        let event = rx.try_recv().unwrap();
        match event {
            McpEvent::StatusUpdate {
                work_item_id,
                status,
                reason,
            } => {
                assert_eq!(work_item_id, "wi-123");
                assert_eq!(status, "Review");
                assert_eq!(reason, "Implementation complete");
            }
            _ => panic!("expected StatusUpdate event"),
        }
    }

    #[test]
    fn handle_log_event_sends_channel_event() {
        let (tx, rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "workbridge_log_event",
                "arguments": {
                    "event_type": "milestone",
                    "payload": {"message": "tests passing"}
                }
            }
        });
        let resp = handle_message(&msg, "wi-456", "", "{}", None, &tx, false).unwrap();
        assert_eq!(
            resp["result"]["content"][0]["text"].as_str().unwrap(),
            "Event logged"
        );

        let event = rx.try_recv().unwrap();
        match event {
            McpEvent::LogEvent {
                work_item_id,
                event_type,
                payload,
            } => {
                assert_eq!(work_item_id, "wi-456");
                assert_eq!(event_type, "milestone");
                assert_eq!(payload["message"], "tests passing");
            }
            _ => panic!("expected LogEvent"),
        }
    }

    #[test]
    fn handle_set_plan_sends_channel_event() {
        let (tx, rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "workbridge_set_plan",
                "arguments": {
                    "plan_text": "Step 1: do the thing"
                }
            }
        });
        let resp = handle_message(&msg, "wi-789", "", "{}", None, &tx, false).unwrap();
        assert_eq!(
            resp["result"]["content"][0]["text"].as_str().unwrap(),
            "Plan saved"
        );

        let event = rx.try_recv().unwrap();
        match event {
            McpEvent::SetPlan { work_item_id, plan } => {
                assert_eq!(work_item_id, "wi-789");
                assert_eq!(plan, "Step 1: do the thing");
            }
            _ => panic!("expected SetPlan event"),
        }
    }

    #[test]
    fn handle_set_plan_rejects_empty_plan() {
        let (tx, rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "workbridge_set_plan",
                "arguments": {
                    "plan_text": "   "
                }
            }
        });
        let resp = handle_message(&msg, "wi-789", "", "{}", None, &tx, false).unwrap();
        assert!(resp.get("error").is_some(), "expected error response");
        assert_eq!(resp["error"]["code"], -32602);

        // No event should have been sent.
        assert!(rx.try_recv().is_err(), "expected no channel event");
    }

    #[test]
    fn handle_get_plan_returns_plan_from_context() {
        let tx = make_tx();
        let context = json!({"plan": "Step 1: implement\nStep 2: test"}).to_string();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "workbridge_get_plan",
                "arguments": {}
            }
        });
        let resp = handle_message(&msg, "wi-plan", "", &context, None, &tx, false).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "Step 1: implement\nStep 2: test");
    }

    #[test]
    fn handle_get_plan_returns_fallback_when_no_plan() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "workbridge_get_plan",
                "arguments": {}
            }
        });
        let resp = handle_message(&msg, "wi-plan", "", "{}", None, &tx, false).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "No plan available.");
    }

    #[test]
    fn handle_notification_returns_none() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        assert!(handle_message(&msg, "test-id", "", "{}", None, &tx, false).is_none());
    }

    #[test]
    fn handle_unknown_tool() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "nonexistent_tool",
                "arguments": {}
            }
        });
        let resp = handle_message(&msg, "test-id", "", "{}", None, &tx, false).unwrap();
        assert!(resp.get("error").is_some());
    }

    #[test]
    fn socket_server_starts_and_stops() {
        let socket_path = PathBuf::from(format!(
            "/tmp/workbridge-test-mcp-{}.sock",
            uuid::Uuid::new_v4()
        ));
        let (tx, _rx) = unbounded();

        let server = McpSocketServer::start(
            socket_path.clone(),
            "test-wi".into(),
            "".into(),
            "{}".into(),
            None,
            tx,
            false,
        )
        .expect("failed to start server");

        // Verify socket file exists.
        assert!(socket_path.exists(), "socket file should exist");

        // Stop the server.
        server.stop();
        // Give the accept thread time to exit and clean up.
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[test]
    fn bridge_args_parse() {
        let args = vec![
            "workbridge".to_string(),
            "--mcp-bridge".to_string(),
            "--socket".to_string(),
            "/tmp/test.sock".to_string(),
        ];
        let parsed = BridgeArgs::parse(&args).unwrap();
        assert_eq!(parsed.socket_path, PathBuf::from("/tmp/test.sock"));
    }

    #[test]
    fn build_mcp_config_produces_valid_json() {
        let exe = PathBuf::from("/usr/local/bin/workbridge");
        let sock = PathBuf::from("/tmp/test.sock");
        let config_str = build_mcp_config(&exe, &sock, &[]);
        let config: Value = serde_json::from_str(&config_str).unwrap();
        assert!(
            config["mcpServers"]["workbridge"]["command"]
                .as_str()
                .unwrap()
                .contains("workbridge")
        );
        let args = config["mcpServers"]["workbridge"]["args"]
            .as_array()
            .unwrap();
        assert!(args.iter().any(|a| a.as_str() == Some("--mcp-bridge")));
        assert!(args.iter().any(|a| a.as_str() == Some("--socket")));
    }

    #[test]
    fn build_mcp_config_includes_extra_servers() {
        use crate::config::McpServerEntry;
        use std::collections::BTreeMap;
        let exe = PathBuf::from("/usr/local/bin/workbridge");
        let sock = PathBuf::from("/tmp/test.sock");
        let extra = vec![McpServerEntry {
            repo: "~/Projects/myrepo".into(),
            name: "chrome-devtools".into(),
            server_type: "stdio".into(),
            command: Some("npx".into()),
            args: vec!["-y".into(), "chrome-devtools-mcp@latest".into()],
            env: BTreeMap::new(),
            url: None,
        }];
        let config_str = build_mcp_config(&exe, &sock, &extra);
        let config: Value = serde_json::from_str(&config_str).unwrap();
        // User server present.
        assert_eq!(
            config["mcpServers"]["chrome-devtools"]["command"]
                .as_str()
                .unwrap(),
            "npx"
        );
        // Workbridge still present.
        assert!(
            config["mcpServers"]["workbridge"]["command"]
                .as_str()
                .is_some()
        );
    }

    #[test]
    fn build_mcp_config_workbridge_key_always_wins() {
        use crate::config::McpServerEntry;
        use std::collections::BTreeMap;
        let exe = PathBuf::from("/usr/local/bin/workbridge");
        let sock = PathBuf::from("/tmp/test.sock");
        // User tries to register a server named "workbridge".
        let extra = vec![McpServerEntry {
            repo: "~/Projects/myrepo".into(),
            name: "workbridge".into(),
            server_type: "stdio".into(),
            command: Some("evil".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
        }];
        let config_str = build_mcp_config(&exe, &sock, &extra);
        let config: Value = serde_json::from_str(&config_str).unwrap();
        // Real workbridge server wins.
        assert!(
            config["mcpServers"]["workbridge"]["command"]
                .as_str()
                .unwrap()
                .contains("workbridge")
        );
        assert_ne!(
            config["mcpServers"]["workbridge"]["command"]
                .as_str()
                .unwrap(),
            "evil"
        );
    }

    // -----------------------------------------------------------------------
    // NDJSON framing tests
    // -----------------------------------------------------------------------

    #[test]
    fn read_message_ndjson() {
        let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"test\"}\n";
        let mut cursor = io::Cursor::new(input.as_slice());
        let msg = read_message(&mut cursor).expect("should parse NDJSON message");
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["method"], "test");
    }

    #[test]
    fn read_message_content_length() {
        let body = r#"{"jsonrpc":"2.0","method":"ping"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut cursor = io::Cursor::new(framed.into_bytes());
        let msg = read_message(&mut cursor).expect("should parse Content-Length message");
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["method"], "ping");
    }

    #[test]
    fn read_message_rejects_oversized_content_length() {
        let huge_len = 32 * 1024 * 1024; // 32 MB, above the 16 MB limit
        let framed = format!("Content-Length: {huge_len}\r\n\r\n");
        let mut cursor = io::Cursor::new(framed.into_bytes());
        let err = read_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("exceeds maximum"),
            "error should mention size limit, got: {}",
            err,
        );
    }

    #[test]
    fn write_message_produces_ndjson() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("write should succeed");
        let output = String::from_utf8(buf).unwrap();
        // NDJSON output must end with a newline.
        assert!(
            output.ends_with('\n'),
            "NDJSON output should end with newline, got: {output:?}",
        );
        // The content before the newline should be valid JSON.
        let parsed: Value =
            serde_json::from_str(output.trim()).expect("output should be valid JSON");
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    #[test]
    fn ndjson_request_response_roundtrip() {
        // Write a message to a buffer, then read it back.
        let original = json!({"jsonrpc": "2.0", "id": 42, "method": "tools/list"});
        let mut buf = Vec::new();
        write_message(&mut buf, &original).expect("write should succeed");

        let mut cursor = io::Cursor::new(buf);
        let roundtripped = read_message(&mut cursor).expect("should read back the message");
        assert_eq!(roundtripped["jsonrpc"], "2.0");
        assert_eq!(roundtripped["id"], 42);
        assert_eq!(roundtripped["method"], "tools/list");
    }

    // -----------------------------------------------------------------------
    // MCP-to-TUI integration test (Issue 6)
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_tool_call_produces_channel_event() {
        let socket_path = PathBuf::from(format!(
            "/tmp/workbridge-test-mcp-integration-{}.sock",
            uuid::Uuid::new_v4()
        ));
        let (tx, rx) = unbounded();

        let server = McpSocketServer::start(
            socket_path.clone(),
            "integration-wi".into(),
            "".into(),
            "{}".into(),
            None,
            tx,
            false,
        )
        .expect("failed to start server");

        // Give the accept loop time to start.
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Connect as a client.
        let stream =
            UnixStream::connect(&socket_path).expect("should connect to MCP socket server");
        stream
            .set_nonblocking(false)
            .expect("should set blocking mode");
        let reader_stream = stream.try_clone().expect("should clone stream");
        let mut writer = stream;
        let mut reader = BufReader::new(reader_stream);

        // Send a workbridge_set_plan tool call in NDJSON format.
        let plan_text = "Step 1: implement feature\nStep 2: add tests";
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "workbridge_set_plan",
                "arguments": {
                    "plan_text": plan_text
                }
            }
        });
        let request_str = serde_json::to_string(&request).unwrap();
        writeln!(writer, "{request_str}").expect("should write request");
        writer.flush().expect("should flush");

        // Read the response (NDJSON - one line).
        let mut response_line = String::new();
        reader
            .read_line(&mut response_line)
            .expect("should read response");
        let response: Value =
            serde_json::from_str(response_line.trim()).expect("response should be valid JSON");
        assert_eq!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or(""),
            "Plan saved",
        );

        // Check the channel received a SetPlan event.
        let event = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("should receive SetPlan event");
        match event {
            McpEvent::SetPlan { work_item_id, plan } => {
                assert_eq!(work_item_id, "integration-wi");
                assert_eq!(plan, plan_text);
            }
            other => panic!("expected SetPlan event, got: {:?}", other),
        }

        // No temp files should have been created (regression: debug logging removed).
        let tmp_entries: Vec<_> = std::fs::read_dir("/tmp")
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("workbridge-mcp-debug")
            })
            .collect();
        assert!(
            tmp_entries.is_empty(),
            "no debug temp files should exist, found: {:?}",
            tmp_entries,
        );

        server.stop();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[test]
    fn review_gate_result_tool_is_unknown() {
        let (tx, rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "workbridge_review_gate_result",
                "arguments": {
                    "approved": true,
                    "detail": "Implementation matches the plan"
                }
            }
        });
        // The review gate tool was removed (gate uses claude --print now).
        let resp = handle_message(
            &msg,
            "wi-gate",
            "",
            r#"{"stage":"ReviewGate"}"#,
            None,
            &tx,
            false,
        )
        .unwrap();
        assert!(resp.get("error").is_some());
        assert_eq!(resp["error"]["code"], -32601);
        assert!(rx.try_recv().is_err());
    }

    // -- Review gate MCP wording regression tests --

    /// Regression: set_status("Review") response must contain "NOT changed"
    /// and "review gate" to prevent Claude from telling the user the status
    /// changed when it has not (it is pending gate approval).
    #[test]
    fn set_status_review_response_contains_not_changed() {
        let (tx, _rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "workbridge_set_status",
                "arguments": {
                    "status": "Review",
                    "reason": "Implementation complete"
                }
            }
        });
        let resp = handle_message(&msg, "wi-wording", "", "{}", None, &tx, false).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("NOT changed") || text.contains("NOT"),
            "Review response must warn Claude that status has NOT changed, got: {text}",
        );
        assert!(
            text.contains("review gate"),
            "Review response must mention 'review gate', got: {text}",
        );
    }

    /// Regression: set_status("Blocked") response must NOT contain "NOT changed"
    /// since non-Review transitions are applied immediately.
    #[test]
    fn set_status_blocked_response_does_not_contain_not_changed() {
        let (tx, _rx) = unbounded();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {
                "name": "workbridge_set_status",
                "arguments": {
                    "status": "Blocked",
                    "reason": "Need user input"
                }
            }
        });
        let resp = handle_message(&msg, "wi-wording", "", "{}", None, &tx, false).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("NOT changed"),
            "Blocked response must NOT contain 'NOT changed', got: {text}",
        );
    }
}
