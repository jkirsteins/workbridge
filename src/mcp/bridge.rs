//! MCP bridge entry point, config builder, and socket-path helpers.
//!
//! - `run_bridge`: the `--mcp-bridge` mode invoked by Claude Code; it
//!   forwards stdin/stdout to a Unix domain socket the TUI is listening
//!   on.
//! - `build_mcp_config`: builds the JSON passed to the agent backend
//!   via `--mcp-config`, pinning the workbridge entry so it always wins.
//! - `socket_path_for_session`: allocates a uniquely-named socket path
//!   under `/tmp` for a new session. The per-session socket under
//!   `/tmp` is an authorized test exception per CLAUDE.md (the MCP
//!   bridge binds a UUID-suffixed Unix socket under `/tmp` in tests).
//! - `BridgeArgs`: argv parser for `--mcp-bridge --socket <path>`.

use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde_json::json;

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
                Ok(0) | Err(_) => break,
                Ok(n) => n,
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
                Ok(0) | Err(_) => break,
                Ok(n) => n,
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

/// Build MCP config JSON for passing to an agent backend (today:
/// Claude Code via `--mcp-config`). Returns the JSON string; the
/// caller writes it to a temp file and passes the path as a CLI flag.
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
