# UI Architecture

WorkBridge uses [rat-salsa](https://github.com/thscharler/rat-salsa) as
the event loop framework with custom widgets for the main panels and
[rat-widget](https://github.com/thscharler/rat-salsa) components for
input dialogs.

## Event Loop

The application runs via `rat_salsa::run_tui()` which manages terminal
setup/teardown, event polling, and the render cycle. Four callbacks drive
the application:

- **init**: starts the background fetcher, installs a 200ms tick timer,
  sets initial pane dimensions
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
2. **Right panel** - the per-work-item PTY session area.

If a target is found and the underlying session is alive, the scroll
event is encoded according to the child process's mouse protocol mode
and encoding (queried from the vt100 parser). The resulting bytes are
written to the PTY master fd. When the child has not enabled mouse
reporting, scrolls are converted to arrow-key sequences (Up/Down).

### Timer-Driven Periodic Work

A 200ms repeating timer drives:
1. Session liveness checks (`check_liveness`)
2. Fetch result drain (`drain_fetch_results`)
3. Pending fetch error drain
4. Signal handling (SIGTERM/SIGINT via AtomicBool)
5. Shutdown deadline enforcement (10s)
6. Fetcher restart when managed repos change

## View Modes

The `ViewMode` enum controls the root overview layout:

- `FlatList` (default): two-panel layout with work item list (left) and
  PTY session (right). See Layout section below.
- `Board`: kanban board with 4 columns organized by workflow stage.
  See Board View section below.

Toggle between modes with Tab. The selected work item is preserved
across toggles. A 1-row view mode header at the top of the screen
shows a segmented tab bar (using the ratatui `Tabs` widget) with
`List` and `Board` labels. The active mode is highlighted. Contextual
keybinding hints appear right-aligned in the header (e.g., board mode
shows arrow key and Shift+arrow controls).

## Focus Model

### Top-Level: Left/Right Panel (Flat List Mode)

The `FocusPanel` enum tracks whether the left panel (work item list) or
right panel (PTY session) has focus. This is NOT managed by rat-focus
because the right panel forwards almost all keys to the PTY, which is
incompatible with rat-focus's widget navigation model.

- Enter on a work item: focus right panel
- Ctrl+]: return to left panel
- Dead session: auto-return to left panel

### Board Mode Navigation

In board view, `handle_key_board()` intercepts key events before the
left/right panel handlers. The `BoardCursor` struct tracks column index
and row index independently.

- Left/Right arrows: move between columns
- Up/Down arrows: move between items within a column
- Shift+Right: advance item to next stage
- Shift+Left: retreat item to previous stage
- Enter: drill down into selected column (filtered flat list + PTY)
- Ctrl+]: return from drill-down to board view

After a stage transition (Shift+arrow), the cursor follows the item
into its new column.

### Within Dialogs

Dialogs (creation modal, settings overlay) have their own focus
management. The creation dialog cycles through fields with Tab/Shift+Tab.
When a dialog is visible, it intercepts all key events before the
left/right panel handlers.

## Adding a New Dialog

1. Create a dialog struct in `src/<dialog_name>.rs` with:
   - `visible: bool`
   - Input state fields (SimpleTextInput for text, Vec for lists)
   - Focus tracking enum
   - `open()`, `close()`, `handle_key()` methods

2. Add the dialog field to `App` in `src/app.rs`

3. In `src/event.rs`, add an intercept block at the top of `handle_key`:
   ```rust
   if app.<dialog>.visible {
       handle_<dialog>_key(app, key);
       return;
   }
   ```

4. In `src/ui.rs`, add rendering after the main layout:
   ```rust
   if state.<dialog>.visible {
       draw_<dialog>(area, buf, state, theme);
   }
   ```

5. Wire the trigger key in `handle_key_left`

See `src/create_dialog.rs` as the reference implementation.

## Rendering

All rendering is Buffer-based (not Frame-based). Widgets use the
`Widget::render(self, area, buf)` and
`StatefulWidget::render(widget, area, buf, &mut state)` patterns from
ratatui-core.

### Layout: Flat List Mode

```
  List   Board                          Tab: switch view
+-- Work Items --+-- Claude Code -----------------+
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
- [BK] Blocked, [RV] Review, [DN] Done

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
Implementing column with a [BK] prefix. PR badges and CI status
are shown on board items. Long titles wrap (not truncate).

The `BoardLayout` struct (in `src/layout.rs`) and `compute_board()`
function calculate 4 equal-width columns from the terminal width.
The focused column's border uses `style_board_column_focused()`;
other columns use `style_board_column_unfocused()`.

Drill-down (Enter on a board item) switches to a filtered two-panel
layout showing only items from the selected column's stage, with the
PTY panel on the right. Ctrl+] returns to the full board view.

### Overlays

Overlays (settings, creation dialog) render on top using:
1. `Clear` widget to blank the popup area
2. `Block` with border and title
3. Content widgets inside the block's inner area
4. 1-cell padding inside the border

When an overlay is visible, background panel borders use unfocused style
to create visual hierarchy.

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

Board-specific styles:
- `style_board_column_focused()` - border for the active column
- `style_board_column_unfocused()` - border for inactive columns
- `style_board_column_header()` - column header text
- `style_board_item_highlight()` - selected item highlight bar

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
