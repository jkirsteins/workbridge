mod agent_backend;
mod app;
mod assembly;
mod click_targets;
mod clipboard;
mod config;
mod create_dialog;
mod dashboard_seed;
mod event;
mod fetcher;
mod github_client;
mod layout;
mod mcp;
mod metrics;
mod pr_service;
mod prompts;
mod salsa;
mod session;
mod theme;
mod ui;
mod work_item;
mod work_item_backend;
mod worktree_service;

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rat_salsa::RunConfig;
use rat_salsa::poll::{PollCrossterm, PollRendered, PollTimers};

use app::App;
use config::{ConfigProvider, FileConfigProvider};
use github_client::GhCliClient;
use salsa::{AppError, AppEvent, Global};
use worktree_service::GitWorktreeService;

/// Handle CLI subcommands. Returns true if a subcommand was handled (caller
/// should exit), false if the TUI should launch.
fn handle_cli(args: &[String]) -> bool {
    match args.get(1).map(|s| s.as_str()) {
        Some("repos") => handle_repos_subcommand(args),
        Some("mcp") => handle_mcp_subcommand(args),
        Some("config") => handle_config_subcommand(args),
        Some("seed-dashboard") => handle_seed_dashboard_subcommand(args),
        _ => return false,
    }
    true
}

/// Dev tool: populate a workbridge `work-items/` directory with synthetic
/// data so the metrics Dashboard can be visually verified end-to-end.
/// Intended to be run against an isolated `HOME` override (see
/// `docs/metrics.md` for the recommended tmux harness flow).
fn handle_seed_dashboard_subcommand(args: &[String]) {
    let Some(dir) = args.get(2) else {
        eprintln!("Usage: workbridge seed-dashboard <work-items-dir>");
        std::process::exit(1);
    };
    if let Err(e) = dashboard_seed::seed_dashboard(std::path::Path::new(dir)) {
        eprintln!("workbridge: seed-dashboard failed: {e}");
        std::process::exit(1);
    }
}

fn handle_repos_subcommand(args: &[String]) {
    match args.get(2).map(|s| s.as_str()) {
        Some("add") => {
            let Some(path) = args.get(3) else {
                eprintln!("Usage: workbridge repos add <path>");
                std::process::exit(1);
            };
            let mut cfg = load_config_or_exit();
            match cfg.add_repo(path) {
                Ok(display) => {
                    save_config_or_exit(&cfg);
                    println!("Added repo: {display}");
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("add-base") => {
            let Some(path) = args.get(3) else {
                eprintln!("Usage: workbridge repos add-base <path>");
                std::process::exit(1);
            };
            let mut cfg = load_config_or_exit();
            match cfg.add_base_dir(path) {
                Ok((display, count)) => {
                    save_config_or_exit(&cfg);
                    println!("Added base directory: {display} ({count} repos discovered)");
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("remove") => {
            let Some(path) = args.get(3) else {
                eprintln!("Usage: workbridge repos remove <path>");
                std::process::exit(1);
            };
            let mut cfg = load_config_or_exit();
            if cfg.remove_path(path) {
                save_config_or_exit(&cfg);
                println!("Removed: {path}");
            } else {
                println!("Path not found in config: {path}");
            }
        }
        Some("list") => {
            let show_all = args.get(3).is_some_and(|a| a == "--all");
            print_repo_list(&load_config_or_exit(), show_all);
        }
        None => {
            print_repo_list(&load_config_or_exit(), false);
        }
        Some(unknown) => {
            eprintln!("Unknown repos subcommand: {unknown}");
            eprintln!("Usage: workbridge repos [list|add|add-base|remove]");
            std::process::exit(1);
        }
    }
}

fn handle_mcp_subcommand(args: &[String]) {
    match args.get(2).map(|s| s.as_str()) {
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
                .map(|s| s.to_string()),
            args: server_val
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
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
                .map(|s| s.to_string()),
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

fn handle_config_subcommand(args: &[String]) {
    match args.get(2).map(|s| s.as_str()) {
        Some("set") => handle_config_set(args),
        None => handle_config_show(),
        Some(unknown) => {
            eprintln!("Unknown config subcommand: {unknown}");
            eprintln!("Usage: workbridge config [set <key> <value>]");
            std::process::exit(1);
        }
    }
}

fn handle_config_show() {
    match config::config_path() {
        Ok(path) => {
            println!("Config file: {}", path.display());
            if path.exists() {
                let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    eprintln!("Error reading config: {e}");
                    std::process::exit(1);
                });
                println!();
                print!("{contents}");
            } else {
                println!("(no config file yet)");
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// Outcome of the `config set` core routine. Kept as a value so the
/// unit tests in `tests::config_set_*` can assert the branch taken
/// without shelling out and without asserting on stdout/stderr text.
#[derive(Debug, PartialEq, Eq)]
enum ConfigSetOutcome {
    /// The key was set and the config was saved. Payload is the
    /// canonical name of the value for logging (same form as will
    /// round-trip through `config.toml`).
    Saved { key: String, value: String },
    /// The user passed a key we don't know about. The CLI wrapper
    /// prints a `workbridge config set` usage line and exits 1.
    UnknownKey(String),
    /// The user passed a value that didn't parse for the given key.
    InvalidValue {
        key: String,
        value: String,
        err: String,
    },
    /// Missing positional arguments.
    MissingArgs,
}

/// Core of `workbridge config set <key> <value>`. Pure in the sense
/// that it takes an explicit `ConfigProvider` (so tests can pass an
/// `InMemoryConfigProvider`) and does not touch global state. The CLI
/// wrapper in `handle_config_set` maps each outcome to `println!` /
/// `eprintln!` + exit-code semantics.
fn apply_config_set(provider: &dyn config::ConfigProvider, args: &[String]) -> ConfigSetOutcome {
    // args[0] is the program name; args[1] == "config"; args[2] ==
    // "set". Key starts at args[3].
    let Some(key) = args.get(3) else {
        return ConfigSetOutcome::MissingArgs;
    };
    let Some(value) = args.get(4) else {
        return ConfigSetOutcome::MissingArgs;
    };

    match key.as_str() {
        "global-assistant-harness" => {
            // Validate the value before loading/mutating config so a
            // typo cannot half-apply.
            if let Err(e) = agent_backend::AgentBackendKind::from_str(value) {
                return ConfigSetOutcome::InvalidValue {
                    key: key.clone(),
                    value: value.clone(),
                    err: e.to_string(),
                };
            }
            let mut cfg = match provider.load() {
                Ok(c) => c,
                Err(e) => {
                    return ConfigSetOutcome::InvalidValue {
                        key: key.clone(),
                        value: value.clone(),
                        err: format!("load failed: {e}"),
                    };
                }
            };
            cfg.defaults.global_assistant_harness = Some(value.clone());
            if let Err(e) = provider.save(&cfg) {
                return ConfigSetOutcome::InvalidValue {
                    key: key.clone(),
                    value: value.clone(),
                    err: format!("save failed: {e}"),
                };
            }
            ConfigSetOutcome::Saved {
                key: key.clone(),
                value: value.clone(),
            }
        }
        other => ConfigSetOutcome::UnknownKey(other.to_string()),
    }
}

fn handle_config_set(args: &[String]) {
    let outcome = apply_config_set(&FileConfigProvider, args);
    match outcome {
        ConfigSetOutcome::Saved { key, value } => {
            println!("saved: {key} = {value}");
        }
        ConfigSetOutcome::UnknownKey(k) => {
            eprintln!("Unknown config key: {k}");
            eprintln!("Supported keys: global-assistant-harness");
            std::process::exit(1);
        }
        ConfigSetOutcome::InvalidValue { key, value, err } => {
            eprintln!("Error: invalid value '{value}' for key '{key}': {err}");
            std::process::exit(1);
        }
        ConfigSetOutcome::MissingArgs => {
            eprintln!("Usage: workbridge config set <key> <value>");
            eprintln!("Supported keys: global-assistant-harness");
            std::process::exit(1);
        }
    }
}

fn load_config_or_exit() -> config::Config {
    config::Config::load().unwrap_or_else(|e| {
        eprintln!("Error loading config: {e}");
        std::process::exit(1);
    })
}

fn save_config_or_exit(cfg: &config::Config) {
    FileConfigProvider.save(cfg).unwrap_or_else(|e| {
        eprintln!("Error saving config: {e}");
        std::process::exit(1);
    });
}

fn print_repo_list(cfg: &config::Config, show_all: bool) {
    let active = cfg.active_repos();
    let entries = if show_all {
        cfg.all_repos()
    } else {
        active.clone()
    };
    if entries.is_empty() {
        if show_all {
            println!("No repositories configured.");
            println!("Use 'workbridge repos add <path>' to add one.");
        } else {
            println!("No managed repositories.");
            println!("Use 'workbridge repos list --all' to see all,");
            println!("or 'workbridge repos add <path>' to add one.");
        }
    } else {
        let label = if show_all { "ALL" } else { "MANAGED" };
        println!("{label} {:<57} {:<12} AVAILABLE", "PATH", "SOURCE");
        println!("{}", "-".repeat(85));
        let active_paths: Vec<_> = active.iter().map(|e| &e.path).collect();
        for entry in &entries {
            let source = match entry.source {
                config::RepoSource::Explicit => "explicit",
                config::RepoSource::Discovered => "discovered",
            };
            let avail = if entry.git_dir_present { "yes" } else { "no" };
            let status = if show_all && !active_paths.contains(&&entry.path) {
                " [unmanaged]"
            } else {
                ""
            };
            println!(
                "     {:<57} {:<12} {}{status}",
                entry.path.display(),
                source,
                avail,
            );
        }
        if !show_all {
            let all_count = cfg.all_repos().len();
            let unmanaged = all_count.saturating_sub(active.len());
            if unmanaged > 0 {
                println!("\n{unmanaged} repo(s) available but unmanaged. Use --all to see all.");
            }
        }
    }
}

fn main() -> Result<(), AppError> {
    let args: Vec<String> = std::env::args().collect();

    // MCP bridge mode: pipe stdin/stdout to/from a Unix socket.
    if args.iter().any(|a| a == "--mcp-bridge") {
        let bridge_args = mcp::BridgeArgs::parse(&args).unwrap_or_else(|| {
            eprintln!("Usage: workbridge --mcp-bridge --socket <path>");
            std::process::exit(1);
        });
        mcp::run_bridge(bridge_args.socket_path);
        return Ok(());
    }

    // CLI subcommands: handle before TUI setup.
    if handle_cli(&args) {
        return Ok(());
    }

    // Load config and discover repos for the TUI.
    let (cfg, config_error) = match config::Config::load() {
        Ok(c) => (c, None),
        Err(e) => {
            let msg = format!("Config error: {e} (using defaults)");
            eprintln!("workbridge: {msg}");
            (config::Config::default(), Some(msg))
        }
    };
    let (backend, backend_error): (Arc<dyn work_item_backend::WorkItemBackend>, Option<String>) =
        match work_item_backend::LocalFileBackend::new() {
            Ok(b) => (Arc::new(b), None),
            Err(e) => {
                let msg = format!("Backend error: {e} (using stub)");
                eprintln!("workbridge: {msg}");
                (Arc::new(app::StubBackend), Some(msg))
            }
        };
    let worktree_service: Arc<dyn worktree_service::WorktreeService + Send + Sync> =
        Arc::new(GitWorktreeService);
    let github_client: Arc<dyn github_client::GithubClient + Send + Sync> =
        Arc::new(GhCliClient::new());

    let mut app = App::with_config_and_worktree_service(
        cfg,
        backend,
        Arc::clone(&worktree_service),
        Box::new(FileConfigProvider),
    );
    // Spawn the background metrics aggregator and wire its receiver into
    // the App so the Dashboard view has fresh data. The aggregator reads
    // the same data dir LocalFileBackend uses; if that dir cannot be
    // resolved we silently skip - the Dashboard renders a placeholder.
    if let Some(data_dir) = metrics::default_data_dir() {
        app.metrics_rx = Some(metrics::spawn_metrics_aggregator(data_dir));
    }
    // Surface config/backend load errors in the TUI status bar so the user sees them.
    if let Some(msg) = config_error {
        app.status_message = Some(msg);
    } else if let Some(msg) = backend_error {
        app.status_message = Some(msg);
    } else if !app.gh_available {
        app.status_message =
            Some("Warning: 'gh' CLI not found. PR creation and merge features require it.".into());
    }

    // Validate branch_issue_pattern at startup. An invalid regex would
    // cause every fetcher thread to exit immediately (the channel
    // disconnects and background updates stop permanently). Catch it
    // early, show an error, and fall back to an empty pattern (which
    // disables issue extraction but keeps the fetcher alive).
    if let Err(e) = regex::Regex::new(&app.config.defaults.branch_issue_pattern) {
        let bad = app.config.defaults.branch_issue_pattern.clone();
        app.config.defaults.branch_issue_pattern = String::new();
        let msg = format!(
            "Invalid branch_issue_pattern '{}': {} (issue extraction disabled)",
            bad, e,
        );
        // Only overwrite if no higher-priority error is already shown.
        if app.status_message.is_none() {
            app.status_message = Some(msg);
        } else {
            app.pending_fetch_errors.push(msg);
        }
    }

    // Install SIGTERM and SIGINT handlers using an atomic flag.
    // When either signal is received, the flag is set and the timer
    // callback initiates the same graceful shutdown path as keyboard quit.
    //
    // Note: AtomicBool can coalesce two rapid signals into one observed
    // event (both set the flag before the timer reads it). This means
    // two quick SIGTERMs could start graceful shutdown instead of force-
    // killing. This is acceptable because the 10-second shutdown deadline
    // handles escalation automatically.
    let signal_received = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&signal_received))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&signal_received))?;

    let mut global = Global {
        ctx: Default::default(),
        theme: theme::Theme::default_theme(),
        signal_received,
        worktree_service,
        github_client,
    };

    // Enable mouse capture so scroll events can be forwarded to embedded
    // PTY sessions. Keyboard enhancements remain disabled because they
    // change how Ctrl+] is reported and would break existing key handling.
    let term_init = rat_salsa::TermInit {
        mouse_capture: true,
        keyboard_enhancements: ratatui_crossterm::crossterm::event::KeyboardEnhancementFlags::empty(
        ),
        ..Default::default()
    };

    // Run the rat-salsa event loop. This handles terminal setup/teardown
    // automatically (raw mode, alternate screen, panic hook for restore).
    rat_salsa::run_tui(
        salsa::app_init,
        salsa::app_render,
        salsa::app_event,
        salsa::app_error,
        &mut global,
        &mut app,
        RunConfig::<AppEvent, AppError>::default()?
            .poll(PollCrossterm)
            .poll(PollTimers::default())
            .poll(PollRendered)
            .term_init(term_init),
    )?;

    // Stop the background fetcher (non-blocking: just sets stop flag).
    // Threads will exit on their own when they check the flag or when
    // their channel send fails. No joining - no UI freeze on quit.
    if let Some(handle) = app.fetcher_handle.take() {
        handle.stop();
    }

    Ok(())
}
// mergequeue e2e test

#[cfg(test)]
mod tests {
    use super::*;
    use config::InMemoryConfigProvider;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// Pins the round-trip: `workbridge config set global-assistant-
    /// harness codex` loads -> mutates -> saves through the provider.
    #[test]
    fn config_set_global_assistant_harness_writes_config() {
        let provider = InMemoryConfigProvider::new();
        let args = argv(&[
            "workbridge",
            "config",
            "set",
            "global-assistant-harness",
            "codex",
        ]);
        let outcome = apply_config_set(&provider, &args);
        assert_eq!(
            outcome,
            ConfigSetOutcome::Saved {
                key: "global-assistant-harness".into(),
                value: "codex".into(),
            }
        );

        // Reload via the provider to confirm the value was persisted.
        let reloaded = provider.load().unwrap();
        assert_eq!(
            reloaded.defaults.global_assistant_harness.as_deref(),
            Some("codex")
        );
    }

    /// Pins that typos are rejected without touching the provider.
    #[test]
    fn config_set_rejects_unknown_harness_name() {
        let provider = InMemoryConfigProvider::new();
        // Seed an existing value so we can assert it survives a
        // failed `set`.
        let mut seed = config::Config::for_test();
        seed.defaults.global_assistant_harness = Some("claude".into());
        provider.save(&seed).unwrap();

        let args = argv(&[
            "workbridge",
            "config",
            "set",
            "global-assistant-harness",
            "gemini",
        ]);
        let outcome = apply_config_set(&provider, &args);
        assert!(
            matches!(outcome, ConfigSetOutcome::InvalidValue { ref value, .. } if value == "gemini")
        );

        // The prior value must survive the rejection.
        let reloaded = provider.load().unwrap();
        assert_eq!(
            reloaded.defaults.global_assistant_harness.as_deref(),
            Some("claude")
        );
    }

    /// Pins that unknown keys are surfaced rather than silently
    /// accepted.
    #[test]
    fn config_set_rejects_unknown_config_key() {
        let provider = InMemoryConfigProvider::new();
        let args = argv(&["workbridge", "config", "set", "bogus-key", "value"]);
        let outcome = apply_config_set(&provider, &args);
        assert_eq!(outcome, ConfigSetOutcome::UnknownKey("bogus-key".into()));
    }

    /// Pins the missing-args branch: both key and value are required.
    #[test]
    fn config_set_missing_args_returns_error() {
        let provider = InMemoryConfigProvider::new();
        let args = argv(&["workbridge", "config", "set"]);
        assert_eq!(
            apply_config_set(&provider, &args),
            ConfigSetOutcome::MissingArgs
        );
        let args = argv(&["workbridge", "config", "set", "global-assistant-harness"]);
        assert_eq!(
            apply_config_set(&provider, &args),
            ConfigSetOutcome::MissingArgs
        );
    }
}
