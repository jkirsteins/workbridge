mod app;
mod assembly;
mod config;
mod create_dialog;
mod event;
mod fetcher;
mod github_client;
mod layout;
mod mcp;
mod prompts;
mod salsa;
mod session;
mod theme;
mod ui;
mod work_item;
mod work_item_backend;
mod worktree_service;

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
        Some("config") => handle_config_subcommand(),
        _ => return false,
    }
    true
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

fn handle_config_subcommand() {
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
    let (backend, backend_error): (Box<dyn work_item_backend::WorkItemBackend>, Option<String>) =
        match work_item_backend::LocalFileBackend::new() {
            Ok(b) => (Box::new(b), None),
            Err(e) => {
                let msg = format!("Backend error: {e} (using stub)");
                eprintln!("workbridge: {msg}");
                (Box::new(app::StubBackend), Some(msg))
            }
        };
    let worktree_service: Arc<dyn worktree_service::WorktreeService + Send + Sync> =
        Arc::new(GitWorktreeService);
    let github_client: Arc<dyn github_client::GithubClient + Send + Sync> = Arc::new(GhCliClient);

    let mut app = App::with_config_and_worktree_service(
        cfg,
        backend,
        Arc::clone(&worktree_service),
        Box::new(FileConfigProvider),
    );
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
