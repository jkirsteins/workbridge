use crate::app::App;
use crate::event::layout::sync_layout;
use crate::event::util::is_ctrl_symbol_char;
use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

/// Key handling when the global assistant drawer is open.
/// Ctrl+G toggles the drawer (closing it, or respawning if the session
/// died). Ctrl+] also closes the drawer. Esc is forwarded to the PTY as
/// \x1b. All other keys are forwarded to the global session PTY using
/// the same encoding as `handle_key_right`.
pub fn handle_global_drawer_key(app: &mut App, key: KeyEvent) -> bool {
    // Ctrl+G toggles the drawer (handles dead-session respawn internally).
    if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
        app.toggle_global_drawer();
        return true;
    }

    // Clear any active text selection on keypress.
    if let Some(entry) = app.global_session.as_mut() {
        entry.selection = None;
    }

    // Exit scrollback mode on any keypress. The key is still forwarded
    // to the PTY so the user seamlessly resumes typing.
    if let Some(entry) = app.global_session.as_mut()
        && entry.scrollback_offset > 0
    {
        entry.scrollback_offset = 0;
    }

    // For any other key, check if the global session is alive. If dead,
    // close the drawer rather than forwarding to a defunct PTY.
    if app.global_session.as_ref().is_none_or(|s| !s.alive) {
        app.global_drawer_open = false;
        app.shell.focus = app.pre_drawer_focus;
        app.shell.status_message = Some("Global assistant session ended".into());
        sync_layout(app);
        return true;
    }

    match key.code {
        // Ctrl+] closes the drawer.
        //
        // The guard goes through `is_ctrl_symbol_char` so we accept
        // both the literal Char(']') and the Char('5') legacy mapping
        // that some terminals emit for the Ctrl+] control byte (0x1D).
        // See `is_ctrl_symbol_char` for the full mapping table.
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && is_ctrl_symbol_char(c, ']') =>
        {
            app.global_drawer_open = false;
            app.shell.focus = app.pre_drawer_focus;
            return true;
        }
        // Forward all other keys to the global session PTY via buffer.
        KeyCode::Esc => {
            app.buffer_bytes_to_global(b"\x1b");
        }
        KeyCode::Enter => {
            app.buffer_bytes_to_global(b"\r");
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let byte = (c.to_ascii_lowercase() as u8)
                    .wrapping_sub(b'a')
                    .wrapping_add(1);
                if byte <= 26 {
                    app.buffer_bytes_to_global(&[byte]);
                }
            } else if key.modifiers.contains(KeyModifiers::ALT) {
                let mut buf = [0u8; 5];
                let s = c.encode_utf8(&mut buf);
                let mut data = vec![0x1bu8];
                data.extend_from_slice(s.as_bytes());
                app.buffer_bytes_to_global(&data);
            } else {
                let mut buf = [0u8; 4];
                let s = c.encode_utf8(&mut buf);
                app.buffer_bytes_to_global(s.as_bytes());
            }
        }
        KeyCode::Backspace => {
            if key.modifiers.contains(KeyModifiers::ALT) {
                app.buffer_bytes_to_global(&[0x1b, 0x7f]);
            } else {
                app.buffer_bytes_to_global(&[0x7f]);
            }
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.buffer_bytes_to_global(b"\x1b[Z");
            } else {
                app.buffer_bytes_to_global(&[0x09]);
            }
        }
        KeyCode::BackTab => {
            app.buffer_bytes_to_global(b"\x1b[Z");
        }
        KeyCode::Up => buffer_global_csi_key(app, b'A', key.modifiers),
        KeyCode::Down => buffer_global_csi_key(app, b'B', key.modifiers),
        KeyCode::Right => buffer_global_csi_key(app, b'C', key.modifiers),
        KeyCode::Left => buffer_global_csi_key(app, b'D', key.modifiers),
        KeyCode::Home => buffer_global_csi_key(app, b'H', key.modifiers),
        KeyCode::End => buffer_global_csi_key(app, b'F', key.modifiers),
        KeyCode::PageUp => {
            app.buffer_bytes_to_global(b"\x1b[5~");
        }
        KeyCode::PageDown => {
            app.buffer_bytes_to_global(b"\x1b[6~");
        }
        KeyCode::Delete => {
            app.buffer_bytes_to_global(b"\x1b[3~");
        }
        KeyCode::F(n) => {
            let seq = f_key_sequence(n);
            app.buffer_bytes_to_global(seq.as_bytes());
        }
        _ => {}
    }
    false
}

/// Buffer a CSI key sequence (arrow, Home, End) for the global PTY.
pub fn buffer_global_csi_key(app: &mut App, key: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        let seq = format!("\x1b[1;{modifier_code}{}", key as char);
        app.buffer_bytes_to_global(seq.as_bytes());
    } else {
        app.buffer_bytes_to_global(&[0x1b, b'[', key]);
    }
}

/// Buffer a CSI key sequence (arrow, Home, End) for the active right-panel PTY.
pub fn buffer_csi_key(app: &mut App, key: u8, modifiers: KeyModifiers) {
    let modifier_code = modifier_param(modifiers);
    if modifier_code > 1 {
        let seq = format!("\x1b[1;{modifier_code}{}", key as char);
        app.buffer_bytes_to_right_panel(seq.as_bytes());
    } else {
        app.buffer_bytes_to_right_panel(&[0x1b, b'[', key]);
    }
}

/// Compute the xterm modifier parameter for ANSI escape sequences.
/// Returns 1 for no modifiers (caller should omit the parameter).
pub const fn modifier_param(modifiers: KeyModifiers) -> u8 {
    let mut code: u8 = 1;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 1;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        code += 2;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 4;
    }
    code
}

/// Return the ANSI escape sequence for a function key F1-F12.
pub fn f_key_sequence(n: u8) -> String {
    match n {
        1 => "\x1bOP".into(),
        2 => "\x1bOQ".into(),
        3 => "\x1bOR".into(),
        4 => "\x1bOS".into(),
        5 => "\x1b[15~".into(),
        6 => "\x1b[17~".into(),
        7 => "\x1b[18~".into(),
        8 => "\x1b[19~".into(),
        9 => "\x1b[20~".into(),
        10 => "\x1b[21~".into(),
        11 => "\x1b[23~".into(),
        12 => "\x1b[24~".into(),
        _ => String::new(),
    }
}
