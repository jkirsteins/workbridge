//! Per-session JSON-RPC handler for the MCP socket server.
//!
//! `handle_message` dispatches incoming requests for a single work item's
//! session: `initialize`, `tools/list`, and `tools/call`. The `tools/call`
//! body lives in [`tool_calls`] to keep individual files small.

use std::path::Path;

use crossbeam_channel::Sender;
use serde_json::{Value, json};

use super::McpEvent;

mod tool_calls;

/// Handle an incoming JSON-RPC message and return an optional response.
/// Notifications (no "id" field) return None.
/// Tool call results are sent to the main thread via the crossbeam channel.
pub fn handle_message(
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
            Some(tools_list_response(id, work_item_kind, read_only))
        }
        "tools/call" => {
            let id = id?;
            let params = msg.get("params")?;
            let tool_name = params.get("name")?.as_str()?;
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            Some(tool_calls::dispatch(&tool_calls::ToolCallCtx {
                id,
                tool_name,
                arguments: &arguments,
                work_item_id,
                work_item_kind,
                context_json,
                activity_log_path,
                tx,
                read_only,
            }))
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

/// Build the `tools/list` response for a per-work-item session.
///
/// The set of tools depends on the session kind:
/// - Read-only sessions (review gate) expose only query/report tools.
/// - `ReviewRequest` sessions get approve/request-changes tools.
/// - Regular work items get `set_status` / `set_plan` / `set_title` tools.
///
/// All non-read-only sessions get the mutating activity/log/delete tools.
fn tools_list_response(id: &Value, work_item_kind: &str, read_only: bool) -> Value {
    let is_review_request = work_item_kind == "ReviewRequest";
    let (read_only_anno, mutating_anno) = tool_annotations();
    let mut tools = common_read_only_tools(&read_only_anno);

    // Read-only sessions (e.g., review gate) get the plan tool
    // in addition to the common read-only tools above, then
    // return early - no mutating tools.
    if read_only {
        push_review_gate_tools(&mut tools, &read_only_anno);
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": tools
            }
        });
    }

    push_mutating_tools(&mut tools, &mutating_anno, is_review_request);

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": tools
        }
    })
}

/// Build the MCP tool-annotation objects once per `tools_list` call.
///
/// These are load-bearing for Codex (per the MCP spec's
/// `ToolAnnotations` struct): Codex 0.120.0's
/// `requires_mcp_tool_approval` returns `false` when
/// `destructiveHint` and `openWorldHint` are both `false`, which
/// skips the "Allow the workbridge MCP server to run tool ..."
/// dialog entirely. Without these annotations Codex falls back to
/// `destructive_hint.unwrap_or(true) || open_world_hint.unwrap_or(true)`,
/// which the user has explicitly rejected.
///
/// `readOnlyHint: true` on the genuinely read-only tools is
/// factually correct and gives Codex an even stronger signal.
/// Mutating tools use `destructiveHint: false, openWorldHint: false`
/// which says "this operation stays inside workbridge's data and does
/// not reach out to the wider system" - accurate for every
/// workbridge_* tool, including `workbridge_delete` (deletes a
/// workbridge record, not a filesystem path).
fn tool_annotations() -> (Value, Value) {
    let read_only_anno: Value = json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "openWorldHint": false,
    });
    let mutating_anno: Value = json!({
        "readOnlyHint": false,
        "destructiveHint": false,
        "openWorldHint": false,
    });
    (read_only_anno, mutating_anno)
}

/// Read-only tools available for every session kind (including the
/// read-only review-gate session).
fn common_read_only_tools(read_only_anno: &Value) -> Vec<Value> {
    vec![
        json!({
            "name": "workbridge_get_context",
            "description": "Get the current context for this work item: stage, title, worktree path.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "annotations": read_only_anno,
        }),
        json!({
            "name": "workbridge_query_log",
            "description": "Read the activity log for this work item.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "annotations": read_only_anno,
        }),
    ]
}

/// Append the extra read-only tools that a review-gate session needs
/// on top of the common baseline.
fn push_review_gate_tools(tools: &mut Vec<Value>, read_only_anno: &Value) {
    tools.push(json!({
        "name": "workbridge_get_plan",
        "description": "Get the implementation plan for this work item.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        },
        "annotations": read_only_anno,
    }));
    tools.push(json!({
        "name": "workbridge_report_progress",
        "description": "Report progress on the current review. Call this to update the user on what you are doing.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Short progress message, e.g. 'Reviewing authentication changes' or 'Found 3 issues, checking severity'"
                }
            },
            "required": ["message"]
        },
        "annotations": read_only_anno,
    }));
}

/// Append the mutating tools for an interactive session. The
/// per-kind (`ReviewRequest` vs regular work item) tool set branches
/// at the end.
fn push_mutating_tools(tools: &mut Vec<Value>, mutating_anno: &Value, is_review_request: bool) {
    push_common_mutating_tools(tools, mutating_anno);
    if is_review_request {
        push_review_request_tools(tools, mutating_anno);
    } else {
        push_regular_work_item_tools(tools, mutating_anno);
    }
}

/// Baseline mutating tools every interactive session gets: activity
/// log, working-indicator signalling, and work-item deletion.
fn push_common_mutating_tools(tools: &mut Vec<Value>, mutating_anno: &Value) {
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
        },
        "annotations": mutating_anno,
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
        },
        "annotations": mutating_anno,
    }));
    tools.push(json!({
        "name": "workbridge_delete",
        "description": "Delete the current work item. This is irreversible. The backend record is deleted immediately and the session is killed. Resource cleanup (worktree removal, branch deletion, PR closure) runs asynchronously in the background.",
        "inputSchema": {
            "type": "object",
            "properties": {}
        },
        "annotations": mutating_anno,
    }));
}

/// Tools exposed only to `ReviewRequest` sessions: approve / request
/// changes, replacing the regular stage-transition tools.
fn push_review_request_tools(tools: &mut Vec<Value>, mutating_anno: &Value) {
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
        },
        "annotations": mutating_anno,
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
        },
        "annotations": mutating_anno,
    }));
}

/// Tools exposed only to regular (non-review-request) work items:
/// `set_status`, `set_plan`, `set_title`.
fn push_regular_work_item_tools(tools: &mut Vec<Value>, mutating_anno: &Value) {
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
        },
        "annotations": mutating_anno,
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
        },
        "annotations": mutating_anno,
    }));
    tools.push(json!({
        "name": "workbridge_set_title",
        "description": "Set or update the title of this work item. Call this once you understand what the user wants to work on.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "A concise title describing the work item"
                }
            },
            "required": ["title"]
        },
        "annotations": mutating_anno,
    }));
}
