use std::path::PathBuf;

use crate::app::{App, DisplayEntry, FocusPanel, RightPanelTab, UserActionKey};
use crate::event::handle_key;
use crate::salsa::ct::event::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn ctrl_backslash_on_dead_terminal_cycles_to_claude_code() {
    use std::sync::{Arc, Mutex};

    use crate::work_item::{
        BackendType, RepoAssociation, SessionEntry, WorkItem, WorkItemId, WorkItemKind,
        WorkItemStatus,
    };

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/tab-dead-terminal.json"));
    app.work_items.push(WorkItem {
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: WorkItemKind::Own,
        title: "Ctrl+\\ cycle test".into(),
        display_id: None,
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![RepoAssociation {
            repo_path: PathBuf::from("/tmp/repo"),
            branch: Some("feature/test".into()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    // Install a dead terminal session for this work item.
    let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.terminal_sessions.insert(
        wi_id,
        SessionEntry {
            parser,
            alive: false,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );

    app.right_panel_tab = RightPanelTab::Terminal;
    app.focus = FocusPanel::Right;

    // Ctrl+\ should cycle to Claude Code, NOT redirect to the left panel.
    // Crossterm 0.28 may deliver the chord either as the literal
    // Char('\\') or as Char('4') (legacy 0x1C mapping); both forms
    // must reach the dispatcher.
    for key in [
        KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL),
    ] {
        // Reset the tab/focus before each variant so the assertions
        // exercise the actual transition rather than the second key
        // being a no-op on an already-cycled state.
        app.right_panel_tab = RightPanelTab::Terminal;
        app.focus = FocusPanel::Right;
        app.status_message = None;

        handle_key(&mut app, key);

        assert!(
            matches!(app.right_panel_tab, RightPanelTab::ClaudeCode),
            "Ctrl+\\ ({key:?}) must flip the dead-terminal tab to Claude Code",
        );
        assert!(
            matches!(app.focus, FocusPanel::Right),
            "focus must stay on the right panel after the Ctrl+\\ ({key:?}) flip",
        );
        let status = app.status_message.as_deref().unwrap_or("");
        assert!(
            !status.contains("returned to work items"),
            "status must not be the dead-session 'returned to work items' message, got: {status}",
        );
    }
}

/// Symmetric regression: `Ctrl+\` on the Claude Code tab when the
/// Claude session has ended must cycle to Terminal (when the work
/// item has a worktree), keeping focus on the right panel. A
/// pre-installed LIVE terminal session makes
/// `spawn_terminal_session()` return early so the test does not
/// fork a real shell.
#[test]
fn ctrl_backslash_on_dead_claude_code_cycles_to_terminal() {
    use std::sync::{Arc, Mutex};

    use crate::work_item::{
        BackendType, RepoAssociation, SessionEntry, WorkItem, WorkItemId, WorkItemKind,
        WorkItemStatus,
    };

    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/tab-dead-claude.json"));
    let wt_path = PathBuf::from("/tmp/tab-dead-claude-worktree");
    app.work_items.push(WorkItem {
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: WorkItemKind::Own,
        title: "Ctrl+\\ cycle test".into(),
        display_id: None,
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![RepoAssociation {
            repo_path: PathBuf::from("/tmp/repo"),
            branch: Some("feature/test".into()),
            worktree_path: Some(wt_path),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    // Install a dead Claude Code session (keyed by (wi_id, status)).
    let dead_parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.sessions.insert(
        (wi_id.clone(), WorkItemStatus::Implementing),
        SessionEntry {
            parser: dead_parser,
            alive: false,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );
    // Pre-install a LIVE terminal session so the Ctrl+\ flip's call
    // to spawn_terminal_session() sees the live entry and returns
    // early - it does NOT fork a real shell from inside the test.
    let live_parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.terminal_sessions.insert(
        wi_id,
        SessionEntry {
            parser: live_parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );

    // Crossterm 0.28 may deliver the chord either as Char('\\') or
    // Char('4') (legacy 0x1C mapping); both forms must reach the
    // dispatcher. Reset the tab/focus before each variant so each
    // assertion exercises a real transition.
    for key in [
        KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL),
    ] {
        app.right_panel_tab = RightPanelTab::ClaudeCode;
        app.focus = FocusPanel::Right;
        app.status_message = None;

        handle_key(&mut app, key);

        assert!(
            matches!(app.right_panel_tab, RightPanelTab::Terminal),
            "Ctrl+\\ ({key:?}) must flip the dead Claude Code tab to Terminal",
        );
        assert!(
            matches!(app.focus, FocusPanel::Right),
            "focus must stay on the right panel after the Ctrl+\\ ({key:?}) flip",
        );
        let status = app.status_message.as_deref().unwrap_or("");
        assert!(
            !status.contains("returned to work items"),
            "status must not be the dead-session 'returned to work items' message, got: {status}",
        );
    }
}

/// Alert dialog: Enter dismisses it.
#[test]
fn alert_dialog_enter_dismisses() {
    let mut app = App::new();
    app.alert_message = Some("Some error".into());

    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_key(&mut app, enter);

    assert!(app.alert_message.is_none());
}

/// Alert dialog: Esc dismisses it.
#[test]
fn alert_dialog_esc_dismisses() {
    let mut app = App::new();
    app.alert_message = Some("Some error".into());

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(app.alert_message.is_none());
}

/// Alert dialog: other keys are swallowed (alert stays visible).
#[test]
fn alert_dialog_swallows_other_keys() {
    let mut app = App::new();
    app.alert_message = Some("Some error".into());

    let key_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
    handle_key(&mut app, key_n);

    // Alert must still be visible - 'n' was swallowed, not passed to the main handler.
    assert!(app.alert_message.is_some());
}

/// Cleanup in-progress: all keys are swallowed, dialog stays open.
#[test]
fn cleanup_in_progress_swallows_keys() {
    let mut app = App::new();
    app.cleanup_prompt_visible = true;
    app.try_begin_user_action(
        UserActionKey::UnlinkedCleanup,
        std::time::Duration::ZERO,
        "Cleaning up unlinked PR...",
    )
    .expect("helper admit should succeed");

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(
        app.cleanup_prompt_visible,
        "dialog should stay open during progress"
    );
    assert!(
        app.is_user_action_in_flight(&UserActionKey::UnlinkedCleanup),
        "in-progress guard must not clear on Esc"
    );
}

/// Delete prompt: Esc cancels the prompt and clears target state.
#[test]
fn delete_prompt_esc_cancels() {
    let mut app = App::new();
    app.delete_prompt_visible = true;
    app.delete_target_title = Some("Test item".into());

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);

    assert!(
        !app.delete_prompt_visible,
        "Esc should dismiss the delete prompt"
    );
    assert!(
        app.delete_target_title.is_none(),
        "target title should be cleared on cancel"
    );
}

/// Delete prompt: unrelated keys are swallowed so stray keystrokes
/// cannot accidentally confirm or leak into the Claude session
/// pane beneath the modal.
#[test]
fn delete_prompt_swallows_other_keys() {
    let mut app = App::new();
    app.delete_prompt_visible = true;

    for ch in ['a', 'n', 'q', ' ', '\u{1b}'] {
        if ch == '\u{1b}' {
            continue; // Esc is tested separately.
        }
        let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        handle_key(&mut app, key);
        assert!(
            app.delete_prompt_visible,
            "prompt should still be visible after pressing '{ch}'"
        );
    }
}

/// Delete in-progress: even the modal's own 'y' confirm is swallowed
/// once the background thread is running. Only Q/Ctrl+Q (force-quit
/// escape hatch) has any effect.
#[test]
fn delete_in_progress_swallows_keys() {
    let mut app = App::new();
    app.delete_prompt_visible = true;
    app.delete_in_progress = true;

    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    handle_key(&mut app, esc);
    let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
    handle_key(&mut app, y);
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    handle_key(&mut app, enter);

    assert!(
        app.delete_prompt_visible,
        "dialog must stay open while cleanup is running"
    );
    assert!(
        app.delete_in_progress,
        "in-progress flag must not clear on stray keys"
    );
}

/// Ctrl+R first press drives through `handle_key` and flips
/// `fetcher_repos_changed = true`. This is the happy-path baseline
/// the two gating tests below depend on (they both assume the
/// first press is admitted through the real dispatch path).
#[test]
fn ctrl_r_first_press_flips_fetcher_repos_changed() {
    let mut app = App::new();
    assert!(!app.fetcher_repos_changed);
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    let changed = handle_key(&mut app, ctrl_r);
    assert!(changed, "Ctrl+R must report state changed");
    assert!(
        app.fetcher_repos_changed,
        "Ctrl+R must set fetcher_repos_changed",
    );
    assert!(
        app.is_user_action_in_flight(&UserActionKey::GithubRefresh),
        "Ctrl+R must have admitted the GithubRefresh helper entry",
    );
}

/// Ctrl+R second press within the debounce window: `handle_key`
/// returns `true` but `fetcher_repos_changed` must NOT be flipped a
/// second time, because the helper entry from the first press is
/// still in flight. This drives through the real `handle_key`
/// dispatch so the debounce/in-flight pre-checks are actually
/// exercised (unlike the unit tests that invoke the helper
/// directly).
#[test]
fn ctrl_r_rapid_double_press_through_handle_key_is_gated() {
    let mut app = App::new();
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);

    // First press admits.
    handle_key(&mut app, ctrl_r);
    assert!(app.fetcher_repos_changed);

    // Simulate the salsa tick consuming the flag (the scheduler
    // reads and resets it once per tick when the restart block
    // runs). We do NOT reset the helper entry - a tight double-
    // press happens BEFORE `drain_fetch_results` has observed any
    // `FetchStarted`, so `activities.pending_fetch_count` is still 0 and the
    // only protection is the helper's in-flight check.
    app.fetcher_repos_changed = false;

    // Second press within the debounce window: the helper's
    // in-flight check rejects, so `fetcher_repos_changed` stays
    // false and a status message is set.
    let changed = handle_key(&mut app, ctrl_r);
    assert!(changed, "handler still returns true on reject path");
    assert!(
        !app.fetcher_repos_changed,
        "second Ctrl+R must not re-flip fetcher_repos_changed",
    );
    assert_eq!(
        app.status_message.as_deref(),
        Some("Refresh already in progress"),
        "in-flight rejection must surface the user-visible message",
    );
}

/// Ctrl+R while `activities.pending_fetch_count > 0`: the hard gate in
/// `handle_key` rejects the press regardless of helper state. This
/// test exercises the exact pre-check added by R1-F-1 - seeding
/// `activities.pending_fetch_count = 1` without touching the helper entry so
/// only the count gate can cause the rejection.
#[test]
fn ctrl_r_rejected_while_pending_fetch_count_nonzero() {
    let mut app = App::new();
    // Seed as if a prior tick's `drain_fetch_results` had counted
    // one `FetchStarted`. No helper entry is inserted, so the
    // in-flight check alone would admit this press.
    app.activities.pending_fetch_count = 1;
    assert!(!app.is_user_action_in_flight(&UserActionKey::GithubRefresh));

    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    let changed = handle_key(&mut app, ctrl_r);
    assert!(changed, "handler returns true even on reject path");
    assert!(
        !app.fetcher_repos_changed,
        "count gate must block the fetcher restart",
    );
    assert!(
        !app.is_user_action_in_flight(&UserActionKey::GithubRefresh),
        "count gate must reject BEFORE the helper is admitted",
    );
    assert_eq!(
        app.status_message.as_deref(),
        Some("Refresh already in progress"),
        "count-gate rejection must surface the user-visible message",
    );
}
