# UI Architecture

WorkBridge uses [rat-salsa](https://github.com/thscharler/rat-salsa) as
the event loop framework with custom widgets for the main panels and
[rat-widget](https://github.com/thscharler/rat-salsa) components for
input dialogs.

## Event Loop

For the contract between workbridge and the external coding agent it
spawns (argv, MCP injection, session lifecycle, read-only sessions,
etc.) see `docs/harness-contract.md`; this file only covers what is
observable inside the TUI process.

The application runs via `rat_salsa::run_tui()` which manages terminal
setup/teardown, event polling, and the render cycle. Four callbacks drive
the application:

- **init**: starts the background fetcher, installs an 8ms render tick
  timer (see invariant 15), sets initial pane dimensions
- **render**: draws the UI to a Buffer (not a Frame)
- **event**: dispatches crossterm events to key/resize handlers, timer
  events to periodic work (liveness, fetch drain, signal checks, shutdown)
- **error**: surfaces errors in the status bar

The callbacks live in `src/salsa.rs`. The `Global` struct holds the
`SalsaAppContext`, theme, and signal flag. The `App` struct (in
`src/app.rs`) holds all mutable application state.

### Control Flow

Event handlers return `Control<AppEvent>`:
- `Control::Continue` - no re-render needed
- `Control::Changed` - trigger a re-render
- `Control::Quit` - exit the application

### Paste handling

`crossterm::event::Event::Paste` is delivered by the terminal when
bracketed paste is enabled. Modern terminal emulators (macOS
Terminal.app / iTerm2 / WezTerm / Ghostty, Windows Terminal, Linux
GNOME Terminal / Konsole / Alacritty / kitty) enable it by default.
The terminal emulator owns the Cmd+V / Ctrl+V / Ctrl+Shift+V keyboard
mapping, as well as drag-and-drop, "Paste" context menu items, and OSC
52 injection; workbridge never sees the modifier+key chord itself,
only the resulting `Event::Paste` payload.

`event::handle_paste` (`src/event.rs`) decides where the payload lands
based on modal state:

1. If the app is `shutting_down`, the paste is dropped (returns
   `false`).
2. If any modal overlay is up (`any_modal_visible` returns `true`),
   the paste is routed to the focused text input inside that modal
   via `route_paste_to_modal_input`. The precedence order mirrors
   `handle_key` exactly so paste and key events never diverge:
   set-branch dialog -> rework prompt -> cleanup reason input ->
   settings review-skill input (editing mode only) -> create dialog
   per-focus (Title / Description / Branch / Repos).
3. If no modal is up, the payload is wrapped in the bracketed-paste
   markers `\x1b[200~...\x1b[201~` and written to the focused PTY
   (global drawer PTY when the drawer is open, otherwise the right
   panel's active tab).

For every single-line `rat_widget::text_input::TextInputState` target
the payload is first passed through `flatten_paste_for_single_line`,
which replaces `\r\n`, `\n`, and `\r` with a single space each (CRLF
is collapsed first so a CRLF does not produce two spaces). The
Description field is a multi-line `rat_widget::textarea::TextAreaState`
and receives the payload verbatim, so newlines in a pasted payload
land as real line breaks.

Pasting into the Branch field also sets
`create_dialog.branch_user_edited = true`, matching the typing
behavior, so a subsequent Tab off Title cannot overwrite the pasted
branch via `auto_fill_branch`. Pasting into Title does NOT auto-fill
Branch on its own; auto-fill only runs on Tab off Title. Pasting into
a modal focus that has no text input (the Repos checkbox area in the
create dialog, the merge-strategy / delete / no-plan / branch-gone /
stale-worktree / cleanup confirmation prompts, in-progress spinners,
alert messages) is a silent no-op: the paste is dropped and no leak
reaches the PTY.

Returning `true` from `handle_paste` triggers a re-render (see
`src/salsa.rs` paste arm); returning `false` skips it.

Adding or modifying a TUI text input without wiring it into
`route_paste_to_modal_input`, or removing paste support from an
existing input, is a P1 (default-overridable) review finding - see
`CLAUDE.md` "Severity overrides". A session authorization naming the
specific field and rationale is required to ship an exception.

### Mouse Events

Mouse capture is enabled so the terminal forwards mouse events to the
application. The handler processes ScrollUp / ScrollDown,
Down(Left) / Up(Left) / Drag(Left); all other mouse events are
ignored.

When a mouse event arrives, the priority check first consults
`App::click_registry` - a per-frame table of rectangles pushed by the
renderer. The registry stores `ClickTarget` values in two variants:

- `ClickTarget::WorkItemRow { index }` - emitted once per visible row
  in the left-panel work item list. `Up(Left)` on the same row as the
  preceding `Down(Left)` selects the row.
- `ClickTarget::Copy { kind, value }` (where `kind` is `PrUrl`,
  `Branch`, `RepoPath`, or `Title`) - chrome labels in the right
  panel. A Down-Up pair on the same target copies the value to the
  clipboard.

Keeping row clicks and copy clicks in separate variants means
`short_display` / `fire_chrome_copy` never have to handle a row-click
payload, and `ClickKind` stays `Copy` / chrome-only.

**Modality note.** Copy clicks fire through the priority check even
when the global drawer is open, on the principle that labels "rendered
anywhere in chrome stay clickable" (see
`chrome_click_inside_global_drawer_still_fires`). Row clicks are
**suppressed** while `global_drawer_open` is `true`, because selection
has side effects (`selected_item`, `right_panel_tab`,
`recenter_viewport_on_selection`) that the user cannot see while the
drawer covers the list and would only discover after closing it. A
suppressed row click falls through to `mouse_target()`, which routes
the click to the drawer's own handler or to `MouseTarget::None` per
the drawer-open classification below.

If the registry does not hit (or the row-click is suppressed by the
drawer gate), `mouse_target()` classifies the cursor against
terminal-absolute coordinates:

1. **Global drawer** - checked first because it overlays everything.
   When the drawer is open, coordinates outside its inner area return
   `MouseTarget::None` so the dimmed background does not receive events.
2. **Right panel** - the per-work-item PTY session area (Claude Code
   or Terminal tab, depending on `right_panel_tab`).
3. **Work item list** - the left-panel list body. Wheel scrolls over
   this area drive the authoritative viewport offset
   (`App::list_scroll_offset`) without moving the selection. The body
   rect is stored on `App::work_item_list_body` each render and
   cleared implicitly when the renderer does not draw the list.

Scroll events over **PTY areas** drive a local scrollback viewport
rather than being forwarded directly to the child process:

- **Scroll-up** always enters or advances local scrollback mode. The
  viewport shifts into the scrollback buffer. Due to a limitation in
  vt100's `visible_rows()` API (usize underflow when offset exceeds
  terminal rows), the maximum scrollback depth is clamped to the
  terminal's row count (typically one screenful). These events are
  never forwarded to the PTY.
- **Scroll-down while in scrollback** moves the viewport back toward
  the live terminal. When the offset reaches 0, the user is back at
  the live view.
- **Scroll-down while NOT in scrollback** is forwarded to the child
  process, encoded according to its mouse protocol mode and encoding
  (queried from the vt100 parser). When the child has not enabled
  mouse reporting, scrolls are converted to arrow-key sequences.
- **Any keypress** while in scrollback mode resets the offset to 0,
  returning to the live terminal view. The key is still forwarded to
  the PTY so the user seamlessly resumes typing.

When scrollback mode is active, the panel title shows a [SCROLLBACK]
indicator so the user knows they are viewing history.

Scroll events over the **work item list** body are handled entirely
locally in `handle_work_item_list_scroll`:

- Wheel ticks move `App::list_scroll_offset` by 3 rows per tick (the
  same step size as the PTY scrollback helper), clamped to
  `[0, App::list_max_item_offset]`.
- Neither `App::selected_item` nor
  `App::recenter_viewport_on_selection` is touched. The viewport and
  the keyboard selection are deliberately decoupled: the user can
  wheel the viewport anywhere without losing their cursor, and can
  resume keyboard navigation at any time. The next `Up` / `Down` /
  `j` / `k` snaps the viewport back to the selection via the
  renderer's recenter pass.
- Left-click on a visible row (dispatched via the
  `ClickTarget::WorkItemRow` priority path) sets `selected_item`,
  sets `right_panel_tab = ClaudeCode`, arms
  `recenter_viewport_on_selection`, and calls `sync_layout` when the
  context-bar presence changes. When `global_drawer_open` is true,
  row-click dispatch is suppressed (see the modality note above) so
  selection does not change silently behind the drawer. Neither the
  wheel path nor the click-to-select path triggers remote I/O, so
  `App::try_begin_user_action` is not used here (the "User action
  guard" section below only applies to handlers that spawn
  `gh` / `git fetch` / network calls).

### Mouse Text Selection

When mouse capture is enabled, users can select text in PTY areas
(right panel, global drawer) by clicking and dragging with the left
mouse button. Selection works like a standard terminal emulator:

- Click-and-drag highlights the selected text (inverted colors).
- On mouse release, the selected text is automatically copied to the
  system clipboard.
- Clicking without dragging clears any existing selection.
- Any keypress clears the selection.
- Selection works in both live view and scrollback mode.

Mouse selection is only intercepted when the child process has NOT
enabled mouse reporting (MouseProtocolMode::None). When the child
has enabled mouse reporting (e.g., vim, htop), mouse events are
forwarded to the PTY as before. Exception: in local scrollback mode,
selection is always available since the PTY is not receiving events.

Clipboard writes go through `src/clipboard.rs::copy`, which attempts
BOTH an OSC 52 escape sequence (written directly to stdout) and
`arboard`. OSC 52 makes the copy work over SSH and inside tmux
(tmux users must have `set -g set-clipboard on` in their tmux.conf);
`arboard` covers terminals that strip OSC 52. Either path succeeding
counts as a successful copy. The escape sequence is built by
`osc52_sequence` so it can be unit-tested without hitting stdout.

### Interactive labels

In the work item detail view, four fields are click-to-copy:

- **Title** (the work item title span)
- **Repo** path
- **Branch** name
- **PR URL**

Each is rendered with `theme.style_interactive()` - the new
`interactive_fg` theme slot (default Cyan) plus a `Modifier::UNDERLINED`
affordance. The underline is the persistent visual signal that says
"clickable". A left-click on any of these fields copies the full
untruncated value via `clipboard::copy` and pushes a top-right toast
that auto-dismisses after ~2 seconds. Multiple toasts stack
vertically with the newest on top.

The toast text reflects the actual clipboard result, not the intent:
on success it reads `Copied: <short-value>`, on failure (both OSC 52
and `arboard` returned an error) it reads `Copy failed: <short-value>`.
Lying about the clipboard state is the worst UX failure mode for this
feature - a user who believes the copy succeeded will paste stale
content and only notice long after. `fire_chrome_copy` in `src/app.rs`
branches on the bool returned by `clipboard::copy`.

Implementation:

- Each renderer that draws an interactive label pushes a
  `ClickTarget { rect, value, kind }` into `App::click_registry`
  (a `RefCell<ClickRegistry>`) as part of its draw call. The rect
  is in **absolute frame coordinates** so it can be compared
  directly to `MouseEvent::column` / `row`.
- `draw_to_buffer` clears the registry at the top of every frame so
  stale targets from the previous draw never leak.
- `handle_mouse` consults the registry **before** the geometric PTY
  classification on every `Down(Left)` / `Up(Left)`: if the cursor
  is inside a registered rect, the event is routed to
  `handle_chrome_click_fallback` regardless of where
  `mouse_target` would have placed it. This priority rule is
  structural - interactive labels drawn anywhere in chrome (right
  panel detail view, global drawer, future overlays) stay clickable
  without per-site plumbing. Without it, labels drawn inside the
  right panel area would be classified as `MouseTarget::RightPanel`
  and swallowed by the text-selection branch. If the priority check
  does not fire, `handle_chrome_click_fallback` is also called as
  the `MouseTarget::None` arm so labels drawn outside all PTY areas
  remain reachable.
- `handle_chrome_click_fallback` arms a pending click on `Down(Left)`,
  cancels on any `Drag(Left)`, and fires `App::fire_chrome_copy` on
  a matching `Up(Left)`. The drag-cancel check runs unconditionally
  at the top of `handle_mouse` so a drag over a PTY pane still
  invalidates an in-flight click-to-copy gesture that started on a
  chrome label. A `SelectUp` that does NOT hit any registered target
  also unconditionally clears `pending_chrome_click`: this guards
  against terminals that coalesce or drop `Drag` events (some X10
  mouse modes, SSH sessions with packet loss), where a stale pending
  click could otherwise linger and fire a false copy on a later
  unrelated `SelectUp` over a same-kind label.
- Values that read as `"(none)"` are NOT registered and render in
  the muted style - the underline would be misleading if there is
  nothing to copy.

To add a new click-to-copy label elsewhere in the UI, follow the
same convention: style with `theme.style_interactive()`, push a
`ClickTarget` into `app.click_registry` with the absolute rect, and
pick (or add) a `ClickKind` variant.

### Blocking I/O Prohibition

The event loop runs on a single thread. Any blocking I/O (network
requests, git commands, file system operations that may be slow) will
freeze the UI until the operation completes. All I/O that could take
more than a few milliseconds must run on a background thread. This
prohibition is transitive: a trait method like
`WorktreeService::github_remote(...)` or `WorkItemBackend::read_plan(...)`
that shells out to `git` / `gh` or hits the filesystem counts as
blocking I/O and must not be called from the UI thread either.

Pattern for background I/O:

1. Create a `crossbeam_channel::bounded(1)` channel pair (tx, rx).
2. Spawn a `std::thread::spawn` closure that performs the I/O and sends
   the result through `tx`.
3. Store the `rx` receiver on the App struct.
4. Poll `rx.try_recv()` in a timer-driven poll method (called every
   ~200ms via the background-work throttle) to pick up results without
   blocking.

See `spawn_import_worktree()` and `poll_worktree_creation()` in
`src/app.rs` as reference implementations.

#### Prefer cached values over trait shell-outs

Many spawn helpers previously ran "fast" pre-flight checks synchronously
on the UI thread - `worktree_service.github_remote(...)` to look up the
`(owner, repo)` pair, `worktree_service.default_branch(...)` to find the
base for a diff, `backend.read_plan(...)` to read a plan file. Each
call is sub-millisecond in isolation but re-introduces the forbidden
blocking-I/O dependency on the main loop.

The rule is: **if the background fetcher has already cached the value,
read from the cache**. Concretely:

- `(owner, repo)` - read from
  `self.repo_data[repo_path].github_remote` (populated by
  `src/fetcher.rs::fetcher_loop`). If the cache is empty (first fetch
  in flight), surface that to the user via `alert_message` / a status
  bar message rather than blocking.
- Worktree / branch metadata - read from
  `self.repo_data[repo_path].worktrees` (which carries
  `has_commits_ahead` so `branch_has_commits` is a pure cache lookup,
  plus `dirty` / `untracked` / `unpushed` / `behind_remote` so the
  `format_work_item_entry` `!cl`, `!pushed`, and `!pulled` chip
  renderers can flag an unclean or out-of-sync worktree without
  shelling out). `!cl` is exclusively for "uncommitted changes in
  the worktree" (a clean-but-diverged branch no longer triggers
  it); `!pushed` fires on `git_state.ahead > 0` and `!pulled` fires
  on `git_state.behind > 0`, so a dirty + diverged row renders all
  three chips alongside each other. The Review -> Done **merge
  guard** runs a live `WorktreeService::list_worktrees` precheck on
  a background thread (`App::spawn_merge_precheck` /
  `App::poll_merge_precheck`) before letting the actual `gh pr merge`
  thread fire - the cache stays authoritative for the `!cl` chip but
  is NEVER consulted for the irrevocable merge decision, because long
  sessions can leave the cached `dirty: true` value stale long after
  the user has committed and pushed. The classification is done via
  `WorktreeCleanliness::from_worktree_info` against the live
  `WorktreeInfo` so the precheck and the chip render share one
  canonical priority ordering and wording. `App::advance_stage` does
  NOT do its own cleanliness check on the Review -> Done branch: it
  unconditionally opens the merge confirm modal, and the live
  precheck inside `execute_merge` is the only authority. (The
  earlier cached guard in `advance_stage` was the source of a stale-
  cache regression where users could not merge after committing
  because the fetcher cache had not refreshed yet.) The merge guard
  reserves its `UserActionKey::PrMerge` slot in `execute_merge`
  BEFORE spawning the precheck and only releases it on the Blocked /
  disconnected branches of `poll_merge_precheck`, so the precheck
  and the actual merge share one single-flight slot across both
  phases.
- Backend file reads (`read_plan`, `list`) - clone the `Arc<dyn
  WorkItemBackend>` into the background closure and run the read there.
- `default_branch`, `github_remote`, `git diff` - clone the
  `Arc<dyn WorktreeService>` into the background closure and run the
  shell-out there. NEVER on the UI thread.

Examples of the cache-first pattern:
- `spawn_pr_creation`, `execute_merge`, `enter_mergequeue`,
  `spawn_review_submission`, `collect_backfill_requests`,
  `reconstruct_mergequeue_watches` all read github_remote from
  `repo_data`.
- `spawn_review_gate` performs every blocking step
  (`backend.read_plan`, `default_branch`, `git diff`, `github_remote`)
  inside its `std::thread::spawn` closure and reports any "cannot run"
  discovery via `ReviewGateMessage::Blocked` so `poll_review_gate`
  applies the rework flow without freezing the UI.
- Review gates carry a `ReviewGateOrigin` tag so `poll_review_gate`
  can dispatch Blocked outcomes correctly. `Mcp` and `Auto` origins
  (Claude requested Review via the MCP status update, or the
  auto-trigger after an Implementing session died) run the full
  rework flow: populate `rework_reasons`, kill the session, respawn
  it with the implementing_rework prompt so Claude sees the reason.
  `Tui` origin (the user pressed `l` on an Implementing item) only
  surfaces the reason in the status bar and leaves the session
  running - killing the user's primary workspace would be a
  destructive regression from the master behaviour. In all origins,
  if the work item was deleted while the gate was in flight, only
  the gate state is dropped; `rework_reasons` is not populated,
  preventing an orphan entry that nothing would ever clear.
- The auto-trigger from a dying Implementing session
  (`reassemble_work_items` -> `check_liveness` retroactive branch)
  does NOT gate on `branch_has_commits`. The background gate's
  `git diff default..branch` is the source of truth: if the branch
  has no changes it arrives as `ReviewGateMessage::Blocked("Cannot
  enter Review: no changes on branch")`, which `poll_review_gate`
  surfaces via the Auto-origin rework flow. Gating the spawn on a
  stale fetcher cache would let items get stuck in Implementing for
  up to two minutes after Claude's final commit (the fetch
  interval), with no auto-retry.
- `spawn_session` routes through `begin_session_open` +
  `poll_session_opens` instead of calling `stage_system_prompt`
  directly. `begin_session_open` spawns a background thread that
  runs **every** blocking step the spawn needs: `backend.read_plan(...)`,
  `McpSocketServer::start(...)` (which binds the socket and spawns
  the accept loop), `AgentBackend::write_session_files(...)` (the
  worktree `.mcp.json` for Claude Code's project discovery), and
  the `std::fs::write` on the temp `--mcp-config` file. It then
  sends a `SessionOpenPlanResult` through a per-work-item receiver.
  `poll_session_opens` drains completed receivers and invokes
  `finish_session_open`, which is pure-CPU work plus the
  `Session::spawn` fork+exec - no filesystem I/O runs on the UI
  thread. `stage_system_prompt` consumes the pre-read plan text
  as a parameter and MUST NOT call `backend.read_plan(...)` itself.
  The receiver map is keyed by `WorkItemId` so parallel session
  opens for different items cannot collide.
- `spawn_global_session` uses the same two-phase pattern: the UI
  thread precomputes the system prompt and shared MCP context,
  then spawns a worker that starts the global MCP server, writes
  the temp `--mcp-config` file, creates the scratch cwd via
  `std::fs::create_dir_all`, and calls `Session::spawn` itself.
  The result flows back through `GlobalSessionOpenPending::rx` and
  `poll_global_session_open` moves the handles into
  `App::global_session` / `App::global_mcp_server` /
  `App::global_mcp_config_path`. Teardown
  (`teardown_global_session`) cancels any in-flight preparation
  by dropping the pending entry and routes the temp config file
  removal through `App::spawn_agent_file_cleanup` so the
  `std::fs::remove_file` runs on a background thread.

#### Streaming progress variant

When a background task needs to send intermediate progress updates
before a final result, use `crossbeam_channel::unbounded()` instead of
`bounded(1)`. Define an enum with Progress and Result variants. The
background thread sends zero or more Progress messages followed by one
Result. The poll method uses a drain loop:

```
loop {
    match rx.try_recv() {
        Ok(Progress(text)) => update progress field, continue
        Ok(Result(r))      => process final result, break
        Err(Empty)         => break (no more messages this tick)
        Err(Disconnected)  => thread died without Result, handle error
    }
}
```

The sender detects cancellation when `tx.send()` returns `Err`
(receiver dropped). Long-running poll loops should include a timeout
to prevent indefinite thread leaks.

See `spawn_review_gate()` and `poll_review_gate()` in `src/app.rs`.

Examples of operations that MUST be async:
- `git fetch`, `git worktree add`, `git clone`
- GitHub API calls (`gh pr list`, `gh api`)
- Any `std::process::Command::output()` call
- Large file reads/writes

### Timer and Render Tick

The application uses a single 8ms (~120fps) repeating timer that serves
two purposes:

1. **Render tick** (every 8ms): every timer fire returns
   `Control::Changed`, triggering a re-render. This keeps embedded PTY
   output smooth - reader threads update the vt100 parser continuously,
   but only a re-render makes changes visible.

2. **Background work tick** (every 25th fire, ~200ms): heavy periodic
   work is throttled via `timeout.counter % BACKGROUND_TICK_DIVISOR == 0`
   to avoid wasting CPU. This drives:
   - Session liveness checks (`check_liveness`)
   - Fetch result drain (`drain_fetch_results`)
   - Pending fetch error drain
   - Signal handling (SIGTERM/SIGINT via AtomicBool)
   - Shutdown deadline enforcement (10s)
   - Fetcher restart when managed repos change

See invariant 15 for the render rate requirement.

## View Modes

The `ViewMode` enum controls the root overview layout:

- `FlatList` (default): two-panel layout with work item list (left) and
  PTY session (right). The right panel has two tabs: Claude Code and
  Terminal. See Layout and Right Panel Tabs sections below.
- `Board`: kanban board with 4 columns organized by workflow stage.
  See Board View section below.
- `Dashboard`: global flow-metrics view (throughput, cycle time,
  backlog size over time, stuck items). Reads from the background
  metrics aggregator - no per-work-item interaction. See
  `docs/metrics.md` for the data model and bucketing rules, and the
  Dashboard View section below for the layout. Number keys `1 2 3 4`
  select the rolling time window (7d / 30d / 90d / 365d) while this
  view is active.

Cycle between modes with Tab (FlatList -> Board -> Dashboard ->
FlatList). The selected work item is preserved across toggles. A
1-row view mode header at the top of the screen shows a segmented
tab bar (using the ratatui `Tabs` widget) with `List`, `Board`, and
`Dashboard` labels. The active mode is highlighted. Contextual
keybinding hints appear right-aligned in the header (e.g., board
mode shows arrow key and Shift+arrow controls; dashboard mode shows
the window-selection number keys).

## Global Shortcuts

These shortcuts are intercepted in `handle_key()` after dialog/overlay
checks but before panel-specific handlers. They work in both flat list
and board views but not inside open dialogs or overlays.

- Ctrl+R: force refresh GitHub data. Restarts the background fetcher,
  triggering an immediate fetch cycle. The status bar shows the
  "Refreshing GitHub data" spinner during the fetch, using the same
  code path as the periodic 120-second auto-refresh.
- Ctrl+\\: cycle the right-panel tab between Claude Code and Terminal.
  Works from both panels without changing focus, so the user can flip
  the right panel without leaving the work item list and (more
  importantly) can flip the tab from inside the PTY - plain Tab is
  forwarded to the PTY so Claude Code's autocomplete works, which
  means the tab switcher can't live on Tab itself. The
  `ClaudeCode -> Terminal` transition is a no-op if the selected work
  item has no worktree.

## Focus Model

### Top-Level: Left/Right Panel (Flat List Mode)

The `FocusPanel` enum tracks whether the left panel (work item list) or
right panel (PTY session) has focus. This is NOT managed by rat-focus
because the right panel forwards almost all keys to the PTY, which is
incompatible with rat-focus's widget navigation model.

- Enter on a work item: focus right panel
- Ctrl+\\: cycle between Claude Code and Terminal tabs (global, does
  not change focus - see "Global Shortcuts" above)
- Ctrl+]: return to left panel
- Ctrl+D / Delete: delete selected work item (modal confirmation)
- o (left panel only): open the selected entry's PR in the default
  browser via `open`. Works on work items (first repo association with
  a PR wins), unlinked PRs, and review requests. Sets a "No PR to open"
  status message on selections that have no PR (group headers, work
  items with no PR yet). Not bound on the right panel because single
  keystrokes there forward to the PTY. The `open` subprocess is
  spawned on a background thread so a stalled launch cannot block the
  UI event loop (see "Blocking I/O Prohibition" below).
- m (left panel only): rebase the selected work item's branch onto
  the latest upstream main. Spawns a background thread that runs
  `git fetch origin <main>` and then a headless harness instance
  (cwd = the work item's worktree, MCP injected) which runs
  `git rebase origin/<main>` and resolves any conflicts in place. The
  user is never asked anything; on a give-up the harness aborts the
  rebase and the UI surfaces the failure reason. No `git push` is
  performed - after a successful rebase the user will see `!pushed`
  (and likely `!pulled`) and pushes manually. Single-flight via
  `UserActionKey::RebaseOnMain` with a 500 ms debounce, so rapid `m`
  presses are coalesced. No-op with a "No branch to rebase" status
  message on selections that are not work items, are unlinked PRs /
  review requests, or have no worktree association. Not bound on the
  right panel because single keystrokes there forward to the PTY.
- Dead session: auto-return to left panel
- Up/Down in left panel: reset right panel tab to Claude Code

### Board Mode Navigation

In board view, `handle_key_board()` intercepts key events before the
left/right panel handlers. The `BoardCursor` struct tracks column index
and row index independently.

- Left/Right arrows: move between columns
- Up/Down arrows: move between items within a column
- Shift+Right: advance item to next stage
- Shift+Left: retreat item to previous stage
- Ctrl+D / Delete: delete selected work item (modal confirmation)
- Enter: drill down into selected column (filtered flat list + PTY)
- Ctrl+]: return from drill-down to board view

After a stage transition (Shift+arrow), the cursor follows the item
into its new column.

### Dashboard Mode Navigation

In Dashboard view, `handle_key_dashboard()` intercepts key events
before the left/right panel handlers. The view has no per-item
cursor - all charts are global, so navigation is limited to
switching the rolling time window and cycling out via Tab.

- `1`: 7-day window
- `2`: 30-day window (default on launch)
- `3`: 90-day window
- `4`: 365-day window
- Tab: cycle to List view
- Q / Ctrl+Q: quit with confirmation (same semantics as other views)
- `?`: open settings overlay

The selected window persists only for the session; it resets to 30d
on each launch. The charts re-read `App.metrics_snapshot` on every
render; the snapshot itself is refreshed by the background
aggregator every ~60s (see `docs/metrics.md`).

### Within Dialogs

Dialogs and prompt overlays intercept all key events before the
left/right panel handlers. The creation dialog cycles through fields
with Tab/Shift+Tab. Prompt dialogs have simpler focus (one active
element at a time).

## Adding a New Overlay

There are two overlay patterns depending on complexity.

### Full Dialog (complex forms, multi-field input)

Use for dialogs with multiple fields, validation, or complex focus.
See `src/create_dialog.rs` as the reference implementation.

1. Create a dialog struct in `src/<dialog_name>.rs` with:
   - `visible: bool`
   - Input state fields (`rat_widget::text_input::TextInputState` for
     single-line text, `rat_widget::textarea::TextAreaState` for
     multi-line text, `Vec` for lists)
   - Focus tracking enum
   - `open()`, `close()`, `handle_key()` methods

2. Add the dialog field to `App` in `src/app.rs`

3. In `src/event.rs`, add an intercept block near the top of `handle_key`:
   ```rust
   if app.<dialog>.visible {
       handle_<dialog>_key(app, key);
       return true;
   }
   ```

4. In `src/ui.rs`, add rendering at the end of `draw_to_buffer()` (after
   prompt dialogs, before or after global drawer as appropriate):
   ```rust
   if app.<dialog>.visible {
       draw_<dialog>(buf, &app.<dialog>, theme, area);
   }
   ```

5. Wire the trigger key in `handle_key_left`

### Prompt Dialog (simple choice or single text input)

Use for blocking prompts that require a choice or a single text field.
These are rendered via the shared `draw_prompt_dialog()` function with
`PromptDialogKind` - no separate struct or file needed.

State fields live directly on `App`:

```rust
pub my_prompt_visible: bool,
pub my_prompt_input: rat_widget::text_input::TextInputState, // if text input needed
pub my_prompt_target: Option<MyTarget>,  // relevant context
```

Key handler function in `event.rs`:

```rust
fn handle_my_prompt(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | _ => { /* cancel */ }
        (_, KeyCode::Char('y')) => { /* confirm */ }
    }
}
```

Intercept in `handle_key()`:

```rust
if app.my_prompt_visible {
    handle_my_prompt(app, key);
    return true;
}
```

Rendering in `draw_to_buffer()` (in the prompt dialog block):

```rust
} else if app.my_prompt_visible {
    draw_prompt_dialog(buf, theme, area, PromptDialogKind::KeyChoice {
        title: "My Prompt",
        body: "Are you sure?",
        options: &[("[y]", "Yes"), ("[Esc]", "Cancel")],
    });
}
```

Do NOT set `app.status_message` to show prompt content - the dialog
renders its own content.

For errors that require acknowledgment from async operations or other
flows, use `app.alert_message` (a general-purpose facility):

```rust
// On error:
app.alert_message = Some(format!("Operation failed: {e}"));
// The alert dialog dismisses itself when the user presses Enter or Esc.
```

### Set branch recovery dialog

WorkBridge enforces a **branch invariant** at the Backlog -> Planning
transition: a work item may not leave Backlog unless at least one of
its `repo_associations` has a branch name. The check lives in
`App::advance_stage()` at the top of the function and gates both
user-driven advances and MCP-driven advances that flow through the same
helper. When the invariant fails, `advance_stage` does NOT call
`apply_stage_change`; it opens the "Set branch name" modal via
`App::open_set_branch_dialog(wi_id, PendingBranchAction::Advance {
from, to })` instead.

The same modal is opened from `App::spawn_session()` when the user
presses Enter on a Planning/Implementing work item whose repo
associations all have `branch.is_none()`. Before this dialog existed,
that path ended in a dead-end `"Set a branch name to start working"`
status message with no way to recover short of editing JSON by hand.

The modal reuses `PromptDialogKind::TextInput` and rat-widget's
`TextInputState` (see `src/ui.rs` and `src/create_dialog.rs`). State lives on
`App::set_branch_dialog: Option<SetBranchDialog>` and carries a
`PendingBranchAction` so the dialog knows what to re-drive after the
branch is persisted. Behaviour:

- **Open**: prefills `{$USER}/{slug}-{suffix}` from the work item's
  title using the same slug helpers the create dialog uses.
- **Enter**: calls `backend.update_branch(...)` on every repo
  association that currently has `branch.is_none()`, reassembles the
  work-item list, then re-drives the pending action (either
  `spawn_session` or `apply_stage_change`). If the backend write
  fails the dialog stays open with an error on the status bar so the
  user can retry.
- **Esc**: dismisses the dialog without touching the backend.

The key intercept in `src/event.rs` sits near the top of `handle_key`,
above the general work-item keybindings, so `d` / `q` / `Enter` are
treated as branch-name input rather than delete / quit / advance while
the dialog is visible.

### Activity indicator placement

Background operations have exactly two valid progress-feedback idioms.
Every long-running background op owes the user one of these - never
neither, never both.

1. **Blocking dialog with inline spinner** - for operations initiated
   from an already-open dialog. The dialog stays open with a spinner
   until the operation completes. The user triggered the action and
   is watching the dialog - progress and errors must appear where they
   are looking. Never close the dialog and move feedback to the status
   bar; that disconnects the result from the action. Examples: the
   delete modal, the merge strategy modal, the unlinked-PR cleanup
   modal.
2. **Status-bar activity** - for operations initiated outside any
   dialog: system-initiated startup work, automatic transitions, and
   user presses that don't open a dialog. Use `start_activity(...)`
   and end the returned `ActivityId` on every terminal path of the
   operation's poll function. The `ActivityId` must always be
   reachable from a stable owner so every drop site can route through
   one helper and end the spinner in exactly one place. Three valid
   forms of structural ownership:
   - **Per-owner state struct** for long-lived operations keyed by
     a work item or session (e.g. `ReviewGateState.activity`,
     `SessionOpenPending.activity`, `PrMergePollState.activity`).
   - **Closure-captured + completion message** for fire-and-forget
     spawns with no persistent state (e.g. the `activity` field on
     `OrphanCleanupFinished`, which the worker thread captures and
     echoes back in its one-shot completion message).
   - **Singleton `App` field** for one-shot app-wide migrations
     where exactly one is ever in flight (e.g.
     `App.pr_identity_backfill_activity` for the startup PR identity
     backfill).
   Examples: the background fetcher, PR creation, mergequeue polling,
   review submission, session-open plan read, the review gate, the
   one-time PR identity backfill migration, the orphan worktree
   cleanup.

The "Session Activity Indicators" described later in this document
(the left-panel list badges that reflect whether a session is alive
and whether Claude has signalled active work via MCP) are a
**complementary** per-item view, NOT a substitute for one of the two
primary idioms. Every background operation still owes a dialog
spinner OR a status-bar activity regardless of whether an item badge
also exists.

The structural-ownership rule applies without exception across all
three forms above: the `ActivityId` must always be reachable from the
owning state struct, completion message, or singleton `App` field -
never a free-floating `Option<ActivityId>` sitting next to an
`Option<Receiver>` that requires manual correlation to find its
matching drop site.

Pattern for user-initiated dialog operations:

1. Add a `<name>_in_progress: bool` field to App.
2. When the user confirms in the dialog handler, call the spawn function
   but do NOT close the dialog (the `_visible` / `confirm_` flag stays
   true).
3. The spawn function sets `in_progress = true` and spawns the thread.
4. In the UI rendering, check `in_progress`: when true, render the
   dialog body with a spinner and empty options (no key choices).
5. In the event handler, add an `in_progress` guard that swallows all
   keys except Q/Ctrl+Q (force quit). This prevents the user from
   dismissing or re-triggering the dialog while the operation runs.
6. In the poll function, on completion (success or failure):
   - Set `in_progress = false`
   - Close the dialog
   - For errors: use `app.alert_message` (red alert dialog), NOT
     `app.status_message` (transient status bar text)
   - For success: use `app.status_message` for positive confirmation

Reference implementations:
- Cleanup dialog: `spawn_unlinked_cleanup()`, `poll_unlinked_cleanup()`,
  `is_user_action_in_flight(&UserActionKey::UnlinkedCleanup)` read by
  event.rs / ui.rs / salsa.rs (see "User action guard" below).
- Merge dialog: `execute_merge()`, `poll_pr_merge()`,
  `merge_in_progress` guard in event.rs.

### User action guard

Every user-initiated remote-I/O spawn on `App` (PR create / merge /
review, worktree create, unlinked-PR cleanup, delete cleanup, Ctrl+R
refresh) is admitted through a single helper:

```rust
if let Some(activity_id) = app.try_begin_user_action(
    UserActionKey::PrCreate,
    Duration::ZERO,         // or Duration::from_millis(500) for Ctrl+R
    "Creating pull request...",
) {
    // Spawn the background thread here.
    app.attach_user_action_payload(
        &UserActionKey::PrCreate,
        UserActionPayload::PrCreate { rx, wi_id },
    );
}
```

The helper owns a single `HashMap<UserActionKey, UserActionState>`
backing three methods:

- `try_begin_user_action(key, debounce, message)` - returns
  `Some(ActivityId)` if the key is free AND the debounce window has
  elapsed since the last attempt. Starts a status-bar activity and
  inserts the map entry. Returns `None` otherwise. **Does NOT emit any
  status message or alert on rejection** - every caller owns its
  rejection UX and the wording is caller-specific.
- `attach_user_action_payload(&key, payload)` - attaches the
  background-thread receiver (and any metadata like `wi_id`) to the
  entry so the same drop site ends both the activity and the channel.
- `end_user_action(&key)` - removes the entry and ends the activity.
  Idempotent, so cancel / retreat / delete paths can call it blindly.
- `is_user_action_in_flight(&key)` - pure in-memory boolean used by
  UI / event / salsa code to gate behaviour on the guard state. Safe
  on the UI thread because it never touches I/O.

Key semantics:

- **One slot per `UserActionKey` variant** (`PrCreate`, `PrMerge`,
  `ReviewSubmit`, `WorktreeCreate`, `UnlinkedCleanup`, `DeleteCleanup`,
  `GithubRefresh`). Single-flight per-action is the point. Per-item
  concurrency (e.g. parallel worktree creation for different repos) is
  intentionally out of scope; if it is ever wanted, key the variant on
  `(RepoPath, Branch)` instead of a bare discriminant.
- **Debounce is caller-specified.** Only Ctrl+R currently passes a
  nonzero value (500 ms); every other caller passes `Duration::ZERO`
  so only in-flight state gates admission.
- **Desync guard:** every `spawn_*` must run ALL validity checks
  (missing repo, missing branch, missing `github_remote` cache) BEFORE
  `try_begin_user_action`, so an early return cannot leave an
  orphaned helper entry. If the helper accepts, the only remaining
  failure modes are the background-thread disconnect (handled in the
  matching `poll_*`) and the explicit retreat / delete cancel paths
  (which call `end_user_action` idempotently).
- **Structural fetcher restarts do not go through the helper.** The
  `fetcher_repos_changed` flag is set by ~11 structural sites in
  `src/app.rs` when the managed-repo set changes; `salsa.rs` honours
  the flag by stopping the old fetcher and starting a new one. Only
  the explicit Ctrl+R press goes through `UserActionKey::GithubRefresh`
  - everything else is "repo set changed", not "user wants fresh
  data", and must not be debounced.
- **At most one fetch spinner.** `drain_fetch_results` checks
  `is_user_action_in_flight(&GithubRefresh)` on `FetchStarted`; if
  true the helper entry's activity is reused, otherwise a local
  `structural_fetch_activity: Option<ActivityId>` field owns the
  spinner for that cycle. When `pending_fetch_count` returns to zero
  both owners are cleared.
- **The `GithubRefresh` helper entry is intentionally short-lived.**
  The Ctrl+R event handler admits the helper entry, sets
  `fetcher_repos_changed = true`, and returns. On the next salsa tick
  the restart block calls `end_user_action(&GithubRefresh)` BEFORE
  stopping the old fetcher (`src/salsa.rs`), so the helper entry
  exists only for the few milliseconds between the keypress and the
  restart. The practical consequence is that spam protection between
  two rapid Ctrl+R presses comes from the 500 ms **debounce**
  (`last_attempted` timestamp), not from the in-flight check -
  `is_user_action_in_flight(&GithubRefresh)` is almost always false
  by the time a second press arrives. The visible spinner for the
  resulting fetch cycle is owned by `structural_fetch_activity` in
  `drain_fetch_results`, not by the (already-dropped) helper entry.
  This is by design: it keeps the single-spinner invariant intact and
  makes the structural restart path the one true source of the
  fetch-spinner activity regardless of whether the refresh was
  user-initiated or triggered by a repo-set change.
- **Handoff at structural restart.** The restart block in
  `src/salsa.rs` calls `end_user_action(&GithubRefresh)` BEFORE
  stopping the old fetcher so a structural restart mid-Ctrl+R never
  leaves the helper with a stale entry pointing at a dead fetcher.
- **Modal-owned spinners hide the status-bar activity.** Both the
  unlinked cleanup modal and the delete cleanup modal already render
  their own in-progress spinners; to avoid a stacked duplicate, those
  spawn functions admit the helper entry and then immediately
  `end_activity(activity_id)` on the returned ID. The map entry stays
  alive so `is_user_action_in_flight` keeps reporting the true state;
  only the visible spinner is suppressed. `execute_merge` follows the
  same rule: the merge confirmation modal renders its own
  "Checking working tree..." / "Merging pull request..." spinner from
  the moment the user pressed merge, so the status-bar activity is
  ended via `end_activity` immediately after `try_begin_user_action`
  returns.
- **Two-phase admission (PrMerge).** `execute_merge` admits the
  `UserActionKey::PrMerge` slot BEFORE the live working-tree precheck
  spawns, and the helper entry is held across both the precheck phase
  (`spawn_merge_precheck` / `poll_merge_precheck`) AND the actual
  `gh pr merge` phase (`perform_merge_after_precheck` /
  `poll_pr_merge`). Only the Blocked / disconnected branches of
  `poll_merge_precheck` and the terminal arms of `poll_pr_merge`
  release the slot. `perform_merge_after_precheck` does NOT re-admit
  the key; it only swaps the payload from
  `UserActionPayload::PrMergePrecheck` to
  `UserActionPayload::PrMerge` via `attach_user_action_payload`,
  which drops the precheck receiver in the same step. This keeps
  the helper's single-flight invariant intact across the
  precheck-to-merge handoff while still letting the precheck thread
  run on a background thread without blocking the UI.

  Both phases own their receivers structurally inside the helper
  slot's payload: the precheck rx lives in `PrMergePrecheck { rx }`
  and the merge rx lives in `PrMerge { rx }`. There is no sibling
  `Option<Receiver>` field on `App`. Every cancellation site that
  calls `end_user_action(&UserActionKey::PrMerge)` (currently
  `retreat_stage` and `delete_work_item_by_id`) drops the entire
  helper entry, which drops whichever payload it held, which drops
  the receiver - one structural step, no lockstep clears to forget.
  This is the "structural ownership over manual correlation"
  pattern from `CLAUDE.md` applied to the merge precheck. The UI
  uses `App::is_merge_precheck_phase()` to switch the modal body
  between "Checking working tree..." and "Merging pull request..."
  by inspecting the payload variant directly.
- **Caller-local rejection messages.** When `try_begin_user_action`
  returns `None`, the caller re-adds its existing alert / status
  string verbatim. `execute_merge` keeps `"PR merge already in
  progress"` as an `alert_message`; `spawn_delete_cleanup` keeps its
  long delete-cleanup alert; the others keep their existing
  `status_message` strings.

Receivers, activity IDs, and per-action metadata all live inside the
map entry's `UserActionPayload`, never as free-standing `Option` fields
on `App`. That is the structural-ownership rule from `CLAUDE.md`
applied to single-flight admission: dropping the map entry drops the
receiver, the activity, and any `WorkItemId` in lockstep, and there is
no way to forget an `if owner_matches` check at a cancel site.

Reference implementations:

- `spawn_pr_creation` / `poll_pr_creation` - bespoke
  `pr_create_pending` queue sits outside the helper (queueing
  semantics are PR-create-specific; the helper only admits the next
  in-flight entry).
- `execute_merge` / `poll_pr_merge`.
- `spawn_review_submission` / `poll_review_submission`.
- `spawn_session` + `spawn_import_worktree` - shared
  `UserActionKey::WorktreeCreate` slot; two callers compete for one
  global admission slot by design.
- `spawn_unlinked_cleanup` / `poll_unlinked_cleanup`.
- `spawn_delete_cleanup` / `poll_delete_cleanup`.
- `drain_fetch_results` / `src/event.rs` Ctrl+R handler /
  `src/salsa.rs` restart block.

## Rendering

All rendering is Buffer-based (not Frame-based). Widgets use the
`Widget::render(self, area, buf)` and
`StatefulWidget::render(widget, area, buf, &mut state)` patterns from
ratatui-core.

### List / viewport / scrollbar

The left-panel work item list uses a **decoupled viewport model**:
`App::list_scroll_offset` (a `Cell<usize>` for interior mutability
during render) is **authoritative** for the viewport position, not
derived from the selection. This is what makes mouse-wheel scrolling
possible without the viewport snapping back to the selection on
every frame.

Three inputs can mutate the offset:

1. **Mouse wheel** over the list body - `handle_work_item_list_scroll`
   adds / subtracts 3 rows per tick, clamped to
   `[0, App::list_max_item_offset]`. Selection is not touched.
2. **Keyboard navigation** - `select_next_item` / `select_prev_item`
   (and any future selection mover) sets
   `App::recenter_viewport_on_selection` to `true`. The next render
   consumes the flag and computes an offset that centers the
   selection in the visible body, clamped at both ends of the list.
3. **List-length clamp** - each render computes
   `max_item_offset = compute_max_item_offset(item_heights,
   body_height)` and clamps the read offset so a list that just
   shrank below the current offset rolls back to the tail without a
   dangling gap.

The offset is also reset to 0 in `build_display_list()` whenever the
display list is rebuilt (view mode toggle, drill-down, item deletion,
fetch cycle). This prevents stale offsets from a previous list shape
carrying over into a structurally different list.

The render flow in `draw_work_item_list` (ui.rs):

1. Compute per-item `item_heights` for the current display list.
2. If `recenter_viewport_on_selection.take()` returned `true` and
   there is a selection, compute a tentative offset via
   `recenter_offset(&item_heights, selected, inner.height)`. Otherwise
   read `list_scroll_offset.get()` directly.
3. Decide whether to reserve a sticky-header slot based on the
   tentative offset (so the sticky fires on the same frame the
   recenter takes effect).
4. Reconcile the offset against the final `body_height` (which may
   be `inner.height - 1` if the sticky slot was reserved). If
   `want_recenter` is true, recompute with the smaller height; in all
   cases clamp to `max_item_offset`.
5. Write the final offset back to `App::list_scroll_offset`; write
   the body rect to `App::work_item_list_body` (for mouse routing)
   and the max offset to `App::list_max_item_offset` (for the
   wheel-scroll handler's clamp).
6. Build the `List` widget, but DO NOT call `ListState::select(...)`.
   The selection highlight is applied per-item in the `ListItem`
   construction (the selected row's background is set to
   `style_tab_highlight_bg`). Letting ratatui own selection would
   re-introduce `get_items_bounds`'s auto-scroll-to-selection and
   defeat the decoupled viewport.
7. Push a `ClickTarget::WorkItemRow { index }` click target for each
   visible selectable row, so `handle_mouse` can route a left-click
   back to the row index without redoing layout math.
8. Render the scrollbar as before; then overlay a single cyan
   filled-block cell in the scrollbar column at the y-coordinate
   corresponding to `selected_item`'s row within the full list, but
   **only when the selection is outside the visible viewport**. This
   is the "your keyboard selection is here" affordance that tells the
   user where they parked their cursor while they wheel-scroll the
   viewport elsewhere. The marker uses
   `theme.scrollbar_selection_marker` (Cyan by default) to stay
   distinct from the gray scrollbar thumb.

### ACTIVE Group Intra-Stage Ordering

Inside each `ACTIVE (<repo>)` sub-group, items are sorted in workflow
order: Planning, then Implementing, then Review, then Mergequeue.
Within a single stage, items keep the deterministic backend path
order as a stable-sort tiebreaker, so single-stage repos render in
exactly the same order as before. The sort is implemented by
`WorkItemStatus::active_group_rank` in `src/work_item.rs` and applied
in `push_repo_groups` in `src/app.rs`. This rule does not apply to
the `BLOCKED`, `BACKLOGGED`, or `DONE` groups, whose items all share
a single status by construction and therefore sort as a no-op.

### Sticky Group Headers

When the left-panel work item list is scrolled down and a group header
(e.g., "ACTIVE (repo)", "BACKLOGGED (repo)") has scrolled above the
viewport, a "sticky" copy of that header is rendered at the top of the
list's inner area. This ensures the user always knows which group the
currently visible items belong to.

The sticky header uses a dedicated reserved row, not an overlay. In
`draw_work_item_list` the left-panel `Block` is rendered into `area`
first, then the inner area is split: if the frame is predicted to need
a sticky header, the first row of the inner area is reserved as a
sticky slot (1 row) and the `List` widget is rendered into a
`body_area` that starts one row below. The sticky row itself is drawn
as a `Paragraph` into the reserved slot. This guarantees that the
topmost visible work item (including the selected item) is never
painted over by the sticky header, fixing the overlap bug where the
selected item's first wrapped line was clobbered.

The "will a sticky fire this frame?" decision is made against the
authoritative `list_scroll_offset` (after any pending recenter has
been applied to produce a tentative offset): if
`find_current_group_header(display_list, tentative_offset)` returns
`Some(h)` with `h < tentative_offset`, the first group header is off
the top of the viewport and the sticky slot is reserved. The slot
reservation shrinks `body_area` by one row; the recenter then runs
again against the smaller body so the centered-selection position
matches the final layout.

`predict_list_offset` is retained under `#[cfg(test)]` only - it is
no longer called from the render path but still documents what
ratatui's own `get_items_bounds` auto-scroll math does, which is
useful for sanity-checking the recenter math against the old
selection-driven auto-scroll behavior.

The sticky row uses a DarkGray background
(`style_sticky_header()` / `style_sticky_header_blocked()`) to visually
separate it from the highlighted item below, which uses a Cyan
background.

Behavior:
- Only active in flat list mode. Board drill-down never reserves the
  slot because the drill-down display list has no group headers.
- Shows the most recent `GroupHeader` that precedes the current scroll
  offset
- Disappears automatically when the original header scrolls back into
  view (i.e., the user scrolls up past it)
- Does not affect item selection. The scrollbar track follows the
  `body_area` height (not the full inner) so the thumb represents the
  list body, not the slot.
- Reserves a dedicated 1-row slot when a sticky is predicted to fire.
  In the rare edge case where selection jumps between frames and the
  post-render offset disagrees with the prediction, we accept a
  one-frame visual glitch (blank slot or briefly missing sticky) and
  the next frame reserves correctly. `debug_assert!` fires in debug
  builds so the mismatch is caught in development.

The header lookup is performed by `find_current_group_header()` in
`ui.rs`, which walks backwards from the scroll offset to find the
nearest `GroupHeader` entry.

### Layout: Flat List Mode

```
  List   Board                          Tab: switch view
+-- Work Items ------------+-- Claude Code | Terminal -------+
|                          |                                 |
| UNLINKED (N)             |  [PTY output or placeholder]    |
| ? a-very-long-pr-branch- |                                 |
|     name-that-wraps      |                                 |
|   repo-dir               |                                 |
|                          |                                 |
| [BL] idea-1              |                                 |
| [PL] plan-2              |                                 |
| [IM] feature-3           |                                 |
| [RV] fix-4               |                                 |
+--------------------------+---------------------------------+
| Context bar: title | [stage] | repo | labels               |
+------------------------------------------------------------+
| Status bar message                                         |
+------------------------------------------------------------+
```

The `List` label is highlighted (active). The header uses the ratatui
`Tabs` widget with `style_view_mode_tab_active()` for the selected tab.

Unlinked work items render as a wrapped branch title (with the `PR#N`
badge right-aligned on the first line) followed by an indented
repo-directory meta line. Long branches wrap across as many lines as
needed - never truncated - matching the wrap-not-truncate convention
used for board items.

Work items are shown as a flat list with stage badges:
- [BL] Backlog, [PL] Planning, [IM] Implementing
- [BK] Blocked, [RV] Review, [MQ] Mergequeue, [DN] Done

Each entry is rendered as a multi-line `ListItem` by
`format_work_item_entry` in `src/ui.rs`:

1. **Title line** - stage badge, wrapped title, optional right-side PR
   / CI / error badges.
2. **Title continuation lines** - only emitted when the title wraps.
3. **Display ID line** - optional. When the work item has a
   backend-provided `display_id` (e.g. `#workbridge-42`), it is
   rendered as a dimmed `#<slug>-<N>` subtitle between the title and
   the branch line, styled with the same `meta_style` as the branch
   line so selection highlighting flows consistently. Legacy records
   without a `display_id` skip this line entirely - row heights are
   variable, which the list rendering already supports for title and
   branch wrap. See `docs/work-items.md` "Display IDs" for the format
   and uniqueness rules.
4. **Branch subtitle line** - the branch name plus optional
   `[no wt]` marker when the worktree is missing. Also styled with
   `meta_style`.

While a work item is running its async review gate (PR existence -> CI
wait -> adversarial review, see docs/work-items.md "Review gate"), a
yellow+bold `[RG]` badge is inserted immediately to the right of the
stage badge (e.g. `[IM][RG] feature-3` or `[BK][RG] fix-5`). The badge
appears the instant the id enters `app.review_gates` and disappears the
instant it is removed via `drop_review_gate` (gate approved, rejected,
or retreated). It never appears on Done items because the gate cannot
run on a Done item. `[RG]` coexists with the `[RR]` review-request kind
badge as `[RR][IM][RG]`. The presence-only `[RG]` badge is what makes
"Claude actively coding" distinguishable from "gate running in the
background" in the list, because both share the same cyan braille
spinner in the left margin.

#### Rebase gate

Pressing `m` on a work item with a worktree spawns a **rebase gate**:
a background `git fetch origin <main>` followed by a headless harness
instance (cwd = the work item's worktree, MCP injected, see
`docs/harness-contract.md` "Known Spawn Sites") that runs
`git rebase origin/<main>` and resolves any conflicts in place.
Single-flight via `UserActionKey::RebaseOnMain` (500 ms debounce); per-
item state lives in `App.rebase_gates: HashMap<WorkItemId,
RebaseGateState>` per the structural-ownership rule, mirroring
`review_gates`. The right pane is taken over by a spinner + progress
text view while a rebase is in flight (the rebase-gate render block
lives in `src/ui.rs` immediately before the review-gate block and
takes precedence over it). On completion, `poll_rebase_gate` drops
the gate via `drop_rebase_gate` (which ends the status-bar activity,
clears the user-action guard slot when the slot is owned by the
work item being dropped, AND `libc::killpg`s the harness's process
group via the `child_pid` slot if it is still alive) and surfaces
a "Rebased onto origin/<main>" or "Rebase onto origin/<main>
failed: <reason>" status message. The harness is spawned with
`Command::process_group(0)` so it becomes its own group leader;
the `killpg` therefore takes down claude AND any `git rebase` /
`git add` subprocesses claude has started, not just claude itself.
`drop_rebase_gate` is also called from `delete_work_item_by_id`
and `force_kill_all`, so deleting a work item or quitting
workbridge while a rebase is in flight tears the gate down
cleanly: the cancellation flag covers the pre-spawn window
(default-branch resolution, `git fetch`, MCP server start, temp-
config write) and the process-group SIGKILL covers everything
from `Command::spawn` onwards, so the harness AND its in-flight
git subprocesses can be stopped at any phase before the background
`spawn_delete_cleanup` thread runs `git worktree remove`
underneath it. The "Rebased" success status
is gated on a local `git merge-base --is-ancestor origin/<main>
HEAD` check that the spawning thread runs against the worktree
before emitting `RebaseResult::Success`; if the check fails
(harness hallucinated, ran the wrong command, or emitted a stale
envelope) the gate downgrades to a Failure status naming the
ancestry mismatch. The UI never claims a rebase succeeded without
local git verification. The audit trail (so a later session
viewing this work item can see the rebase happened) is written by
the background thread via
`App.backend.append_activity_existing_only` (NOT the UI thread,
per the absolute blocking-I/O invariant), using a backend Arc
cloned at `spawn_rebase_gate` setup time. The `_existing_only`
variant - not `append_activity` - is the structural orphan-log
defense: it opens with `OpenOptions::create(false)`, so a racing
`backend.delete` that already archived the active log cannot be
silently reverted by a post-cancellation append. See
`docs/harness-contract.md` C10 / C11 for the full POSIX
explanation. Any append error is surfaced as a suffix on the
status message rather than silently dropped. The harness is explicitly told NOT to call
`workbridge_set_status` for this purpose because that would be a
no-op `Implementing -> Implementing` transition.
No `git push` is performed - after a successful rebase the user
sees `!pushed` (and likely `!pulled`) on the row and pushes
manually.

#### Worktree-state chips

Three chips report the local git state of a work item's worktree.
All three are pure cache reads off `RepoAssociation.git_state`
(populated by the background fetcher in `src/assembly.rs` /
`src/worktree_service.rs`) and therefore safe to render on the UI
thread:

- `!cl` - the worktree has uncommitted changes (`git_state.dirty`).
  Yellow. The chip is exclusively for "the working copy is dirty";
  ahead/behind state has its own chips below. A clean-but-diverged
  branch no longer shows `!cl`.
- `!pushed` - the local branch has commits its upstream does not
  (`git_state.ahead > 0`). Magenta. "Action available: push."
- `!pulled` - the upstream has commits the local branch does not
  (`git_state.behind > 0`). Magenta. "Action available: pull (or
  rebase via `m`)."

The three chips coexist on the same line: a dirty, diverged branch
renders `!cl !pushed !pulled` with `!cl` in LightYellow and both
divergence chips in Magenta. The chip labels (not the colors)
distinguish push-direction from pull-direction, while `!cl` stays
visually distinct from both. Both divergence chips are rendered
alongside `!cl` in `format_work_item_entry` (`src/ui.rs`).

Stage transitions: Shift+Right to advance, Shift+Left to retreat.

Left panel: 25% of width (min 30 columns)
Right panel: remainder minus 2 for borders
Status bar: 1 row, conditional on status_message.is_some()

### Layout: Board View

```
  List   Board          Tab: switch view | <-/->: columns | ...
+- Backlog ----+- Planning ---+- Implementing +- Review -----+
|              |              |               |              |
| idea-1       | plan-2       | [BK] fix-5    | fix-4        |
| idea-6       |              | feature-3     |              |
|              |              |               |              |
+--------------+--------------+---------------+--------------+
| Context bar: title | [stage] | repo | labels               |
+------------------------------------------------------------+
| Status bar message                                         |
+------------------------------------------------------------+
```

The `Board` label is highlighted (active). Board-mode hints show
arrow key navigation and Shift+arrow stage transitions.

The `repo` segment in the context bar is the short repo name (last
path segment of the work item's repo root), not the full path - the
full path is available via the right panel Terminal tab's shell
prompt.

Four columns displayed: Backlog, Planning, Implementing, Review.
Done items are hidden from the board. Blocked items appear in the
Implementing column with a [BK] prefix. Mergequeue items appear in the
Review column with a [MQ] prefix. PR badges and CI status are shown on
board items. Long titles wrap (not truncate).

The `BoardLayout` struct (in `src/layout.rs`) and `compute_board()`
function calculate 4 equal-width columns from the terminal width.
The focused column's border uses `style_board_column_focused()`;
other columns use `style_board_column_unfocused()`.

Drill-down (Enter on a board item) switches to a filtered two-panel
layout showing only items from the selected column's stage, with the
PTY panel on the right. Ctrl+] returns to the full board view.

### Layout: Dashboard View

```
  List   Board   Dashboard    Tab: switch view | 1/2/3/4: 7d/30d/90d/365d window
+-- Dashboard (window: 30d) --------------------------------------------+
| Throughput 11/30d   Cycle p50 5d   Cycle p90 20d   Backlog now 5 (+5)  |
+-- Done vs PRs merged  [G] done [M] merged (daily) -+-- Created per day +
|                                                    |                  |
|              GG    GG                              |      BB BB BB    |
|              GG    GG MM      MM                   |      BB BB BB    |
|              GG GG GG MM  GG  MM GG                |   BB BB BB BB    |
+- -29d ---- -19d --- -9d ------- now ---------------+- -29d ...  now --+
+-- Backlog size over time  now 5 / peak 5 ----------+-- Stuck items ---+
|                                                    |  Review  5d0h    |
|                                  ▁▁▁▁▁██████████   |  Review  3d0h    |
|                              ▃▃▃▃██████████████    |  Blocked 2d0h    |
|                          ▆▆▆▆████████████████      |                  |
+- -29d ---- -19d --- -9d ------- now ---------------+------------------+
```

The `Dashboard` label is highlighted (active). The Dashboard is a
2x2 grid of chart panels (`Done vs PRs merged`, `Created per day`,
`Backlog size over time`, `Stuck items`) sitting below a one-row KPI
strip. The KPI strip shows throughput, cycle-time p50/p90, current
backlog + delta from window start, and stuck-item count. Every
chart panel has overlaid x-axis labels on its bottom border at 0%,
33%, 66%, and 100% of the chart width (`-Nd` / `-Md` / `-Kd` /
`now`), written by `draw_bottom_axis_labels` directly into the
buffer row after the block is rendered.

The Done-vs-PRs-merged chart is a grouped `BarChart` with two bars
per bucket (green = Done, magenta = merged). Long windows bucket
aggressively (daily for 7d/30d, weekly for 90d, monthly for 365d)
so the bar density stays readable. The other two charts use
`ratatui_widgets::sparkline::Sparkline` for 1/8-cell filled-area
rendering. Long windows (90d/365d) are downsampled with
`downsample_for_sparkline` so the inner width of the chart panel
always receives exactly one data point per column (otherwise
`Sparkline` would truncate the tail). The Stuck items panel is a
plain `Paragraph` list.

See `docs/metrics.md` for the data pipeline, aggregation rules, and
per-chart semantics.

### Right Panel Tabs

The right panel has two tabs: **Claude Code** and **Terminal**. The
`RightPanelTab` enum tracks which tab is active. The tab bar is shown
in the right panel's block title when the selected work item has a
worktree; otherwise only "Claude Code" is shown.

- **Claude Code**: the per-work-item Claude Code PTY session (existing
  behavior). Shows session output, dead-session prompts, work item
  details, or error lists depending on session state.
- **Terminal**: a shell session (`$SHELL`, falling back to `/bin/sh`)
  with cwd set to the work item's worktree path. Spawned lazily on
  first tab switch. One terminal session per work item, stored in
  `App::terminal_sessions` keyed by `WorkItemId`.

Tab switching:
- Ctrl+\\: cycle between Claude Code and Terminal. Global intercept
  in `handle_key()` so it works from both panels and does not change
  focus. The `ClaudeCode -> Terminal` transition is a no-op if the
  selected work item has no worktree. Still fires even when the
  current tab's session has ended - the on-screen "Press Ctrl+\\ to
  switch back to Claude Code" hint (shown on the dead-terminal
  placeholder in `src/ui.rs`) and the symmetric dead-Claude case both
  rely on this. Because the intercept runs before the right-panel
  dead-session early-return, a dead session never blocks the flip.
  On the Claude-Code-dead -> Terminal flip, the terminal session is
  spawned lazily via `spawn_terminal_session()` if the work item has
  a worktree.
- Tab (while right panel is focused): forwarded to the PTY as `\t`
  (0x09) so Claude Code's autocomplete fires. Not intercepted by
  workbridge. On a dead right-panel session Tab takes the standard
  escape-hatch path (see below).
- Shift+Tab (while right panel is focused, live session): forwarded
  to PTY as CSI Z (not intercepted).
- All other keys on a dead right-panel session redirect focus to the
  left panel with a "returned to work items" status message (the
  existing escape hatch). Ctrl+], Tab, Shift+Tab / BackTab, plain
  letters, Enter, Esc all take this path. Only the global `Ctrl+\\`
  intercept bypasses it.

Terminal sessions are cleaned up on:
- Work item deletion (killed in `delete_work_item_by_id`)
- Orphan detection (work item removed from `work_items` list)
- App shutdown (SIGTERM then SIGKILL like Claude sessions)

Navigating to a different work item (Up/Down in left panel) resets the
tab to Claude Code.

### Overlays

All overlays must call `dim_background(buf, area)` before rendering
their own content. This applies `Modifier::DIM` and forces foreground
to `Color::DarkGray` across every cell, ensuring the overlay is the
clear focal point regardless of existing colors or borders.

Overlay rendering pattern:
1. `dim_background(buf, area)` - dim the entire screen
2. `Clear` widget to blank the popup area (restores default cell style)
3. `Block` with border, title, and appropriate `BorderType`
4. Content widgets inside the block's inner area
5. 1-cell padding inside the border

### Overlay visual conventions

Two distinct visual identities separate overlay types:

| Overlay type | Border type | Border color | Use for |
|---|---|---|---|
| **Prompt dialog** | `Rounded` | Cyan | Blocking choice/input prompts |
| **Alert dialog** | `Rounded` | Red | Error messages (dismissed with Enter/Esc) |
| **Full dialog** | `Plain` | Cyan | Complex forms (create, settings) |
| **Drawer** | `Plain` | Cyan | Global assistant panel |

`Rounded` borders are the visual signal for "attention required." Cyan
`Rounded` = choice prompt; Red `Rounded` = error/alert. Never use `Rounded`
for non-blocking overlays.

For error messages that require acknowledgment, set `app.alert_message =
Some(msg)` instead of `status_message`. This surfaces a red-bordered alert
dialog that blocks interaction until dismissed with Enter or Esc.

### Overlay z-order (back to front)

1. Main UI (panels, context bar, status bar)
2. Settings overlay
3. Prompt dialogs (merge, rework, no-plan, cleanup, branch-gone, stale-worktree, delete)
4. Alert dialog (renders above all other prompts)
5. Global assistant drawer
6. Create dialog

### Global assistant drawer session lifetime

The global assistant drawer (toggled with Ctrl+G) does NOT keep its
`claude` session alive across drawer openings. Every open spawns a
fresh session with an empty context and scrollback; every close
(Ctrl+G or Esc while the drawer is open) immediately tears the
session down via `App::teardown_global_session`. Teardown:

1. SIGTERMs the `claude` child (graceful grace period + SIGKILL
   via `Session::kill`).
2. Drops the `SessionEntry` so `Session::Drop` joins the reader
   thread.
3. Drops `global_mcp_server`.
4. Deletes the temp MCP config file and clears
   `global_mcp_config_path`.
5. Drains `pending_global_pty_bytes` so buffered keystrokes from
   the previous session never leak into the next one.

The rule is "every Ctrl+G opening sees a blank-slate PTY," so any
new state added to the global assistant must also be cleared in
`teardown_global_session`.

The session cwd is a dedicated workbridge-owned scratch directory
(`$TMPDIR/workbridge-global-assistant-cwd`, created idempotently
on each spawn). This is deliberately NOT `$HOME`: Claude Code's
workspace trust dialog ("Do you trust the files in this folder?")
persists acceptance per-project in `~/.claude.json`, but the home
directory does not reliably persist that acceptance, so using
`$HOME` as the cwd would produce the trust prompt on every single
Ctrl+G. A stable non-home scratch path lets Claude Code's own
trust-persistence mechanism cover it after the first acceptance,
without workbridge ever reading or writing `~/.claude.json`
itself (which would be a file-injection workaround - see the
"Severity overrides" section in `CLAUDE.md`).

### Settings overlay tabs

The settings overlay (opened with `?`) has three tabs, cycled with Tab:

1. **Repos** - manage which repositories are tracked (Left/Right to switch
   columns, Enter to move a repo between managed/available).
2. **Review Gate** - edit the review skill (slash command) used by the review
   gate. Enter starts inline editing, Enter saves to `config.toml`, Esc cancels.
   The value is stored in `defaults.review_skill`.
3. **Keybindings** - scrollable reference of all keyboard shortcuts.

When the Review Gate tab is in editing mode, Tab and `?`/Esc are captured by
the text input. Esc cancels editing first; a second Esc (or `?`) closes the
overlay.

### What NOT to do

- Do not set `app.status_message` to show prompt content. Prompt dialogs
  render their own content; the status bar is for transient notifications.
- Do not set `app.status_message` for error messages that need acknowledgment.
  Use `app.alert_message` instead so a red alert dialog appears.
- Same-key-repeat confirmations (quit) stay in the status bar -
  they are not blocking choice prompts and do not need dialog boxes.
  Destructive operations on real git state (e.g., work item deletion)
  use a modal prompt dialog instead, both to block the Claude session
  from receiving stray keystrokes during cleanup and because the
  operation should not feel like a "tap twice" shortcut.
- Do not skip `dim_background()` in new overlays.

## Theme

The `Theme` struct in `src/theme.rs` centralizes all colors. It uses
ANSI-safe colors (Reset for text against terminal background, absolute
colors only when the Theme controls both foreground and background).

To style a rat-widget component, pass Theme styles directly:
```rust
TextInput::new().style(theme.style_text())
```

View mode header styles:
- `style_view_mode_tab()` - inactive tab label (dimmed)
- `style_view_mode_tab_active()` - active tab label (bold, inverted highlight)
- `style_view_mode_hints()` - keybinding hints text (dimmed)

Session activity indicator styles:
- `style_badge_session_idle()` - filled circle for idle session (Gray)
- `style_badge_session_working()` - animated braille spinner for active work (Cyan, Bold)

Board-specific styles:
- `style_board_column_focused()` - border for the active column
- `style_board_column_unfocused()` - border for inactive columns
- `style_board_column_header()` - column header text
- `style_board_item_highlight()` - selected item highlight bar

## Session Activity Indicators

Work items display a visual indicator when a Claude session exists,
distinguishing three states:

1. **No session**: no indicator (default).
2. **Session exists, idle**: a filled circle in Gray. This means a Claude
   session is alive but has not signaled active work.
3. **Actively working**: an animated braille spinner in Cyan+Bold. Claude
   has called `workbridge_set_activity(working=true)` via MCP.

In the flat list view, the indicator appears in the left margin
(replacing the selection caret ">"). In the board view, it appears on
the second line alongside PR and CI indicators.

The activity state is signaled by Claude via the `workbridge_set_activity`
MCP tool and is cleared automatically when the session process exits
(detected during liveness checks).

The spinner reuses the same braille-dot frames and 200ms tick rate as the
status bar activity indicator. The `spinner_tick` counter advances when
either status-bar activities or `claude_working` entries exist.

## Testing

### Snapshot Tests

Create a Buffer, call the render function directly, convert to string:
```rust
fn render_test(state: &mut App, width: u16, height: u16) -> String {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
    let theme = Theme::default_theme();
    draw_to_buffer(Rect::new(0, 0, width, height), &mut buf, state, &theme);
    // Convert buf cells to string...
}
```

### Event Tests

Event handlers are regular functions - call them directly:
```rust
handle_key(&mut app, key_event);
assert!(app.create_dialog.visible);
```

### Unit Tests

App methods (assembly, display list, session management) are tested
independently of the event loop. Tests use `InMemoryConfigProvider` and
`StubBackend` - never real config files or GitHub API calls.

See `docs/TESTING.md` for testing rules.

## Widget Inventory

From rat-widget (available for future dialogs):
- TextInput, TextArea, NumberInput, DateInput
- Checkbox, Radio, Choice/Select, ComboBox
- List, Table (rat-ftable)
- Dialog frame, Message dialog, File dialog
- Menu, Menubar
- Slider, Calendar
- Form layout helpers

Currently used:
- TextInput, TextArea (rat-widget) - Create Work Item dialog fields. The
  Description TextArea uses `TextWrap::Word(2)` so long text wraps at word
  boundaries, and `Scroll::new()` on the vertical axis so content that
  exceeds `DESC_TEXTAREA_HEIGHT` scrolls with a visible scrollbar.
- List (ratatui-widgets) - work item list, repo selection, board columns
  (rendered via StatefulWidget::render in board view)
- Tabs (ratatui-widgets) - view mode header (List/Board segmented tab bar)
- Block, Paragraph, Clear (ratatui-widgets) - layout and overlays
- PseudoTerminal (tui-term) - PTY output rendering
