use crate::app::{App, RightPanelTab};
use crate::work_item::SelectionState;

/// Encode a scroll event as bytes to send to a PTY session.
///
/// When the child has not enabled mouse reporting (mode is `None`), the scroll
/// is converted to arrow key sequences (Up/Down). When the child has enabled
/// mouse reporting, the event is encoded according to the child's chosen
/// encoding (SGR or Default/Utf8).
///
/// Returns `None` if the event cannot be encoded (e.g., Default encoding with
/// coordinates exceeding 222).
pub fn encode_mouse_scroll(
    scroll_up: bool,
    local_col: u16,
    local_row: u16,
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    if mode == vt100::MouseProtocolMode::None {
        // Child has not enabled mouse reporting - convert to arrow keys.
        // Send 3 lines per scroll tick for usable scroll speed.
        let arrow = if scroll_up { b"\x1b[A" } else { b"\x1b[B" };
        let mut data = Vec::with_capacity(arrow.len() * 3);
        for _ in 0..3 {
            data.extend_from_slice(arrow);
        }
        return Some(data);
    }

    // Button codes: 64 = scroll up, 65 = scroll down.
    let button: u8 = if scroll_up { 64 } else { 65 };

    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            // SGR encoding: ESC [ < button ; col+1 ; row+1 M
            let seq = format!("\x1b[<{};{};{}M", button, local_col + 1, local_row + 1);
            Some(seq.into_bytes())
        }
        vt100::MouseProtocolEncoding::Default | vt100::MouseProtocolEncoding::Utf8 => {
            // X10/Default encoding: ESC [ M <button+32> <col+1+32> <row+1+32>
            // Coordinates > 222 cannot be encoded (would exceed printable byte range).
            let cx = local_col + 1 + 32;
            let cy = local_row + 1 + 32;
            if cx > 255 || cy > 255 {
                return None;
            }
            Some(vec![0x1b, b'[', b'M', button + 32, cx as u8, cy as u8])
        }
    }
}

/// Check whether the child process in the global drawer has enabled mouse
/// reporting and we are NOT in local scrollback mode. When the child wants
/// mouse events, we should forward them rather than intercepting for selection.
pub fn child_wants_mouse_global(app: &App) -> bool {
    // In scrollback mode, always intercept for selection.
    if app
        .global_drawer
        .session
        .as_ref()
        .is_some_and(|e| e.scrollback_offset > 0)
    {
        return false;
    }
    app.global_drawer
        .session
        .as_ref()
        .filter(|s| s.alive)
        .and_then(|s| s.parser.lock().ok())
        .is_some_and(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
}

/// Check whether the child process in the right panel has enabled mouse
/// reporting and we are NOT in local scrollback mode.
pub fn child_wants_mouse_right(app: &App) -> bool {
    // In scrollback mode, always intercept for selection.
    let in_scrollback = match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app
            .active_session_entry()
            .is_some_and(|e| e.scrollback_offset > 0),
        RightPanelTab::Terminal => app
            .active_terminal_entry()
            .is_some_and(|e| e.scrollback_offset > 0),
    };
    if in_scrollback {
        return false;
    }
    let entry_ref = match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app.active_session_entry(),
        RightPanelTab::Terminal => app.active_terminal_entry(),
    };
    entry_ref
        .filter(|s| s.alive)
        .and_then(|s| s.parser.lock().ok())
        .is_some_and(|p| p.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None)
}

/// Get a mutable reference to the active session entry based on the current
/// right panel tab.
pub fn active_session_entry_mut_for_tab(
    app: &mut App,
) -> Option<&mut crate::work_item::SessionEntry> {
    match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app.active_session_entry_mut(),
        RightPanelTab::Terminal => app.active_terminal_entry_mut(),
    }
}

/// Handle scroll events for the global drawer.
pub fn handle_scroll_global(
    app: &mut App,
    scroll_up: bool,
    local_col: u16,
    local_row: u16,
) -> bool {
    // Scroll-up always enters/advances local scrollback (never forwarded to PTY).
    // Clamp to the terminal row count because vt100's visible_rows()
    // panics if scrollback_offset > rows (usize underflow).
    if scroll_up {
        if let Some(entry) = app.global_drawer.session.as_mut() {
            let max = entry
                .parser
                .lock()
                .ok()
                .map_or(0, |p| p.screen().size().0 as usize);
            entry.scrollback_offset = (entry.scrollback_offset + 3).min(max);
        }
        return true;
    }
    // Scroll-down while in scrollback: decrement offset locally.
    if let Some(entry) = app.global_drawer.session.as_mut()
        && entry.scrollback_offset > 0
    {
        entry.scrollback_offset = entry.scrollback_offset.saturating_sub(3);
        return true;
    }
    // Scroll-down while NOT in scrollback: forward to PTY as before.
    let proto = app
        .global_drawer
        .session
        .as_ref()
        .filter(|s| s.alive)
        .and_then(|s| {
            let parser = s.parser.lock().ok()?;
            let screen = parser.screen();
            Some((
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            ))
        });
    if let Some((mode, encoding)) = proto
        && let Some(data) = encode_mouse_scroll(scroll_up, local_col, local_row, mode, encoding)
    {
        app.send_bytes_to_global(&data);
        return true;
    }
    false
}

/// Handle scroll events for the right panel.
pub fn handle_scroll_right(app: &mut App, scroll_up: bool, local_col: u16, local_row: u16) -> bool {
    // Scroll-up always enters/advances local scrollback (never forwarded to PTY).
    // Clamp to the terminal row count because vt100's visible_rows()
    // panics if scrollback_offset > rows (usize underflow).
    if scroll_up {
        if let Some(entry) = app.active_session_entry_mut() {
            let max = entry
                .parser
                .lock()
                .ok()
                .map_or(0, |p| p.screen().size().0 as usize);
            entry.scrollback_offset = (entry.scrollback_offset + 3).min(max);
        }
        return true;
    }
    // Scroll-down while in scrollback: decrement offset locally.
    if app
        .active_session_entry()
        .is_some_and(|s| s.scrollback_offset > 0)
    {
        if let Some(entry) = app.active_session_entry_mut() {
            entry.scrollback_offset = entry.scrollback_offset.saturating_sub(3);
        }
        return true;
    }
    // Scroll-down while NOT in scrollback: forward to PTY as before.
    // Extract mouse protocol info from the correct session based on
    // which tab is active. Skip if the session is not alive.
    let entry_ref = match app.right_panel_tab {
        RightPanelTab::ClaudeCode => app.active_session_entry(),
        RightPanelTab::Terminal => app.active_terminal_entry(),
    };
    let proto = entry_ref.filter(|s| s.alive).and_then(|s| {
        let parser = s.parser.lock().ok()?;
        let screen = parser.screen();
        Some((
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
        ))
    });
    if let Some((mode, encoding)) = proto
        && let Some(data) = encode_mouse_scroll(scroll_up, local_col, local_row, mode, encoding)
    {
        match app.right_panel_tab {
            RightPanelTab::ClaudeCode => app.send_bytes_to_active(&data),
            RightPanelTab::Terminal => app.send_bytes_to_terminal(&data),
        }
        return true;
    }
    false
}

/// Finalize selection on mouse-up for the global drawer session.
pub fn handle_selection_up_global(app: &mut App, local_row: u16, local_col: u16) -> bool {
    let Some(entry) = app.global_drawer.session.as_mut() else {
        return false;
    };
    let sel = match entry.selection.as_mut() {
        Some(s) if s.dragging => s,
        _ => return false,
    };
    sel.current = (local_row, local_col);
    sel.dragging = false;
    // If anchor == current (click with no drag), clear selection.
    if sel.anchor == sel.current {
        entry.selection = None;
        return true;
    }
    copy_selection_to_clipboard(entry);
    true
}

/// Finalize selection on mouse-up for the right panel session.
pub fn handle_selection_up_right(app: &mut App, local_row: u16, local_col: u16) -> bool {
    let Some(entry) = active_session_entry_mut_for_tab(app) else {
        return false;
    };
    let sel = match entry.selection.as_mut() {
        Some(s) if s.dragging => s,
        _ => return false,
    };
    sel.current = (local_row, local_col);
    sel.dragging = false;
    // If anchor == current (click with no drag), clear selection.
    if sel.anchor == sel.current {
        entry.selection = None;
        return true;
    }
    copy_selection_to_clipboard(entry);
    true
}

/// Extract the selected text from a session's terminal and copy it to the
/// system clipboard.
fn copy_selection_to_clipboard(entry: &crate::work_item::SessionEntry) {
    let Some(sel) = entry.selection.as_ref() else {
        return;
    };

    let Ok(mut parser) = entry.parser.lock() else {
        return;
    };

    // Set scrollback to match the viewport the user sees.
    let (rows, cols) = parser.screen().size();
    let clamped = entry.scrollback_offset.min(rows as usize);
    parser.set_scrollback(clamped);

    // Translate the selection's inclusive end column into the
    // exclusive form `vt100::Screen::contents_between` expects.
    let (start_row, start_col, end_row, end_col) = selection_to_vt100_bounds(sel, cols);

    let text = parser
        .screen()
        .contents_between(start_row, start_col, end_row, end_col);

    if text.is_empty() {
        return;
    }

    // Copy to system clipboard via the OSC 52 + arboard dual-path
    // helper. This fixes the existing PTY drag-select path over SSH
    // as a side benefit: OSC 52 works when `arboard` can't reach a
    // native display. Return value is intentionally ignored here -
    // this drag-select path does not surface clipboard success /
    // failure to the user (see the Ctrl+C path for the toast-aware
    // wiring).
    let _ = crate::side_effects::clipboard::copy(&text);
}

/// Convert a user-facing `SelectionState` (inclusive end cell) into the
/// bound tuple `vt100::Screen::contents_between` expects (exclusive end
/// column).
///
/// `SelectionState` stores the cell the user's cursor is over when the
/// mouse is released, so `current` is the last highlighted cell. vt100's
/// `contents_between`, by contrast, treats `end_col` as exclusive on both
/// the same-row and multi-row paths (see vt100-0.15.2/src/screen.rs:182
/// and row.rs:98). Passing the inclusive end directly truncates the last
/// character under the cursor - that's the off-by-one this helper fixes.
///
/// The returned `end_col` is clamped to `cols` so a selection that ends
/// on the final column (`cols - 1`) does not overflow past the row.
///
/// The normalization (anchor-vs-current ordering) is delegated to
/// `SelectionState::normalized_bounds` so the highlight renderer in
/// `src/ui.rs` and this clipboard helper agree on exactly one rule for
/// which corner is "start" and which is "end". See that method's
/// rationale for why the logic lives on the struct.
pub fn selection_to_vt100_bounds(sel: &SelectionState, cols: u16) -> (u16, u16, u16, u16) {
    let (start_row, start_col, end_row, end_col) = sel.normalized_bounds();
    let exclusive_end_col = end_col.saturating_add(1).min(cols);
    (start_row, start_col, end_row, exclusive_end_col)
}

#[cfg(test)]
mod encoding_tests {
    use super::encode_mouse_scroll;

    /// SGR encoding produces correct escape sequences for scroll up.
    #[test]
    fn encode_mouse_scroll_sgr_up() {
        let data = encode_mouse_scroll(
            true,
            5,
            10,
            vt100::MouseProtocolMode::PressRelease,
            vt100::MouseProtocolEncoding::Sgr,
        );
        // button=64 (scroll up), col=5+1=6, row=10+1=11
        assert_eq!(data, Some(b"\x1b[<64;6;11M".to_vec()));
    }

    /// SGR encoding produces correct escape sequences for scroll down.
    #[test]
    fn encode_mouse_scroll_sgr_down() {
        let data = encode_mouse_scroll(
            false,
            0,
            0,
            vt100::MouseProtocolMode::AnyMotion,
            vt100::MouseProtocolEncoding::Sgr,
        );
        // button=65 (scroll down), col=0+1=1, row=0+1=1
        assert_eq!(data, Some(b"\x1b[<65;1;1M".to_vec()));
    }

    /// Default (X10) encoding produces correct escape sequences.
    #[test]
    fn encode_mouse_scroll_default() {
        let data = encode_mouse_scroll(
            true,
            2,
            3,
            vt100::MouseProtocolMode::Press,
            vt100::MouseProtocolEncoding::Default,
        );
        // button=64, col=2, row=3
        // bytes: ESC [ M (64+32) (2+1+32) (3+1+32) = ESC [ M 96 35 36
        assert_eq!(data, Some(vec![0x1b, b'[', b'M', 96, 35, 36]));
    }

    /// Default encoding returns None when coordinates exceed the encodable
    /// range (col or row + 1 + 32 > 255).
    #[test]
    fn encode_mouse_scroll_default_overflow() {
        // col = 250: 250 + 1 + 32 = 283 > 255 -> None
        let data = encode_mouse_scroll(
            true,
            250,
            0,
            vt100::MouseProtocolMode::Press,
            vt100::MouseProtocolEncoding::Default,
        );
        assert_eq!(data, None);
    }

    /// When mouse protocol mode is None, scroll converts to arrow key
    /// sequences (3 per tick).
    #[test]
    fn encode_mouse_scroll_no_mode_up() {
        let data = encode_mouse_scroll(
            true,
            0,
            0,
            vt100::MouseProtocolMode::None,
            vt100::MouseProtocolEncoding::Default,
        );
        // 3x Up arrow
        assert_eq!(data, Some(b"\x1b[A\x1b[A\x1b[A".to_vec()));
    }

    /// When mouse protocol mode is None, scroll down converts to Down arrow
    /// sequences (3 per tick).
    #[test]
    fn encode_mouse_scroll_no_mode_down() {
        let data = encode_mouse_scroll(
            false,
            0,
            0,
            vt100::MouseProtocolMode::None,
            vt100::MouseProtocolEncoding::Default,
        );
        // 3x Down arrow
        assert_eq!(data, Some(b"\x1b[B\x1b[B\x1b[B".to_vec()));
    }
}

/// Regression tests for the selection-to-clipboard extraction off-by-one.
///
/// `SelectionState` stores an inclusive end cell (the cell the cursor is
/// over when the mouse is released), but vt100's `contents_between`
/// expects an exclusive end column. Before the fix, the extraction
/// silently truncated the last character; `selection_to_vt100_bounds`
/// is the helper responsible for the inclusive->exclusive conversion.
#[cfg(test)]
pub mod selection_clipboard_tests {
    use super::{SelectionState, selection_to_vt100_bounds};

    /// Build a fresh vt100 parser of the requested size and feed it the
    /// supplied payload. Keeps the test setup tiny and deterministic.
    fn parser_with(rows: u16, cols: u16, payload: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(payload);
        parser
    }

    /// Helper: extract the text a selection would copy, running through
    /// the same `selection_to_vt100_bounds` + `contents_between` path
    /// `copy_selection_to_clipboard` uses at runtime.
    fn extract(parser: &vt100::Parser, sel: &SelectionState) -> String {
        let (_, cols) = parser.screen().size();
        let (sr, sc, er, ec) = selection_to_vt100_bounds(sel, cols);
        parser.screen().contents_between(sr, sc, er, ec)
    }

    /// Single-row selection ending mid-line must include the cell under
    /// the cursor. Before the fix, `end_col = 10` went straight into
    /// `contents_between`, which treats it as exclusive, yielding
    /// "hello worl" instead of "hello world".
    #[test]
    fn single_row_selection_includes_last_cell() {
        let parser = parser_with(5, 40, b"hello world");
        let sel = SelectionState {
            anchor: (0, 0),
            current: (0, 10), // inclusive end on the 'd'
            dragging: false,
        };
        assert_eq!(extract(&parser, &sel), "hello world");
    }

    /// A reversed selection (drag right-to-left) must produce the same
    /// text as the equivalent forward selection;
    /// `SelectionState::normalized_bounds` already handles the ordering,
    /// but we pin the behavior so a future edit to the helper cannot
    /// regress it.
    #[test]
    fn reversed_selection_matches_forward_selection() {
        let parser = parser_with(5, 40, b"hello world");
        let forward = SelectionState {
            anchor: (0, 0),
            current: (0, 10),
            dragging: false,
        };
        let reversed = SelectionState {
            anchor: (0, 10),
            current: (0, 0),
            dragging: false,
        };
        assert_eq!(extract(&parser, &forward), extract(&parser, &reversed));
    }

    /// Multi-row selections exercise the `start_row < end_row` arm of
    /// `contents_between`, which also treats `end_col` as exclusive.
    /// The last character of the end row must be included.
    #[test]
    fn multi_row_selection_includes_end_row_last_cell() {
        // Two rows of distinct content, no auto-wrap: move the cursor
        // explicitly via CR/LF so each line starts at column 0.
        let parser = parser_with(5, 40, b"abcdef\r\nuvwxyz");
        // Select from row 0 col 2 ('c') through row 1 col 5 ('z').
        let sel = SelectionState {
            anchor: (0, 2),
            current: (1, 5),
            dragging: false,
        };
        let text = extract(&parser, &sel);
        assert!(
            text.ends_with('z'),
            "multi-row selection must include final cell, got {text:?}",
        );
        // Full expected text. The start-row contributes "cdef" plus a
        // newline (row not wrapped), then the end row contributes
        // "uvwxyz" - the last 'z' is what the off-by-one previously
        // dropped.
        assert_eq!(text, "cdef\nuvwxyz");
    }

    /// A selection that ends exactly on the final column must not
    /// overflow past `cols`; `selection_to_vt100_bounds` clamps the
    /// exclusive end to `cols` so the vt100 call stays in-bounds and
    /// the final column is still included.
    #[test]
    fn selection_ending_on_last_column_is_clamped() {
        const COLS: u16 = 10;
        // Fill exactly the first row with 10 characters so column 9
        // holds the final 'j'.
        let parser = parser_with(3, COLS, b"abcdefghij");
        let sel = SelectionState {
            anchor: (0, 0),
            current: (0, COLS - 1), // inclusive end on final column
            dragging: false,
        };

        // Confirm the helper's clamp kicks in: inclusive end COLS-1
        // plus one would be COLS, and we want that preserved (not
        // wrapped to COLS+1 or truncated to COLS-1).
        let (_, _, _, exclusive_end) = selection_to_vt100_bounds(&sel, COLS);
        assert_eq!(exclusive_end, COLS);

        let text = extract(&parser, &sel);
        assert_eq!(text, "abcdefghij");
    }

    /// Edge case: a degenerate single-cell selection (anchor ==
    /// current) must still copy exactly one character, not an empty
    /// string. Pre-fix this returned "" because vt100 saw
    /// `start_col == end_col` and short-circuited to empty.
    #[test]
    fn single_cell_selection_copies_one_character() {
        let parser = parser_with(3, 10, b"abcdefghij");
        let sel = SelectionState {
            anchor: (0, 4),
            current: (0, 4), // the 'e'
            dragging: false,
        };
        assert_eq!(extract(&parser, &sel), "e");
    }

    /// Saturation safety: even a pathological `current` column beyond
    /// `cols` (shouldn't happen in practice - mouse events are clipped
    /// upstream - but the helper must not panic) gets clamped to cols.
    #[test]
    fn selection_past_last_column_saturates_to_cols() {
        const COLS: u16 = 10;
        let sel = SelectionState {
            anchor: (0, 0),
            current: (0, u16::MAX),
            dragging: false,
        };
        let (sr, sc, er, ec) = selection_to_vt100_bounds(&sel, COLS);
        assert_eq!((sr, sc, er), (0, 0, 0));
        assert_eq!(ec, COLS, "exclusive end must be clamped to cols");
    }

    /// Symmetry between the visible highlight and the clipboard output.
    ///
    /// The user-facing invariant is "whatever the user sees highlighted
    /// is exactly what lands in the clipboard". Two independent code
    /// paths enforce that: `render_selection_overlay` (`src/ui.rs`)
    /// reverses one `Buffer` cell per highlighted position, and
    /// `selection_to_vt100_bounds` + `contents_between` (`src/event.rs`)
    /// produces the clipboard string. If either side drifts - e.g. the
    /// renderer flips from `..=end_col` to `..end_col`, or the helper
    /// stops adding the inclusive-to-exclusive +1 - the two will
    /// disagree.
    ///
    /// This test drives the real renderer into a `Buffer`, counts the
    /// `Modifier::REVERSED` cells, and asserts that count equals the
    /// number of non-newline characters the clipboard path produces for
    /// the same selection. The per-case rows are filled to full width
    /// so there are no trailing blank cells on either side that could
    /// be counted differently by the two paths.
    ///
    /// If this test fails, check the inclusive/exclusive translation on
    /// both sides before adjusting the assertion: the two must cover
    /// the same cells, not compensate for each other.
    #[test]
    fn highlight_cell_count_matches_clipboard_chars() {
        use ratatui_core::buffer::Buffer;
        use ratatui_core::layout::{Position, Rect};
        use ratatui_core::style::Modifier;

        use crate::ui::render_selection_overlay;

        /// Count cells in `buf` that have `REVERSED` set - i.e. cells
        /// the selection overlay marked as highlighted.
        fn count_reversed(buf: &Buffer, area: Rect) -> usize {
            let mut n = 0;
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell(Position::new(x, y))
                        && cell.modifier.contains(Modifier::REVERSED)
                    {
                        n += 1;
                    }
                }
            }
            n
        }

        // Each case uses rows filled to full terminal width, so vt100
        // will emit exactly `cols` characters per fully-covered row and
        // the renderer will flip exactly `cols` cells per fully-covered
        // row. Multi-row selections include `\n` separators in the
        // clipboard text (one per non-wrapped row boundary); those are
        // filtered out before counting characters because they do not
        // correspond to any highlighted cell.
        struct Case {
            name: &'static str,
            rows: u16,
            cols: u16,
            payload: &'static [u8],
            anchor: (u16, u16),
            current: (u16, u16),
        }

        let cases = [
            Case {
                name: "single-row mid-line",
                rows: 3,
                cols: 10,
                payload: b"abcdefghij",
                anchor: (0, 2),
                current: (0, 7),
            },
            Case {
                name: "single-row to final column",
                rows: 3,
                cols: 10,
                payload: b"abcdefghij",
                anchor: (0, 0),
                current: (0, 9),
            },
            Case {
                name: "reversed (drag right-to-left)",
                rows: 3,
                cols: 10,
                payload: b"abcdefghij",
                anchor: (0, 7),
                current: (0, 2),
            },
            Case {
                // Two 10-col rows fully filled, no wrap: payload
                // writes 10 chars, CR/LF, 10 more chars. Selection
                // covers row 0 cols 3..=9 plus row 1 cols 0..=5.
                name: "multi-row across full-width rows",
                rows: 3,
                cols: 10,
                payload: b"abcdefghij\r\nklmnopqrst",
                anchor: (0, 3),
                current: (1, 5),
            },
        ];

        for case in &cases {
            let parser = parser_with(case.rows, case.cols, case.payload);
            let sel = SelectionState {
                anchor: case.anchor,
                current: case.current,
                dragging: false,
            };

            // Drive the real renderer into a buffer sized to the
            // terminal. The buffer starts empty; render_selection_overlay
            // only touches cells inside the selection, so the pre-existing
            // content does not matter for cell counting.
            let area = Rect::new(0, 0, case.cols, case.rows);
            let mut buf = Buffer::empty(area);
            render_selection_overlay(&mut buf, area, &sel);
            let highlighted = count_reversed(&buf, area);

            // Run the same path copy_selection_to_clipboard uses.
            let clipboard_text = extract(&parser, &sel);
            let clipboard_chars = clipboard_text.chars().filter(|c| *c != '\n').count();

            assert_eq!(
                clipboard_chars, highlighted,
                "case {:?}: clipboard produced {clipboard_chars} chars ({:?}) but overlay highlighted {highlighted} cells",
                case.name, clipboard_text,
            );
            assert!(
                highlighted > 0,
                "case {:?}: sanity check - a non-empty selection must highlight at least one cell",
                case.name,
            );
        }
    }
}
