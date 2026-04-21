mod agent_backend;
mod app;
mod assembly;
mod cli;
mod click_targets;
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
pub mod side_effects;
mod theme;
mod ui;
mod work_item;
mod work_item_backend;
mod worktree_service;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use app::App;
use config::FileConfigProvider;
use github_client::GhCliClient;
use rat_salsa::RunConfig;
use rat_salsa::poll::{PollCrossterm, PollRendered, PollTimers};
use salsa::{AppError, AppEvent, Global};
use worktree_service::GitWorktreeService;

/// Handle CLI subcommands. Returns true if a subcommand was handled (caller
/// should exit), false if the TUI should launch.
fn handle_cli(args: &[String]) -> bool {
    match args.get(1).map(std::string::String::as_str) {
        Some("repos") => cli::repos::handle_repos_subcommand(args),
        Some("mcp") => cli::mcp::handle_mcp_subcommand(args),
        Some("config") => cli::config::handle_config_subcommand(args),
        Some("seed-dashboard") => cli::seed_dashboard::handle_seed_dashboard_subcommand(args),
        _ => return false,
    }
    true
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

    let mut app = App::with_config_worktree_and_github(
        cfg,
        backend,
        Arc::clone(&worktree_service),
        Arc::clone(&github_client),
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
        let msg = format!("Invalid branch_issue_pattern '{bad}': {e} (issue extraction disabled)");
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
        ctx: rat_salsa::SalsaAppContext::default(),
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
