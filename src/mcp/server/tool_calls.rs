//! `tools/call` handlers for per-session MCP requests.
//!
//! Dispatch lives in [`dispatch`]; each tool handler is a small helper
//! that formats the JSON-RPC response and (for mutating tools) sends a
//! matching [`McpEvent`] on the channel.

use std::path::Path;

use crossbeam_channel::Sender;
use serde_json::{Value, json};

use crate::mcp::McpEvent;

/// Context passed to each `tools/call` handler.
///
/// Bundling these fields keeps dispatcher / helper signatures short
/// (the alternative would be a 9-argument function, which triggers
/// `clippy::too_many_arguments`).
pub struct ToolCallCtx<'a> {
    pub id: &'a Value,
    pub tool_name: &'a str,
    pub arguments: &'a Value,
    pub work_item_id: &'a str,
    pub work_item_kind: &'a str,
    pub context_json: &'a str,
    pub activity_log_path: Option<&'a Path>,
    pub tx: &'a Sender<McpEvent>,
    pub read_only: bool,
}

/// Dispatch a `tools/call` request to the appropriate handler.
///
/// Enforces read-only mode by rejecting mutating tool names before the
/// call reaches its handler. See `handle_message` for the `initialize` /
/// `tools/list` branches.
pub fn dispatch(ctx: &ToolCallCtx<'_>) -> Value {
    let id = ctx.id;
    let tool_name = ctx.tool_name;
    let arguments = ctx.arguments;
    let work_item_id = ctx.work_item_id;
    let work_item_kind = ctx.work_item_kind;
    let context_json = ctx.context_json;
    let activity_log_path = ctx.activity_log_path;
    let tx = ctx.tx;
    let read_only = ctx.read_only;

    // Reject mutating tool calls in read-only mode. Even if a
    // caller somehow discovers the tool name, the call is blocked.
    if read_only {
        match tool_name {
            "workbridge_get_context"
            | "workbridge_get_plan"
            | "workbridge_query_log"
            | "workbridge_report_progress" => {
                // Allowed - fall through to normal handling.
            }
            _ => {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": format!("tool '{tool_name}' is not available in read-only mode")
                    }
                });
            }
        }
    }

    match tool_name {
        "workbridge_set_status" => set_status(id, arguments, work_item_id, tx),
        "workbridge_get_context" => get_context(id, context_json),
        "workbridge_log_event" => log_event(id, arguments, work_item_id, tx),
        "workbridge_query_log" => query_log(id, activity_log_path),
        "workbridge_report_progress" => report_progress(id, arguments, work_item_id, tx),
        "workbridge_set_plan" => set_plan(id, arguments, work_item_id, tx),
        "workbridge_set_title" => set_title(id, arguments, work_item_id, tx),
        "workbridge_set_activity" => set_activity(id, arguments, work_item_id, tx),
        "workbridge_delete" => delete_work_item(id, work_item_id, tx),
        "workbridge_get_plan" => get_plan(id, context_json),
        "workbridge_approve_review" | "workbridge_request_changes" => {
            submit_review(id, tool_name, arguments, work_item_id, work_item_kind, tx)
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("unknown tool: {tool_name}")
            }
        }),
    }
}

fn channel_error(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{
                "type": "text",
                "text": "Error: TUI channel disconnected"
            }],
            "isError": true
        }
    })
}

fn text_result(id: &Value, text: impl Into<String>) -> Value {
    let text = text.into();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{
                "type": "text",
                "text": text
            }]
        }
    })
}

fn set_status(id: &Value, arguments: &Value, work_item_id: &str, tx: &Sender<McpEvent>) -> Value {
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
    if tx.send(event).is_err() {
        return channel_error(id);
    }

    let response_text = if status == "Review" {
        format!(
            "Status change to {status} submitted to review gate. \
             The status has NOT changed yet - it remains at its current stage \
             until the review gate approves or rejects. \
             Do NOT tell the user the status changed."
        )
    } else {
        format!("Status change to {status} requested - pending validation by workbridge")
    };
    text_result(id, response_text)
}

fn get_context(id: &Value, context_json: &str) -> Value {
    text_result(id, context_json)
}

fn log_event(id: &Value, arguments: &Value, work_item_id: &str, tx: &Sender<McpEvent>) -> Value {
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
    if tx.send(event).is_err() {
        return channel_error(id);
    }
    text_result(id, "Event logged")
}

fn query_log(id: &Value, activity_log_path: Option<&Path>) -> Value {
    let log_text = match activity_log_path {
        Some(path) if path.exists() => std::fs::read_to_string(path)
            .unwrap_or_else(|e| format!("Error reading activity log: {e}")),
        Some(_) => "No activity log entries yet.".to_string(),
        None => "Activity log path not configured.".to_string(),
    };
    text_result(id, log_text)
}

fn report_progress(
    id: &Value,
    arguments: &Value,
    work_item_id: &str,
    tx: &Sender<McpEvent>,
) -> Value {
    let message = arguments
        .get("message")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let _ = tx.send(McpEvent::ReviewGateProgress {
        work_item_id: work_item_id.to_string(),
        message,
    });
    text_result(id, "Progress reported.")
}

fn set_plan(id: &Value, arguments: &Value, work_item_id: &str, tx: &Sender<McpEvent>) -> Value {
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
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32602,
                "message": "plan_text must not be empty or whitespace-only"
            }
        });
    }

    let event = McpEvent::SetPlan {
        work_item_id: work_item_id.to_string(),
        plan: plan_text,
    };
    if tx.send(event).is_err() {
        return channel_error(id);
    }
    text_result(id, "Plan saved")
}

fn set_title(id: &Value, arguments: &Value, work_item_id: &str, tx: &Sender<McpEvent>) -> Value {
    let title = arguments
        .get("title")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    if title.trim().is_empty() {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32602,
                "message": "title must not be empty or whitespace-only"
            }
        });
    }

    let event = McpEvent::SetTitle {
        work_item_id: work_item_id.to_string(),
        title,
    };
    if tx.send(event).is_err() {
        return channel_error(id);
    }
    text_result(id, "Title updated")
}

fn set_activity(id: &Value, arguments: &Value, work_item_id: &str, tx: &Sender<McpEvent>) -> Value {
    let working = arguments
        .get("working")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let event = McpEvent::SetActivity {
        work_item_id: work_item_id.to_string(),
        working,
    };
    if tx.send(event).is_err() {
        return channel_error(id);
    }
    let state_text = if working { "working" } else { "idle" };
    text_result(id, format!("Activity state set to {state_text}"))
}

fn delete_work_item(id: &Value, work_item_id: &str, tx: &Sender<McpEvent>) -> Value {
    let event = McpEvent::DeleteWorkItem {
        work_item_id: work_item_id.to_string(),
    };
    if tx.send(event).is_err() {
        return channel_error(id);
    }
    text_result(
        id,
        "Delete request sent to TUI. The backend record will be deleted and the session killed \
         on the next event loop tick. Resource cleanup (worktree removal, branch deletion, PR \
         closure) runs asynchronously in the background. This session will be terminated.",
    )
}

fn get_plan(id: &Value, context_json: &str) -> Value {
    let ctx: Value = serde_json::from_str(context_json).unwrap_or_else(|_| json!({}));
    let plan = ctx
        .get("plan")
        .and_then(|v| v.as_str())
        .unwrap_or("No plan available.");
    text_result(id, plan.to_string())
}

fn submit_review(
    id: &Value,
    tool_name: &str,
    arguments: &Value,
    work_item_id: &str,
    work_item_kind: &str,
    tx: &Sender<McpEvent>,
) -> Value {
    if work_item_kind != "ReviewRequest" {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{
                    "type": "text",
                    "text": "Error: review tools are only available for ReviewRequest work items"
                }],
                "isError": true
            }
        });
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
    if tx.send(event).is_err() {
        return channel_error(id);
    }

    let verb = if action == "approve" {
        "Approval"
    } else {
        "Changes-requested review"
    };
    text_result(
        id,
        format!(
            "{verb} submitted - pending GitHub API call. \
             The work item will move to Done once the review is posted."
        ),
    )
}
