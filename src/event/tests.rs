use std::path::PathBuf;

use crate::app::App;
use crate::event::handle_key;
use crate::event::util::is_ctrl_symbol_char;
use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

/// `is_ctrl_symbol_char` must accept both the literal symbol and
/// crossterm 0.28's legacy numeric form for every Ctrl+<symbol>
/// chord whose control byte sits in 0x1C..=0x1F.
#[test]
fn is_ctrl_symbol_char_covers_legacy_mapping() {
    // Literal forms always match.
    assert!(is_ctrl_symbol_char('\\', '\\'));
    assert!(is_ctrl_symbol_char(']', ']'));
    assert!(is_ctrl_symbol_char('^', '^'));
    assert!(is_ctrl_symbol_char('_', '_'));

    // Legacy numeric forms also match.
    assert!(is_ctrl_symbol_char('4', '\\'));
    assert!(is_ctrl_symbol_char('5', ']'));
    assert!(is_ctrl_symbol_char('6', '^'));
    assert!(is_ctrl_symbol_char('7', '_'));

    // Cross-mapping is rejected.
    assert!(!is_ctrl_symbol_char('5', '\\'));
    assert!(!is_ctrl_symbol_char('4', ']'));

    // Unmapped symbols never collide with the legacy table.
    assert!(!is_ctrl_symbol_char('4', 'a'));
    assert!(is_ctrl_symbol_char('a', 'a'));
}

/// F-2: Create dialog is unreachable during shutdown.
/// When `shutting_down` is true, `handle_key` must ignore all keys except
/// Q (force quit). Even if the create dialog was open when shutdown
/// began, it should be closed and no input should reach it.
#[test]
fn create_dialog_closed_during_shutdown() {
    let mut app = App::new();

    // Open the create dialog.
    app.create_dialog.open(&[PathBuf::from("/repo/a")], None);
    assert!(app.create_dialog.visible, "dialog should be open");

    // Begin shutdown.
    app.shell.shutting_down = true;

    // Send a key event (anything, e.g. Enter). handle_key should close
    // the dialog and ignore the key.
    let enter_key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_key(&mut app, enter_key);

    assert!(
        !app.create_dialog.visible,
        "create dialog should be closed during shutdown",
    );
}

/// F-2: Ctrl+N does NOT open the create dialog during shutdown.
#[test]
fn ctrl_n_blocked_during_shutdown() {
    let mut app = App::new();
    app.shell.shutting_down = true;

    let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
    handle_key(&mut app, ctrl_n);

    assert!(
        !app.create_dialog.visible,
        "Ctrl+N should not open create dialog during shutdown",
    );
}

/// Merge prompt: Esc cancels the merge prompt.
#[test]
fn merge_prompt_esc_cancels() {
    let mut app = App::new();
    app.confirm_merge = true;
    app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
        "/tmp/test.json",
    )));
    app.shell.status_message = Some("Merge prompt".into());

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(!app.confirm_merge, "confirm_merge should be cleared");
    assert!(app.merge_wi_id.is_none(), "merge_wi_id should be cleared");
    assert!(
        app.shell.status_message.is_none(),
        "status_message should be cleared",
    );
}

/// Merge prompt: unrecognized key also cancels the prompt.
#[test]
fn merge_prompt_unknown_key_cancels() {
    let mut app = App::new();
    app.confirm_merge = true;
    app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
        "/tmp/test.json",
    )));
    app.shell.status_message = Some("Merge prompt".into());

    let key_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    handle_key(&mut app, key_x);

    assert!(!app.confirm_merge, "confirm_merge should be cleared");
    assert!(app.merge_wi_id.is_none(), "merge_wi_id should be cleared");
}

/// Rework prompt: Esc cancels and stays in Review.
#[test]
fn rework_prompt_esc_cancels() {
    let mut app = App::new();
    app.rework_prompt_visible = true;
    app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
        "/tmp/test.json",
    )));
    app.shell.status_message = Some("Rework reason: ".into());

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(
        !app.rework_prompt_visible,
        "rework_prompt_visible should be cleared",
    );
    assert!(
        app.rework_prompt_wi.is_none(),
        "rework_prompt_wi should be cleared",
    );
    assert!(
        app.shell.status_message.is_none(),
        "status_message should be cleared",
    );
}

/// Rework prompt: typing characters updates the status message.
#[test]
fn rework_prompt_typing_updates_input() {
    let mut app = App::new();
    app.rework_prompt_visible = true;
    app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
        "/tmp/test.json",
    )));

    // Type 'a'
    let key_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
    handle_key(&mut app, key_a);

    assert!(app.rework_prompt_visible, "prompt should still be visible");
    assert_eq!(app.rework_prompt_input.text(), "a");
    // Input is shown in the dialog overlay, not the status bar.
}

/// Rework prompt blocks other keys (settings, quit, etc.).
#[test]
fn rework_prompt_blocks_other_keys() {
    let mut app = App::new();
    app.rework_prompt_visible = true;
    app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
        "/tmp/test.json",
    )));
    app.shell.status_message = Some("Rework reason: ".into());

    // Press 'q' - should type 'q' into the input, not quit.
    let key_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    handle_key(&mut app, key_q);

    assert!(
        !app.shell.should_quit,
        "should not quit while rework prompt is open"
    );
    assert_eq!(app.rework_prompt_input.text(), "q");
}

// -- Feature: no-plan prompt --

/// No-plan prompt: Esc dismisses and clears state.
#[test]
fn no_plan_prompt_esc_dismisses() {
    let mut app = App::new();
    app.no_plan_prompt_visible = true;
    app.no_plan_prompt_queue
        .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));
    app.shell.status_message =
        Some("No plan available. [p] Plan from branch  [Esc] Stay blocked".into());

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(
        !app.no_plan_prompt_visible,
        "no_plan_prompt_visible should be cleared",
    );
    assert!(
        app.no_plan_prompt_queue.is_empty(),
        "no_plan_prompt_queue should be empty",
    );
    assert!(
        app.shell.status_message.is_none(),
        "status_message should be cleared",
    );
}

/// No-plan prompt: Esc with queued items shows next item.
#[test]
fn no_plan_prompt_esc_advances_queue() {
    let mut app = App::new();
    app.no_plan_prompt_visible = true;
    app.no_plan_prompt_queue
        .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/first.json",
        )));
    app.no_plan_prompt_queue
        .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/second.json",
        )));

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(
        app.no_plan_prompt_visible,
        "prompt should remain visible with queued items",
    );
    assert_eq!(
        app.no_plan_prompt_queue.len(),
        1,
        "first item should be popped, second remains",
    );
    // The dialog is now a rendered overlay; status_message is no longer used
    // to show prompt content. Dialog content comes from draw_prompt_dialog().
}

/// No-plan prompt blocks other keys (quit, settings, etc.).
#[test]
fn no_plan_prompt_blocks_other_keys() {
    let mut app = App::new();
    app.no_plan_prompt_visible = true;
    app.no_plan_prompt_queue
        .push_back(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));
    app.shell.status_message = Some("No plan available.".into());

    // Press 'q' - should not quit.
    let key_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    handle_key(&mut app, key_q);

    assert!(
        !app.shell.should_quit,
        "should not quit while no-plan prompt is open",
    );
    assert!(
        app.no_plan_prompt_visible,
        "prompt should still be visible after unrecognized key",
    );
}

/// Merge prompt blocks other keys during shutdown check.
#[test]
fn merge_prompt_blocks_during_active() {
    let mut app = App::new();
    app.confirm_merge = true;
    app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
        "/tmp/test.json",
    )));

    // Press 'q' - should cancel the merge prompt, not quit.
    let key_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    handle_key(&mut app, key_q);

    assert!(
        !app.shell.should_quit,
        "should not quit while merge prompt is open"
    );
    assert!(
        !app.confirm_merge,
        "merge should be cancelled by unknown key"
    );
}

/// Merge in-progress swallows keys (dialog shows spinner, no interaction).
#[test]
fn merge_in_progress_swallows_keys() {
    let mut app = App::new();
    app.confirm_merge = true;
    app.merge_in_progress = true;
    app.merge_wi_id = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
        "/tmp/test.json",
    )));

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(app.confirm_merge, "dialog should stay open during progress");
    assert!(
        app.merge_in_progress,
        "in-progress flag must not clear on Esc"
    );
    assert!(
        app.merge_wi_id.is_some(),
        "merge_wi_id must not clear during progress"
    );
}

// Remaining dispatch-level tests (Ctrl+\ cycling, alert dialog,
// cleanup/delete in-progress, Ctrl+R debounce) live in
// `src/event/tests_dispatch.rs`.
