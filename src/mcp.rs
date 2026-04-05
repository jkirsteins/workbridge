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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    pub fn start(
        socket_path: PathBuf,
        work_item_id: String,
        context_json: String,
        activity_log_path: Option<PathBuf>,
        tx: Sender<McpEvent>,
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
            accept_loop(
                listener,
                &work_item_id,
                &context_json,
                activity_log_path.as_deref(),
                &tx,
                &stop_clone,
            );
            // Clean up the socket file when the accept loop exits.
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

/// Accept loop: waits for connections and handles each one.
fn accept_loop(
    listener: UnixListener,
    work_item_id: &str,
    context_json: &str,
    activity_log_path: Option<&Path>,
    tx: &Sender<McpEvent>,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                handle_connection(stream, work_item_id, context_json, activity_log_path, tx);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No pending connection - sleep briefly and retry.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => {
                // Accept failed - socket may be broken, exit the loop.
                break;
            }
        }
    }
}

/// Handle a single MCP connection (read messages, send responses).
fn handle_connection(
    stream: UnixStream,
    work_item_id: &str,
    context_json: &str,
    activity_log_path: Option<&Path>,
    tx: &Sender<McpEvent>,
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
        let response = handle_message(&msg, work_item_id, context_json, activity_log_path, tx);
        if let Some(resp) = response
            && write_message(&mut writer, &resp).is_err()
        {
            break;
        }
    }
}

/// Read a Content-Length framed JSON-RPC message from a reader.
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
        if let Some(val) = trimmed.strip_prefix("Content-Length:")
            && let Ok(len) = val.trim().parse::<usize>()
        {
            content_length = Some(len);
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))
}

/// Write a Content-Length framed JSON-RPC message to a writer.
fn write_message(writer: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_string(msg)?;
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()
}

/// Handle an incoming JSON-RPC message and return an optional response.
/// Notifications (no "id" field) return None.
/// Tool call results are sent to the main thread via the crossbeam channel.
fn handle_message(
    msg: &Value,
    work_item_id: &str,
    context_json: &str,
    activity_log_path: Option<&Path>,
    tx: &Sender<McpEvent>,
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
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "workbridge_set_status",
                            "description": "Update the workflow stage of the current work item. Call this when you finish implementing to signal completion, or to signal that you are blocked and need user input.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "status": {
                                        "type": "string",
                                        "description": "The new workflow stage. One of: Backlog, Planning, Implementing, Blocked, Review, Done",
                                        "enum": ["Backlog", "Planning", "Implementing", "Blocked", "Review", "Done"]
                                    },
                                    "reason": {
                                        "type": "string",
                                        "description": "Optional reason for the status change (shown to the user)"
                                    }
                                },
                                "required": ["status"]
                            }
                        },
                        {
                            "name": "workbridge_get_context",
                            "description": "Get the current context for this work item: stage, title, repo path.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {}
                            }
                        },
                        {
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
                        },
                        {
                            "name": "workbridge_query_log",
                            "description": "Read the activity log for this work item.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {}
                            }
                        },
                        {
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
                        }
                    ]
                }
            }))
        }
        "tools/call" => {
            let id = id?;
            let params = msg.get("params")?;
            let tool_name = params.get("name")?.as_str()?;
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

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
                        Some(json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{
                                    "type": "text",
                                    "text": format!("Status updated to {status}")
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
pub fn build_mcp_config(exe_path: &Path, socket_path: &Path) -> String {
    let config = json!({
        "mcpServers": {
            "workbridge": {
                "command": exe_path.to_string_lossy(),
                "args": [
                    "--mcp-bridge",
                    "--socket", socket_path.to_string_lossy()
                ]
            }
        }
    });
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
        let resp = handle_message(&msg, "test-id", "{}", None, &tx).unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "workbridge");
    }

    #[test]
    fn handle_tools_list() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let resp = handle_message(&msg, "test-id", "{}", None, &tx).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"workbridge_set_status"));
        assert!(names.contains(&"workbridge_get_context"));
        assert!(names.contains(&"workbridge_log_event"));
        assert!(names.contains(&"workbridge_query_log"));
        assert!(names.contains(&"workbridge_set_plan"));
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
        let resp = handle_message(&msg, "wi-123", "{}", None, &tx).unwrap();
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
        let resp = handle_message(&msg, "wi-456", "{}", None, &tx).unwrap();
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
        let resp = handle_message(&msg, "wi-789", "{}", None, &tx).unwrap();
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
    fn handle_notification_returns_none() {
        let tx = make_tx();
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        assert!(handle_message(&msg, "test-id", "{}", None, &tx).is_none());
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
        let resp = handle_message(&msg, "test-id", "{}", None, &tx).unwrap();
        assert!(resp.get("error").is_some());
    }

    #[test]
    fn socket_server_starts_and_stops() {
        let socket_path = PathBuf::from(format!(
            "/tmp/workbridge-test-mcp-{}.sock",
            uuid::Uuid::new_v4()
        ));
        let (tx, _rx) = unbounded();

        let server =
            McpSocketServer::start(socket_path.clone(), "test-wi".into(), "{}".into(), None, tx)
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
        let config_str = build_mcp_config(&exe, &sock);
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
}
