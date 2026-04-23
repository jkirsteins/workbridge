//! Global-assistant JSON-RPC handler.
//!
//! The global assistant exposes read-only query tools over all managed
//! repos plus `workbridge_create_work_item` (the only mutating tool on
//! this surface). Context is re-read on every message so tool calls see
//! fresh data as repos / work items change.

use crossbeam_channel::Sender;
use serde_json::{Value, json};

use super::McpEvent;

/// Handle an incoming JSON-RPC message for the global assistant.
/// Exposes read-only query tools plus `workbridge_create_work_item`.
pub fn handle_global_message(
    msg: &Value,
    context_json: &str,
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
                        "name": "workbridge-global",
                        "version": "0.1.0"
                    }
                }
            }))
        }
        "notifications/initialized" => None,
        "tools/list" => {
            let id = id?;
            Some(build_global_tools_list_response(id))
        }
        "tools/call" => handle_global_tools_call(msg, context_json, tx),
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

/// Build the JSON-RPC `tools/list` response payload for the global
/// assistant. Each tool carries explicit
/// `readOnlyHint` / `destructiveHint` / `openWorldHint` annotations
/// because Codex treats the absence of these as "require approval" -
/// see the per-work-item `tools/list` branch for the same rationale.
fn build_global_tools_list_response(id: &Value) -> Value {
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
    let tools = vec![
        json!({
            "name": "workbridge_list_repos",
            "description": "List all managed repositories with their paths.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "annotations": read_only_anno,
        }),
        json!({
            "name": "workbridge_list_work_items",
            "description": "List all work items with their current status, title, associated repo, branch, and PR info.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "annotations": read_only_anno,
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
            },
            "annotations": read_only_anno,
        }),
        json!({
            "name": "workbridge_create_work_item",
            "description": "Create a new work item from the current exploration context. Use this when the user wants to turn their research into actionable work. The work item will be created in Planning status and a planning session will be spawned automatically.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Concise title for the work item"
                    },
                    "description": {
                        "type": "string",
                        "description": "Description capturing the exploration context, findings, and intended work"
                    },
                    "repo_path": {
                        "type": "string",
                        "description": "Absolute path to the target repository (must be one of the managed repos)"
                    }
                },
                "required": ["title", "description", "repo_path"]
            },
            "annotations": mutating_anno,
        }),
    ];

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": tools
        }
    })
}

/// Dispatch a JSON-RPC `tools/call` for the global assistant to the
/// appropriate tool handler. Deserializes the shared dynamic-context
/// JSON once per call and routes by `tool_name`.
fn handle_global_tools_call(
    msg: &Value,
    context_json: &str,
    tx: &Sender<McpEvent>,
) -> Option<Value> {
    let id = msg.get("id")?;
    let params = msg.get("params")?;
    let tool_name = params.get("name")?.as_str()?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

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
            Some(tool_text_response(id, &text))
        }
        "workbridge_list_work_items" => {
            let items = ctx.get("work_items").cloned().unwrap_or(json!([]));
            let text = serde_json::to_string_pretty(&items).unwrap_or_default();
            Some(tool_text_response(id, &text))
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
                .unwrap_or_else(|| json!({"error": "repo not found in managed repos"}));

            let text = serde_json::to_string_pretty(&repo_info).unwrap_or_default();
            Some(tool_text_response(id, &text))
        }
        "workbridge_create_work_item" => {
            Some(handle_global_create_work_item(id, &arguments, &ctx, tx))
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

/// Shared envelope for tools/call success responses that just wrap a
/// string body in the MCP `content: [{type: text, text}]` shape.
fn tool_text_response(id: &Value, text: &str) -> Value {
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

/// Handle `workbridge_create_work_item` for the global assistant.
/// Validates the required fields and that `repo_path` is managed, then
/// enqueues a `CreateWorkItem` event on `tx` for the UI thread to
/// process.
fn handle_global_create_work_item(
    id: &Value,
    arguments: &Value,
    ctx: &Value,
    tx: &Sender<McpEvent>,
) -> Value {
    let title = arguments
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = arguments
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let repo_path = arguments
        .get("repo_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if title.is_empty() || repo_path.is_empty() {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{
                    "type": "text",
                    "text": "Error: title and repo_path are required"
                }],
                "isError": true
            }
        });
    }

    // Verify repo_path is in the managed repos list.
    let repo_known = ctx
        .get("repos")
        .and_then(|repos| repos.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|r| {
                r.get("path")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| p == repo_path)
            })
        });

    if !repo_known {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{
                    "type": "text",
                    "text": format!("Error: '{}' is not a managed repository", repo_path)
                }],
                "isError": true
            }
        });
    }

    let _ = tx.send(McpEvent::CreateWorkItem {
        title: title.clone(),
        description,
        repo_path: repo_path.clone(),
    });

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{
                "type": "text",
                "text": format!("Work item '{}' creation requested for repo '{}'. A planning session will start automatically once the main thread processes the request.", title, repo_path)
            }]
        }
    })
}
