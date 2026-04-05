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

### Timer-Driven Periodic Work

A 200ms repeating timer drives:
1. Session liveness checks (`check_liveness`)
2. Fetch result drain (`drain_fetch_results`)
3. Pending fetch error drain
4. Signal handling (SIGTERM/SIGINT via AtomicBool)
5. Shutdown deadline enforcement (10s)
6. Fetcher restart when managed repos change

## Focus Model

### Top-Level: Left/Right Panel

The `FocusPanel` enum tracks whether the left panel (work item list) or
right panel (PTY session) has focus. This is NOT managed by rat-focus
because the right panel forwards almost all keys to the PTY, which is
incompatible with rat-focus's widget navigation model.

- Enter on a work item: focus right panel
- Ctrl+]: return to left panel
- Dead session: auto-return to left panel

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

### Layout

```
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

Work items are shown as a flat list with stage badges:
- [BL] Backlog, [PL] Planning, [IM] Implementing
- [BK] Blocked, [RV] Review, [DN] Done

Stage transitions: Shift+Right to advance, Shift+Left to retreat.

Left panel: 25% of width (min 30 columns)
Right panel: remainder minus 2 for borders
Status bar: 1 row, conditional on status_message.is_some()

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
- List (ratatui-widgets) - work item list, repo selection
- Block, Paragraph, Clear (ratatui-widgets) - layout and overlays
- PseudoTerminal (tui-term) - PTY output rendering
