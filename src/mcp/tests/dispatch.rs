//! Unit tests for the per-session `handle_message` dispatcher and the
//! `tools/call` handlers.

use crossbeam_channel::{Sender, unbounded};
use serde_json::json;

use crate::mcp::McpEvent;
use crate::mcp::server::handle_message;

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
            "clientInfo": {"name": "workbridge-test-agent", "version": "1.0"}
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
    assert_eq!(tools.len(), 8);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"workbridge_set_status"));
    assert!(names.contains(&"workbridge_get_context"));
    assert!(names.contains(&"workbridge_log_event"));
    assert!(names.contains(&"workbridge_query_log"));
    assert!(names.contains(&"workbridge_set_plan"));
    assert!(names.contains(&"workbridge_set_activity"));
    assert!(names.contains(&"workbridge_set_title"));
    assert!(names.contains(&"workbridge_delete"));
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
    assert_eq!(tools.len(), 4);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"workbridge_get_context"));
    assert!(names.contains(&"workbridge_query_log"));
    assert!(names.contains(&"workbridge_get_plan"));
    assert!(names.contains(&"workbridge_report_progress"));
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

/// Regression: `set_status("Review`") response must contain "NOT changed"
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

/// Regression: `set_status("Blocked`") response must NOT contain "NOT changed"
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

#[test]
fn review_request_session_includes_delete_tool() {
    let tx = make_tx();
    let msg = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list"
    });
    let resp = handle_message(&msg, "test-id", "ReviewRequest", "{}", None, &tx, false).unwrap();
    let tools = resp["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"workbridge_delete"),
        "workbridge_delete must be available in review request sessions"
    );
    // Verify review-specific tools are also present.
    assert!(names.contains(&"workbridge_approve_review"));
    assert!(names.contains(&"workbridge_request_changes"));
}

#[test]
fn review_request_session_accepts_delete_call() {
    // workbridge_delete is available for all non-read-only sessions,
    // including ReviewRequest items.
    let (tx, _rx) = unbounded();
    let msg = json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": "tools/call",
        "params": {
            "name": "workbridge_delete",
            "arguments": {}
        }
    });
    let resp = handle_message(&msg, "test-id", "ReviewRequest", "{}", None, &tx, false).unwrap();
    let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !is_error,
        "workbridge_delete must not return isError for ReviewRequest, got: {text}"
    );
    assert!(
        text.contains("Delete request sent"),
        "response must confirm delete was sent, got: {text}"
    );
}
