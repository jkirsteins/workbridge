//! Integration tests covering NDJSON / Content-Length framing, bridge
//! argument parsing, MCP config building, and the full socket-server
//! round trip.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crossbeam_channel::unbounded;
use serde_json::{Value, json};

use crate::mcp::{BridgeArgs, McpEvent, McpSocketServer, build_mcp_config};
// The framing helpers live on `mcp::mod` and are only `pub(super)`;
// the test module is a sibling under `mcp::tests`, so the same crate
// path reaches them.
use crate::mcp::{read_message, write_message};

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
        String::new(),
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
    crate::side_effects::clock::sleep(std::time::Duration::from_millis(200));
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
    use std::collections::BTreeMap;

    use crate::config::McpServerEntry;
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
    use std::collections::BTreeMap;

    use crate::config::McpServerEntry;
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
        "error should mention size limit, got: {err}",
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
    let parsed: Value = serde_json::from_str(output.trim()).expect("output should be valid JSON");
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
        String::new(),
        "{}".into(),
        None,
        tx,
        false,
    )
    .expect("failed to start server");

    // Give the accept loop time to start.
    crate::side_effects::clock::sleep(std::time::Duration::from_millis(100));

    // Connect as a client.
    let stream = UnixStream::connect(&socket_path).expect("should connect to MCP socket server");
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

    // Bounded-wait receive via the shared helper, which polls
    // `try_recv` on a mock-clock-driven timer instead of calling
    // the stdlib `recv_timeout` (which internally reads the real
    // monotonic clock via `Condvar::wait_timeout` and is blocked
    // by the side-effects gate). See
    // `crate::side_effects::clock::bounded_recv` for the design.
    let event = crate::side_effects::clock::bounded_recv(&rx, "MCP event channel awaiting SetPlan");
    match event {
        McpEvent::SetPlan { work_item_id, plan } => {
            assert_eq!(work_item_id, "integration-wi");
            assert_eq!(plan, plan_text);
        }
        other => panic!("expected SetPlan event, got: {other:?}"),
    }

    // No temp files should have been created (regression: debug logging removed).
    let tmp_entries: Vec<_> = std::fs::read_dir("/tmp")
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("workbridge-mcp-debug")
        })
        .collect();
    assert!(
        tmp_entries.is_empty(),
        "no debug temp files should exist, found: {tmp_entries:?}",
    );

    server.stop();
    crate::side_effects::clock::sleep(std::time::Duration::from_millis(200));
}
