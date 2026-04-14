# UI Architecture

WorkBridge uses [rat-salsa](https://github.com/thscharler/rat-salsa) as
the event loop framework with custom widgets for the main panels and
[rat-widget](https://github.com/thscharler/rat-salsa) components for
input dialogs.

## Event Loop

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

### Mouse Events

Mouse capture is enabled so the terminal forwards mouse events to the
application. Currently only ScrollUp and ScrollDown are handled; all
other mouse events are ignored.

When a scroll event arrives, `mouse_target()` performs hit-testing
against terminal-absolute coordinates to determine which PTY area (if
any) the cursor is over:

1. **Global drawer** - checked first because it overlays everything.
   When the drawer is open, coordinates outside its inner area return
   `MouseTarget::None` so the dimmed background does not receive events.
2. **Right panel** - the per-work-item PTY session area (Claude Code
   or Terminal tab, depending on `right_panel_tab`).

Scroll events drive a local scrollback viewport rather than being
forwarded directly to the child process:

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

### Mouse Text Selection

When mouse capture is enabled, users can select text in PTY areas
(right panel, global drawer) by clicking and dragging with the left
mouse button. Selection works like a standard terminal emulator:

- Click-and-drag highlights the selected text (inverted colors).
- On mouse release, the selected text is automatically copied to the
  system clipboard via the arboard crate.
- Clicking without dragging clears any existing selection.
- Any keypress clears the selection.
- Selection works in both live view and scrollback mode.

Mouse selection is only intercepted when the child process has NOT
enabled mouse reporting (MouseProtocolMode::None). When the child
has enabled mouse reporting (e.g., vim, htop), mouse events are
forwarded to the PTY as before. Exception: in local scrollback mode,
selection is always available since the PTY is not receiving events.

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
  `self.repo_data[repo_path].worktrees` (which now carries
  `has_commits_ahead` so `branch_has_commits` is a pure cache lookup).
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
  directly. `begin_session_open` spawns a background thread that runs
  `backend.read_plan(...)` and sends a `SessionOpenPlanResult` through
  a per-work-item receiver; `poll_session_opens` drains completed
  receivers and invokes `finish_session_open`, which builds the claude
  command and spawns the PTY on the UI thread. `stage_system_prompt`
  now takes the pre-read plan text as a parameter and MUST NOT call
  `backend.read_plan(...)` itself. The receiver map is keyed by
  `WorkItemId` so parallel session opens for different items cannot
  collide.

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

## Focus Model

### Top-Level: Left/Right Panel (Flat List Mode)

The `FocusPanel` enum tracks whether the left panel (work item list) or
right panel (PTY session) has focus. This is NOT managed by rat-focus
because the right panel forwards almost all keys to the PTY, which is
incompatible with rat-focus's widget navigation model.

- Enter on a work item: focus right panel
- Tab (when right panel focused): cycle between Claude Code and Terminal tabs
- Ctrl+]: return to left panel
- Ctrl+D / Delete: delete selected work item (modal confirmation)
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
   - Input state fields (SimpleTextInput for text, Vec for lists)
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
pub my_prompt_input: SimpleTextInput,    // if text input needed
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

The modal reuses `PromptDialogKind::TextInput` and `SimpleTextInput`
(see `src/ui.rs` and `src/create_dialog.rs`). State lives on
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
     `SessionOpenPending.activity`, `MergequeuePollState.activity`).
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
  only the visible spinner is suppressed.
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

### Scroll Offset Persistence

The left-panel work item list persists its scroll offset between render
frames via `App::list_scroll_offset`, a `Cell<usize>`. This field uses
interior mutability (`Cell`) because rendering takes `&App` (immutable),
but the offset must be written back after each render so the viewport
stays stable during keyboard navigation.

The render flow in `draw_work_item_list` (ui.rs):

1. Create a `ListState` seeded with the persisted offset:
   `ListState::default().with_offset(app.list_scroll_offset.get())`
2. Set the selected item: `state.select(app.selected_item)`
3. Render via `StatefulWidget::render` - ratatui's `get_items_bounds()`
   checks whether the selected item falls within the visible range and
   only adjusts the offset when it does not
4. Write the (possibly adjusted) offset back:
   `app.list_scroll_offset.set(state.offset())`

This means the highlight moves freely within the visible viewport. The
viewport only scrolls when the highlight reaches a border (top or
bottom edge).

The offset is reset to 0 in `build_display_list()` whenever the display
list is rebuilt (view mode toggle, drill-down, item deletion, fetch
cycle). This prevents stale offsets from a previous list shape carrying
over into a structurally different list. ratatui re-clamps the offset
on the next render frame based on the selected item position.

### Sticky Group Headers

When the left-panel work item list is scrolled down and a group header
(e.g., "ACTIVE (repo)", "BACKLOGGED (repo)") has scrolled above the
viewport, a "sticky" copy of that header is rendered at the top of the
list's inner area. This ensures the user always knows which group the
currently visible items belong to.

The sticky header is rendered as a `Paragraph` widget overlay after the
`List` widget has already rendered, overwriting the first row of the
inner area. It uses a DarkGray background (`style_sticky_header()` /
`style_sticky_header_blocked()`) to visually separate it from the
highlighted item below, which uses a Cyan background.

Behavior:
- Only active in flat list mode (not board drill-down, which has no
  group headers)
- Shows the most recent `GroupHeader` that precedes the current scroll
  offset
- Disappears automatically when the original header scrolls back into
  view (i.e., the user scrolls up past it)
- Does not affect scrollbar position or item selection
- Overlays the first visible row - does not insert an extra row

The header lookup is performed by `find_current_group_header()` in
`ui.rs`, which walks backwards from the scroll offset to find the
nearest `GroupHeader` entry.

### Layout: Flat List Mode

```
  List   Board                          Tab: switch view
+-- Work Items --+-- Claude Code | Terminal -------+
|                |                                 |
| UNLINKED (N)   |  [PTY output or placeholder]    |
| ? pr-branch    |                                 |
|                |                                 |
| [BL] idea-1    |                                 |
| [PL] plan-2    |                                 |
| [IM] feature-3 |                                 |
| [RV] fix-4     |                                 |
+----------------+---------------------------------+
| Context bar: title | repo | labels               |
+--------------------------------------------------+
| Status bar message                               |
+--------------------------------------------------+
```

The `List` label is highlighted (active). The header uses the ratatui
`Tabs` widget with `style_view_mode_tab_active()` for the selected tab.

Work items are shown as a flat list with stage badges:
- [BL] Backlog, [PL] Planning, [IM] Implementing
- [BK] Blocked, [RV] Review, [MQ] Mergequeue, [DN] Done

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
| Context bar: title | repo | labels                         |
+------------------------------------------------------------+
| Status bar message                                         |
+------------------------------------------------------------+
```

The `Board` label is highlighted (active). Board-mode hints show
arrow key navigation and Shift+arrow stage transitions.

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

Tab switching (while right panel is focused):
- Tab: cycle between Claude Code and Terminal
- Shift+Tab: forwarded to PTY as CSI Z (not intercepted)

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
3. Prompt dialogs (merge, rework, no-plan, cleanup, branch-gone, delete)
4. Alert dialog (renders above all other prompts)
5. Global assistant drawer
6. Create dialog

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
- SimpleTextInput (custom, in create_dialog.rs) - lightweight text input
- List (ratatui-widgets) - work item list, repo selection, board columns
  (rendered via StatefulWidget::render in board view)
- Tabs (ratatui-widgets) - view mode header (List/Board segmented tab bar)
- Block, Paragraph, Clear (ratatui-widgets) - layout and overlays
- PseudoTerminal (tui-term) - PTY output rendering
