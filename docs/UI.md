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
more than a few milliseconds must run on a background thread.

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

Toggle between modes with Tab. The selected work item is preserved
across toggles. A 1-row view mode header at the top of the screen
shows a segmented tab bar (using the ratatui `Tabs` widget) with
`List` and `Board` labels. The active mode is highlighted. Contextual
keybinding hints appear right-aligned in the header (e.g., board mode
shows arrow key and Shift+arrow controls).

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

### Activity indicator placement

Activity indicators follow a strict ownership rule based on who
initiated the action:

**User-initiated actions from a dialog** must show progress inline in
that dialog. The dialog stays open with a spinner until the operation
completes. The user triggered the action and is watching the dialog -
progress and errors must appear where they are looking. Never close the
dialog and move feedback to the status bar; that disconnects the result
from the action.

**System-initiated actions** (triggered by Claude, periodic background
fetches, or automatic transitions) belong in the status bar. The user
did not explicitly trigger these, so a non-blocking global indicator is
appropriate.

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
  `cleanup_in_progress` guard in event.rs
- Merge dialog: `execute_merge()`, `poll_pr_merge()`,
  `merge_in_progress` guard in event.rs

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
