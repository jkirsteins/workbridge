use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rat_salsa::event::RenderedEvent;
use rat_salsa::timer::{TimeOut, TimerDef};
use rat_salsa::{Control, SalsaAppContext, SalsaContext};
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::Rect;

use crate::app::App;
use crate::event;
use crate::fetcher;
use crate::github_client;
use crate::layout;
use crate::theme::Theme;
use crate::ui;
use crate::worktree_service;

// Use the crossterm event type re-exported by rat-event, which matches
// the version that rat-salsa's PollCrossterm expects (crossterm 0.29 via
// ratatui-crossterm). The project's direct crossterm dependency (0.28)
// is used by the existing event loop and will be migrated in a later phase.
pub use rat_event::crossterm as ct;

/// Custom event type for the application.
///
/// Each variant wraps one of the event sources that rat-salsa's poll
/// implementations produce. The From impls below satisfy the trait
/// bounds that PollCrossterm, PollTimers, and PollRendered require.
#[derive(Debug)]
#[allow(dead_code)]
pub enum AppEvent {
    /// Terminal events (keyboard, mouse, resize) from crossterm.
    Crossterm(ct::event::Event),
    /// Timer tick (periodic liveness, fetch drain, shutdown checks).
    Timer(TimeOut),
    /// Sent immediately after a frame render completes.
    Rendered,
    /// Internal messages between components (future: dialog results).
    #[allow(dead_code)]
    Message(AppMessage),
}

/// Internal messages between components (for future use).
#[derive(Debug)]
#[allow(dead_code)]
pub enum AppMessage {
    CreateConfirmed {
        title: String,
        repos: Vec<std::path::PathBuf>,
        branch: Option<String>,
    },
    CreateCancelled,
}

/// Application error type.
///
/// Wraps the error kinds that can occur during the rat-salsa event
/// loop. run_tui requires `Error: From<io::Error>`.
/// RunConfig::default() requires `Error: From<crossbeam::channel::TryRecvError>`.
#[derive(Debug)]
pub enum AppError {
    Io(std::io::Error),
    General(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Io(e) => write!(f, "{}", e),
            AppError::General(msg) => write!(f, "{}", msg),
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Io(e)
    }
}

impl From<crossbeam_channel::TryRecvError> for AppError {
    fn from(e: crossbeam_channel::TryRecvError) -> Self {
        AppError::General(format!("channel recv error: {}", e))
    }
}

// -- From impls required by rat-salsa poll sources --

impl From<ct::event::Event> for AppEvent {
    fn from(event: ct::event::Event) -> Self {
        AppEvent::Crossterm(event)
    }
}

impl From<TimeOut> for AppEvent {
    fn from(timeout: TimeOut) -> Self {
        AppEvent::Timer(timeout)
    }
}

impl From<RenderedEvent> for AppEvent {
    fn from(_: RenderedEvent) -> Self {
        AppEvent::Rendered
    }
}

/// Global context that implements SalsaContext.
///
/// This is the "thin" global state that rat-salsa's run_tui requires.
/// It holds the SalsaAppContext (which run_tui populates during init),
/// plus application-wide immutable state like the theme and signal flag.
///
/// All mutable application state lives in the State parameter (the
/// existing App struct), not here.
pub struct Global {
    pub ctx: SalsaAppContext<AppEvent, AppError>,
    pub theme: Theme,
    pub signal_received: Arc<AtomicBool>,
    pub worktree_service: Arc<dyn worktree_service::WorktreeService + Send + Sync>,
    pub github_client: Arc<dyn github_client::GithubClient + Send + Sync>,
}

impl SalsaContext<AppEvent, AppError> for Global {
    fn set_salsa_ctx(&mut self, app_ctx: SalsaAppContext<AppEvent, AppError>) {
        self.ctx = app_ctx;
    }

    fn salsa_ctx(&self) -> &SalsaAppContext<AppEvent, AppError> {
        &self.ctx
    }
}

// -- rat-salsa callbacks --

/// Initialization callback. Called once after rat-salsa sets up the terminal.
/// Starts the tick timer, runs initial assembly, and starts the background fetcher.
pub fn app_init(state: &mut App, ctx: &mut Global) -> Result<(), AppError> {
    // Render tick: ~120fps (8ms).  PTY output arrives on reader threads
    // and updates the vt100 parser, but only a timer-driven re-render
    // makes it visible.  A fast tick keeps embedded terminal rendering
    // smooth (drag-and-drop paste, scrolling output, etc.).
    //
    // Heavy background work (liveness, fetch drain, signal checks,
    // shutdown deadline) is throttled inside the handler to run only
    // every BACKGROUND_TICK_DIVISOR-th tick (~200ms).
    ctx.add_timer(
        TimerDef::new()
            .timer(Duration::from_millis(8))
            .repeat_forever(),
    );

    // Set initial pane dimensions from the terminal size.
    {
        let term = ctx.terminal();
        let term_ref = term.borrow();
        let size = term_ref.size().map_err(AppError::from)?;
        let bottom_rows = u16::from(state.has_visible_status_bar())
            + u16::from(state.selected_work_item_context().is_some());
        let pl = layout::compute(size.width, size.height, bottom_rows);
        state.pane_cols = pl.pane_cols;
        state.pane_rows = pl.pane_rows;

        // Compute global drawer PTY dimensions via shared helper.
        let dl = layout::compute_drawer(size.width, size.height);
        state.global_pane_cols = dl.pane_cols;
        state.global_pane_rows = dl.pane_rows;
    }

    // Initial reassembly + display list build (already done in App::new,
    // but re-run in case config setup added status messages that affect layout).
    state.reassemble_work_items();
    state.build_display_list();

    // Start background fetcher for active repos with git directories.
    let active_repos: Vec<PathBuf> = state
        .active_repo_cache
        .iter()
        .filter(|r| r.git_dir_present)
        .map(|r| r.path.clone())
        .collect();

    let extra_branches = state.extra_branches_from_backend();
    if !active_repos.is_empty() {
        let (rx, handle) = fetcher::start_with_extra_branches(
            active_repos,
            Arc::clone(&ctx.worktree_service),
            Arc::clone(&ctx.github_client),
            state.config.defaults.branch_issue_pattern.clone(),
            extra_branches,
        );
        state.fetch_rx = Some(rx);
        state.fetcher_handle = Some(handle);
    }

    // Backfill PR identity for Done items that were merged before
    // persistence was added. One-time startup migration - can be removed
    // once no Done items with pr_identity=None remain on disk.
    let backfill_requests = state.collect_backfill_requests();
    if !backfill_requests.is_empty() {
        let gc = Arc::clone(&ctx.github_client);
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            use std::collections::HashMap;
            // Group by (owner, repo) to make one API call per repo.
            let mut by_repo: HashMap<(String, String), Vec<_>> = HashMap::new();
            for (wi_id, repo_path, branch, owner, repo_name) in backfill_requests {
                by_repo
                    .entry((owner, repo_name))
                    .or_default()
                    .push((wi_id, repo_path, branch));
            }
            for ((owner, repo_name), items) in by_repo {
                let merged_prs = match gc.list_merged_prs(&owner, &repo_name) {
                    Ok(prs) => prs,
                    Err(e) => {
                        let _ = tx.send(Err(format!(
                            "failed to list merged PRs for {owner}/{repo_name}: {e}"
                        )));
                        continue;
                    }
                };
                for (wi_id, repo_path, branch) in items {
                    if let Some(pr) = merged_prs.iter().find(|p| p.head_branch == branch) {
                        let identity = crate::work_item_backend::PrIdentityRecord {
                            number: pr.number,
                            title: pr.title.clone(),
                            url: pr.url.clone(),
                        };
                        let _ = tx.send(Ok(crate::app::PrIdentityBackfillResult {
                            wi_id,
                            repo_path,
                            identity,
                        }));
                    }
                }
            }
        });
        state.pr_identity_backfill_rx = Some(rx);
    }

    Ok(())
}

/// Render callback. Called by rat-salsa when the UI needs to be redrawn.
/// Receives a raw Buffer instead of a Frame - widgets render directly to it.
pub fn app_render(
    area: Rect,
    buf: &mut Buffer,
    state: &mut App,
    ctx: &mut Global,
) -> Result<(), AppError> {
    // Use ui::draw_to_buffer which renders directly to the buffer.
    ui::draw_to_buffer(area, buf, state, &ctx.theme);
    Ok(())
}

/// Event callback. Dispatches crossterm events to key/resize handlers,
/// timer events to periodic work (liveness, fetch drain, signals, shutdown).
pub fn app_event(
    evt: &AppEvent,
    state: &mut App,
    ctx: &mut Global,
) -> Result<Control<AppEvent>, AppError> {
    match evt {
        AppEvent::Crossterm(ct_event) => {
            match ct_event {
                ct::event::Event::Key(key) => {
                    if !event::handle_key(state, *key) {
                        return Ok(Control::Continue);
                    }
                }
                ct::event::Event::Resize(cols, rows) => {
                    event::handle_resize(state, *cols, *rows);
                }
                ct::event::Event::Mouse(mouse_event) => {
                    if !event::handle_mouse(state, *mouse_event) {
                        // Mouse event did not modify state (e.g. motion,
                        // click, or scroll that wasn't forwarded). Skip
                        // re-render.
                        return Ok(Control::Continue);
                    }
                }
                ct::event::Event::Paste(data) => {
                    if !event::handle_paste(state, data) {
                        return Ok(Control::Continue);
                    }
                }
                _ => {
                    return Ok(Control::Continue);
                }
            }
            // Check if the app wants to quit after handling the key event.
            if state.should_quit && !state.shutting_down {
                // Initiate graceful shutdown.
                state.send_sigterm_all();
                state.cleanup_all_mcp();
                state.shutting_down = true;
                state.shutdown_started = Some(std::time::Instant::now());
                state.should_quit = false;
                state.status_message =
                    Some("Waiting for sessions (force quit in 10s, or press Q)".into());
                if state.all_dead() {
                    return Ok(Control::Quit);
                }
            } else if state.should_quit && state.shutting_down {
                // Force quit during shutdown (Q pressed).
                return Ok(Control::Quit);
            }
            Ok(Control::Changed)
        }
        AppEvent::Timer(timeout) => {
            // Flush any buffered PTY writes before rendering. Key events
            // that forward to the PTY buffer bytes instead of writing
            // immediately, so rapid keystrokes (e.g. drag-and-drop
            // arriving as individual key events) are batched into a
            // single write(). The child process receives them in one
            // read() and echoes atomically - matching native terminal
            // behavior.
            state.flush_pty_buffers();

            // The render tick fires at ~120fps (8ms).  Heavy background
            // work only runs every BACKGROUND_TICK_DIVISOR-th tick to
            // keep CPU usage reasonable (~200ms cadence).
            const BACKGROUND_TICK_DIVISOR: usize = 25;
            let is_background_tick = timeout.counter % BACKGROUND_TICK_DIVISOR == 0;

            if is_background_tick {
                // Advance spinner for activity indicator animation.
                // Tick when status-bar activities exist OR when any work
                // item has Claude actively working (the list/board spinner
                // needs it).
                if !state.activities.is_empty() || !state.claude_working.is_empty() {
                    state.spinner_tick = state.spinner_tick.wrapping_add(1);
                }

                // Poll MCP status updates BEFORE liveness check so that a
                // review gate verdict arriving in the same tick as session
                // exit is processed before check_liveness clears
                // review_gate_wi.
                state.poll_mcp_status_updates();

                // Liveness check on all sessions.
                state.check_liveness();

                // Drain fetch results and reassemble if new data arrived.
                if state.drain_fetch_results() {
                    // Re-apply evictions so stale in-flight fetches don't
                    // resurrect recently-closed PRs in the unlinked list.
                    if !state.cleanup_evicted_branches.is_empty() {
                        state.apply_cleanup_evictions();
                    }
                    state.reassemble_work_items();
                    state.build_display_list();
                    state.global_mcp_context_dirty = true;
                }

                // Drain PR identity backfill results (one-time startup
                // migration).
                if state.drain_pr_identity_backfill() {
                    state.reassemble_work_items();
                    state.build_display_list();
                    state.global_mcp_context_dirty = true;
                }

                // Refresh dynamic context for the global MCP server only
                // when underlying data has changed, avoiding redundant
                // JSON serialization on every tick.
                if state.global_mcp_context_dirty && state.global_mcp_server.is_some() {
                    state.refresh_global_mcp_context();
                    state.global_mcp_context_dirty = false;
                }

                // Poll async operations. Capture status bar visibility
                // before and after so we can sync layout if an activity
                // started or ended.
                let had_status = state.has_visible_status_bar();

                // Poll async review gate result.
                state.poll_review_gate();

                // Poll async PR creation result.
                state.poll_pr_creation();

                // Poll async PR merge result.
                state.poll_pr_merge();

                // Poll async review submission result.
                state.poll_review_submission();

                // Poll mergequeue items for externally merged PRs.
                state.poll_mergequeue();

                // Poll async worktree creation result.
                state.poll_worktree_creation();

                // Poll async unlinked-item cleanup result.
                state.poll_unlinked_cleanup();

                // Surface queued fetch errors.
                state.drain_pending_fetch_errors();

                // If status bar visibility changed (activity started/
                // ended), resync layout so pane dimensions match the
                // actual display area.
                if state.has_visible_status_bar() != had_status {
                    event::sync_layout(state);
                }

                // Check for external signals (SIGTERM, SIGINT).
                if ctx.signal_received.swap(false, Ordering::Relaxed) {
                    if state.shutting_down {
                        // Second signal during shutdown - force kill and
                        // exit.
                        state.force_kill_all();
                        return Ok(Control::Quit);
                    } else {
                        // First signal - initiate graceful shutdown.
                        state.send_sigterm_all();
                        state.cleanup_all_mcp();
                        state.shutting_down = true;
                        state.shutdown_started = Some(std::time::Instant::now());
                        state.status_message =
                            Some("Waiting for sessions (force quit in 10s, or press Q)".into());
                        if state.all_dead() {
                            return Ok(Control::Quit);
                        }
                    }
                }

                // Shutdown deadline checks.
                if state.shutting_down {
                    if state.all_dead() {
                        return Ok(Control::Quit);
                    }
                    if state.should_quit {
                        return Ok(Control::Quit);
                    }
                    if let Some(started) = state.shutdown_started {
                        let elapsed = started.elapsed();
                        if elapsed >= Duration::from_secs(10) {
                            state.force_kill_all();
                            return Ok(Control::Quit);
                        }
                        let remaining = 10u64.saturating_sub(elapsed.as_secs());
                        state.status_message = Some(format!(
                            "Waiting for sessions (force quit in {remaining}s, or press Q)"
                        ));
                    }
                }

                // Restart the background fetcher if repo management
                // changed.
                if state.fetcher_repos_changed {
                    state.fetcher_repos_changed = false;
                    state.fetcher_disconnected = false;
                    // Stop the old fetcher.
                    if let Some(handle) = state.fetcher_handle.take() {
                        handle.stop();
                    }
                    state.fetch_rx = None;
                    // Start a new fetcher with the updated repo list.
                    let new_repos: Vec<PathBuf> = state
                        .active_repo_cache
                        .iter()
                        .filter(|r| r.git_dir_present)
                        .map(|r| r.path.clone())
                        .collect();
                    // Prune stale repo_data entries.
                    state.repo_data.retain(|k, _| new_repos.contains(k));
                    // Reassemble immediately so stale data is cleared.
                    state.reassemble_work_items();
                    state.build_display_list();
                    if !new_repos.is_empty() {
                        let new_extra = state.extra_branches_from_backend();
                        let (rx, handle) = fetcher::start_with_extra_branches(
                            new_repos,
                            Arc::clone(&ctx.worktree_service),
                            Arc::clone(&ctx.github_client),
                            state.config.defaults.branch_issue_pattern.clone(),
                            new_extra,
                        );
                        state.fetch_rx = Some(rx);
                        state.fetcher_handle = Some(handle);
                    }
                }
            }

            Ok(Control::Changed)
        }
        AppEvent::Rendered => Ok(Control::Continue),
        AppEvent::Message(_msg) => {
            // Future: handle inter-component messages (dialog results).
            Ok(Control::Continue)
        }
    }
}

/// Error callback. Re-raises I/O errors (terminal/poll failures should
/// exit cleanly). Non-fatal errors are downgraded to status messages.
pub fn app_error(
    err: AppError,
    state: &mut App,
    _ctx: &mut Global,
) -> Result<Control<AppEvent>, AppError> {
    match err {
        AppError::Io(_) => Err(err),
        _ => {
            state.status_message = Some(format!("Error: {err}"));
            Ok(Control::Changed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F-1: app_error re-raises I/O errors instead of swallowing them.
    /// Terminal and poll failures must propagate so rat-salsa exits
    /// cleanly rather than looping with a broken terminal.
    #[test]
    fn app_error_reraises_io_errors() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let mut state = App::new();
        let mut ctx = Global {
            ctx: SalsaAppContext::default(),
            theme: Theme::default_theme(),
            signal_received: Arc::new(AtomicBool::new(false)),
            worktree_service: Arc::new(crate::app::StubWorktreeService),
            github_client: Arc::new(crate::github_client::MockGithubClient::new()),
        };

        // I/O error should be re-raised (Err).
        let io_err = AppError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "terminal gone",
        ));
        let result = app_error(io_err, &mut state, &mut ctx);
        assert!(
            result.is_err(),
            "I/O errors should be re-raised, not swallowed",
        );

        // Non-fatal error should be downgraded to a status message (Ok).
        let general_err = AppError::General("channel recv error: empty".into());
        let result = app_error(general_err, &mut state, &mut ctx);
        assert!(
            result.is_ok(),
            "Non-fatal errors should be downgraded to status messages",
        );
        assert!(
            state
                .status_message
                .as_deref()
                .unwrap_or("")
                .contains("channel recv error"),
            "status message should contain the error text",
        );
    }
}
