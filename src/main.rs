mod app;
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

fn main() -> io::Result<()> {
    let app = App::new();

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
