mod app;
mod config;
mod event;
mod layout;
mod session;
mod ui;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;

/// RAII guard that restores the terminal on drop.
///
/// Session cleanup is handled by the graceful shutdown flow in the main
/// loop. This guard only restores the terminal. If we reach Drop via a
/// panic, individual Session Drop impls will SIGKILL their children.
struct TerminalGuard {
    app: Option<App>,
}

impl TerminalGuard {
    fn app_mut(&mut self) -> &mut App {
        self.app.as_mut().expect("TerminalGuard must always own an App")
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Restore the terminal so the user gets a usable shell back.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        // Sessions are cleaned up by their own Drop impls (SIGKILL)
        // if we reach here via panic. Normal exit already handled
        // shutdown in the main loop.
    }
}

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
        Some("list") | None => {
            let cfg = load_config_or_exit();
            let entries = cfg.all_repos();
            if entries.is_empty() {
                println!("No repositories configured.");
                println!("Use 'workbridge repos add <path>' to add one.");
            } else {
                println!("{:<60} {:<12} AVAILABLE", "PATH", "SOURCE");
                println!("{}", "-".repeat(80));
                for entry in &entries {
                    let source = match entry.source {
                        config::RepoSource::Explicit => "explicit",
                        config::RepoSource::Discovered => "discovered",
                    };
                    let avail = if entry.available { "yes" } else { "no" };
                    println!("{:<60} {:<12} {}", entry.path.display(), source, avail);
                }
            }
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
    cfg.save().unwrap_or_else(|e| {
        eprintln!("Error saving config: {e}");
        std::process::exit(1);
    });
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

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
    let discovered = cfg.discover_repos();
    let mut app = App::with_config(cfg, discovered);
    // Surface config load errors in the TUI status bar so the user sees them.
    if let Some(msg) = config_error {
        app.status_message = Some(msg);
    }

    // Install a panic hook that restores the terminal before printing the panic.
    // Child processes are cleaned up automatically when the PTY master fd closes
    // (the OS sends SIGHUP to the process group).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort terminal restore - ignore errors since we are panicking.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);

        // Invoke the default panic handler so the user sees the backtrace.
        default_hook(info);
    }));

    // Install SIGTERM and SIGINT handlers using an atomic flag.
    // When either signal is received, the flag is set and the main loop
    // initiates the same graceful shutdown path as keyboard quit.
    //
    // Note: AtomicBool can coalesce two rapid signals into one observed
    // event (both set the flag before the main loop reads it). This means
    // two quick SIGTERMs could start graceful shutdown instead of force-
    // killing. This is acceptable because the 10-second shutdown deadline
    // handles escalation automatically - a supervisor that sends SIGTERM
    // and then waits will see the process exit within 10s regardless.
    let signal_received = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&signal_received))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&signal_received))?;

    // Create the RAII guard BEFORE enabling raw mode so that any failure during
    // terminal setup triggers cleanup on early return via ?.
    let mut guard = TerminalGuard {
        app: Some(app),
    };

    // Terminal setup: enable raw mode and switch to alternate screen.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Set initial pane dimensions from the terminal size.
    let size = terminal.size()?;
    let app = guard.app_mut();
    let has_status = app.status_message.is_some();
    let pl = layout::compute(size.width, size.height, has_status);
    app.pane_cols = pl.pane_cols;
    app.pane_rows = pl.pane_rows;

    let mut last_tick = Instant::now();

    loop {
        let app = guard.app_mut();

        // Render the UI.
        terminal.draw(|frame| ui::draw(frame, app))?;

        let app = guard.app_mut();

        // Poll for events or tick.
        let tick_occurred = event::poll_and_handle(app, &mut last_tick)?;

        // Liveness check runs on periodic ticks. Reader threads handle
        // PTY output continuously - the UI thread only needs to check
        // if child processes have exited.
        if tick_occurred {
            app.check_liveness();
        }

        // Check for external signals (SIGTERM, SIGINT).
        if signal_received.swap(false, Ordering::Relaxed) {
            if app.shutting_down {
                // Second signal during shutdown - force kill and exit.
                app.force_kill_all();
                break;
            } else {
                // First signal - initiate graceful shutdown.
                app.send_sigterm_all();
                app.shutting_down = true;
                app.shutdown_started = Some(Instant::now());
                app.status_message =
                    Some("Waiting for sessions (force quit in 10s, or press Q)".into());
                if app.all_dead() {
                    break;
                }
            }
        }

        if app.shutting_down {
            // During shutdown, exit once all sessions have died.
            if app.all_dead() {
                break;
            }
            // Force quit (Q during shutdown) sets should_quit.
            if app.should_quit {
                break;
            }
            // Check the 10-second deadline. If elapsed, force-kill and exit.
            if let Some(started) = app.shutdown_started {
                let elapsed = started.elapsed();
                if elapsed >= Duration::from_secs(10) {
                    app.force_kill_all();
                    break;
                }
                // Update the status bar with remaining seconds.
                let remaining = 10u64.saturating_sub(elapsed.as_secs());
                app.status_message = Some(format!(
                    "Waiting for sessions (force quit in {remaining}s, or press Q)"
                ));
            }
            continue;
        }

        if app.should_quit {
            // Initiate graceful shutdown: send SIGTERM to all sessions,
            // then continue the main loop so the UI stays responsive
            // while children handle the signal.
            app.send_sigterm_all();
            app.shutting_down = true;
            app.shutdown_started = Some(Instant::now());
            app.should_quit = false;
            app.status_message =
                Some("Waiting for sessions (force quit in 10s, or press Q)".into());
            // If all sessions are already dead (or none exist), exit now.
            if app.all_dead() {
                break;
            }
        }
    }

    Ok(())
}
