//! `workbridge mcp` subcommand handlers.

use super::{load_config_or_exit, save_config_or_exit};
use crate::config;

pub fn handle_mcp_subcommand(args: &[String]) {
    match args.get(2).map(std::string::String::as_str) {
        Some("add") => handle_mcp_add(args),
        Some("remove") => handle_mcp_remove(args),
        Some("list") => handle_mcp_list(args),
        Some("import") => handle_mcp_import(args),
        None => {
            eprintln!("Usage: workbridge mcp [add|remove|list|import]");
            std::process::exit(1);
        }
        Some(unknown) => {
            eprintln!("Unknown mcp subcommand: {unknown}");
            eprintln!("Usage: workbridge mcp [add|remove|list|import]");
            std::process::exit(1);
        }
    }
}

fn handle_mcp_add(args: &[String]) {
    let repo_path = args.get(3).unwrap_or_else(|| {
        eprintln!("Usage: workbridge mcp add <repo-path> <name> [--command <cmd>] [--args <arg>...] [--env KEY=VALUE...] [--url <url>]");
        std::process::exit(1);
    });
    let name = args.get(4).unwrap_or_else(|| {
        eprintln!("Usage: workbridge mcp add <repo-path> <name> [--command <cmd>] [--args <arg>...] [--env KEY=VALUE...] [--url <url>]");
        std::process::exit(1);
    });

    let mut command: Option<String> = None;
    let mut cmd_args: Vec<String> = Vec::new();
    let mut env: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut url: Option<String> = None;
    let mut server_type = "stdio".to_string();

    let mut i = 5usize;
    while i < args.len() {
        match args[i].as_str() {
            "--command" => {
                command = args.get(i + 1).cloned();
                i += 2;
            }
            "--url" => {
                url = args.get(i + 1).cloned();
                server_type = "http".to_string();
                i += 2;
            }
            "--args" => {
                i += 1;
                while i < args.len() && !args[i].starts_with("--") {
                    cmd_args.push(args[i].clone());
                    i += 1;
                }
            }
            "--env" => {
                i += 1;
                while i < args.len() && !args[i].starts_with("--") {
                    if let Some((k, v)) = args[i].split_once('=') {
                        env.insert(k.to_string(), v.to_string());
                    } else {
                        eprintln!("Invalid --env entry (expected KEY=VALUE): {}", args[i]);
                        std::process::exit(1);
                    }
                    i += 1;
                }
            }
            other => {
                eprintln!("Unknown flag: {other}");
                std::process::exit(1);
            }
        }
    }

    if server_type == "stdio" && command.is_none() {
        eprintln!("Error: --command is required for stdio MCP servers (or use --url for http)");
        std::process::exit(1);
    }

    let entry = config::McpServerEntry {
        repo: repo_path.clone(),
        name: name.clone(),
        server_type,
        command,
        args: cmd_args,
        env,
        url,
    };

    let mut cfg = load_config_or_exit();
    match cfg.add_mcp_server(entry) {
        Ok(()) => {
            save_config_or_exit(&cfg);
            let repo_display = config::normalize_repo_path(repo_path);
            println!("Added MCP server '{name}' for repo '{repo_display}'");
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

fn handle_mcp_remove(args: &[String]) {
    let repo_path = args.get(3).unwrap_or_else(|| {
        eprintln!("Usage: workbridge mcp remove <repo-path> <name>");
        std::process::exit(1);
    });
    let name = args.get(4).unwrap_or_else(|| {
        eprintln!("Usage: workbridge mcp remove <repo-path> <name>");
        std::process::exit(1);
    });
    let mut cfg = load_config_or_exit();
    if cfg.remove_mcp_server(repo_path, name) {
        save_config_or_exit(&cfg);
        let repo_display = config::normalize_repo_path(repo_path);
        println!("Removed MCP server '{name}' from repo '{repo_display}'");
    } else {
        let repo_display = config::normalize_repo_path(repo_path);
        println!("MCP server '{name}' not found for repo '{repo_display}'");
    }
}

fn handle_mcp_list(args: &[String]) {
    let cfg = load_config_or_exit();
    let repo_filter = args.get(3).map(|p| config::normalize_repo_path(p));

    let servers: Vec<_> = cfg
        .mcp_servers
        .iter()
        .filter(|s| repo_filter.as_ref().is_none_or(|r| &s.repo == r))
        .collect();

    if servers.is_empty() {
        if let Some(ref repo) = repo_filter {
            println!("No MCP servers configured for repo '{repo}'.");
        } else {
            println!("No MCP servers configured.");
            println!("Use 'workbridge mcp add <repo-path> <name> ...' to add one.");
        }
        return;
    }

    let mut current_repo = "";
    for server in &servers {
        if server.repo != current_repo {
            println!("\nRepo: {}", server.repo);
            current_repo = &server.repo;
        }
        if server.server_type == "http" {
            println!(
                "  {} (http)  url: {}",
                server.name,
                server.url.as_deref().unwrap_or("<no url>")
            );
        } else {
            println!(
                "  {}  command: {}{}",
                server.name,
                server.command.as_deref().unwrap_or("<no command>"),
                if server.args.is_empty() {
                    String::new()
                } else {
                    format!("  args: {}", server.args.join(" "))
                }
            );
        }
    }
}

fn handle_mcp_import(args: &[String]) {
    let repo_path = args.get(3).unwrap_or_else(|| {
        eprintln!("Usage: workbridge mcp import <repo-path> <json-file>");
        std::process::exit(1);
    });
    let json_file = args.get(4).unwrap_or_else(|| {
        eprintln!("Usage: workbridge mcp import <repo-path> <json-file>");
        std::process::exit(1);
    });

    let contents = std::fs::read_to_string(json_file).unwrap_or_else(|e| {
        eprintln!("Error reading '{json_file}': {e}");
        std::process::exit(1);
    });
    let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap_or_else(|e| {
        eprintln!("Error parsing JSON from '{json_file}': {e}");
        std::process::exit(1);
    });
    let servers = parsed
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| {
            eprintln!("No 'mcpServers' object found in '{json_file}'");
            std::process::exit(1);
        });

    let mut entries: Vec<config::McpServerEntry> = Vec::new();
    for (name, server_val) in servers {
        let server_type = server_val
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("stdio")
            .to_string();
        let entry = config::McpServerEntry {
            repo: repo_path.clone(),
            name: name.clone(),
            server_type: server_type.clone(),
            command: server_val
                .get("command")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
            args: server_val
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            env: server_val
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            url: server_val
                .get("url")
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string),
        };
        entries.push(entry);
    }

    let repo_display = config::normalize_repo_path(repo_path);
    let count = entries.len();
    let mut cfg = load_config_or_exit();
    cfg.import_mcp_servers(&repo_display, entries);
    save_config_or_exit(&cfg);
    println!("Imported {count} MCP server(s) for repo '{repo_display}'");
}
