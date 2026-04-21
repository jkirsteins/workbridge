use crate::app::{App, UserActionKey};
use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

/// Returns true if `c` is the character that crossterm 0.28's legacy
/// keyboard parser reports for `Ctrl+<symbol>`. Crossterm maps the raw
/// control bytes 0x1C..=0x1F to `KeyCode::Char`('4'..='7') with CONTROL,
/// so a Ctrl+\ press arrives either as the literal '\\' or as '4'
/// depending on the host terminal. This helper centralises the
/// translation so call sites do not need to rediscover the mapping.
pub fn is_ctrl_symbol_char(c: char, symbol: char) -> bool {
    let legacy = match symbol {
        '\\' => Some('4'),
        ']' => Some('5'),
        '^' => Some('6'),
        '_' => Some('7'),
        _ => None,
    };
    c == symbol || Some(c) == legacy
}

/// Returns true if `key` is `Ctrl+<symbol>` regardless of whether
/// crossterm delivered it as the literal symbol or via its legacy
/// control-byte translation. Use this for any Ctrl+\ / Ctrl+] /
/// Ctrl+^ / Ctrl+_ binding so we do not keep one-off matching the
/// numeric form at every call site.
pub fn is_ctrl_symbol(key: KeyEvent, symbol: char) -> bool {
    key.modifiers == KeyModifiers::CONTROL
        && matches!(key.code, KeyCode::Char(c) if is_ctrl_symbol_char(c, symbol))
}

/// Returns `true` when any modal overlay (dialog, confirmation, in-progress
/// operation spinner, etc.) is currently visible. Used by paste/mouse/key
/// handlers to swallow input so stray events cannot reach the underlying
/// PTY or left-panel state while a modal owns the screen. This flag is
/// also the gate that triggers modal-text-input paste routing in
/// `route_paste_to_modal_input`: when it returns `true`, paste events
/// are directed at the focused text input inside the modal (if any)
/// rather than being swallowed or leaked to the PTY.
///
/// Keep this list exhaustive: every new overlay must be added here or the
/// modal will not reliably swallow paste and mouse events.
pub fn any_modal_visible(app: &App) -> bool {
    app.create_dialog.visible
        || app.settings.visible
        || app.rework_prompt_visible
        || app.no_plan_prompt_visible
        || app.branch_gone_prompt.is_some()
        || app.stale_worktree_prompt.is_some()
        || app.stale_recovery_in_progress
        || app.confirm_merge
        || app.cleanup_prompt_visible
        || app.is_user_action_in_flight(&UserActionKey::UnlinkedCleanup)
        || app.merge_in_progress
        || app.delete_prompt_visible
        || app.delete_in_progress
        || app.alert_message.is_some()
        || app.set_branch_dialog.is_some()
}
