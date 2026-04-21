//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

/// Pins that once a harness choice is committed, the display
/// name follows that choice (not the static `self.agent_backend`).
#[test]
fn agent_backend_display_name_follows_harness_choice() {
    let mut app = App::new();
    // Seed a selected work item + a harness_choice entry for Codex.
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/wb-display-name.json"));
    app.work_items.push(WorkItem {
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: WorkItemKind::Own,
        title: "display-name-test".into(),
        display_id: Some("test-1".into()),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.build_display_list();
    app.selected_item = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)));
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::Codex);

    assert_eq!(
        app.agent_backend_display_name(),
        AgentBackendKind::Codex.display_name(),
        "display name must follow the per-work-item harness_choice"
    );
    // Swap to Claude and re-check.
    app.harness_choice
        .insert(wi_id, AgentBackendKind::ClaudeCode);
    assert_eq!(
        app.agent_backend_display_name(),
        AgentBackendKind::ClaudeCode.display_name()
    );
}

/// Pins the Codex-only permission marker:
/// `agent_backend_display_name_with_permission_marker` appends
/// `" [!]"` when the committed harness is Codex, and renders
/// unchanged for Claude Code and the neutral
/// `SESSION_TITLE_NONE` placeholder. This is the visible reminder
/// that Codex runs without its built-in sandbox on every spawn
/// path - see README "Per-harness permission model".
#[test]
fn agent_backend_display_name_with_permission_marker_appends_for_codex_only() {
    let mut app = App::new();

    // No committed harness -> neutral placeholder, unmarked.
    assert_eq!(
        app.agent_backend_display_name_with_permission_marker(),
        App::SESSION_TITLE_NONE,
        "neutral placeholder must render unmarked"
    );

    // Seed a selected work item + harness_choice = Claude.
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/wb-marker-test.json"));
    app.work_items.push(WorkItem {
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: WorkItemKind::Own,
        title: "marker-test".into(),
        display_id: Some("marker-1".into()),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.build_display_list();
    app.selected_item = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)));
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);

    // Claude Code renders unmarked.
    assert_eq!(
        app.agent_backend_display_name_with_permission_marker(),
        AgentBackendKind::ClaudeCode.display_name(),
        "Claude Code must render unmarked (no permission marker)"
    );

    // Swap to Codex -> marker must append.
    app.harness_choice.insert(wi_id, AgentBackendKind::Codex);
    let marked = app.agent_backend_display_name_with_permission_marker();
    assert_eq!(
        marked,
        format!(
            "{}{}",
            AgentBackendKind::Codex.display_name(),
            App::PERMISSION_MARKER_CODEX
        ),
        "Codex must render with the permission marker appended, got {marked:?}"
    );
    // Sanity-check the literal marker value stays single-typable
    // characters (global rule: no fancy unicode).
    assert_eq!(
        App::PERMISSION_MARKER_CODEX,
        " [!]",
        "permission marker's literal value is load-bearing for the UI"
    );
}

/// Regression test for the "marker silently drops" divergence
/// bug: when a work item is selected but has NO `harness_choice`
/// entry, and the Ctrl+G drawer is open with global=Codex, the
/// name resolution falls through to the global harness ("Codex")
/// so the marker resolution MUST do the same. Prior to the fix
/// the marker path used `if let Some(id) = ... else if
/// drawer_open` (short-circuit on selected item) while the name
/// path used a proper fall-through, dropping the `" [!]"` in
/// this exact state. Both now delegate to
/// `resolved_harness_kind()`.
#[test]
fn permission_marker_falls_through_to_global_drawer_when_item_has_no_harness_choice() {
    let mut app = App::new();

    // Seed the config with global_assistant_harness = "codex"
    // so `global_assistant_harness_kind()` returns Some(Codex).
    app.config.defaults.global_assistant_harness = Some("codex".into());

    // Seed a selected work item WITHOUT inserting a
    // harness_choice entry for it - this is the state right
    // after a fresh work item is selected and the user opens
    // the Ctrl+G drawer before pressing c / x.
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/wb-marker-fallthrough.json"));
    app.work_items.push(WorkItem {
        id: wi_id,
        backend_type: BackendType::LocalFile,
        kind: WorkItemKind::Own,
        title: "marker-fallthrough".into(),
        display_id: Some("marker-2".into()),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.build_display_list();
    app.selected_item = app
        .display_list
        .iter()
        .position(|e| matches!(e, DisplayEntry::WorkItemEntry(_)));
    assert!(
        app.selected_work_item_id().is_some(),
        "precondition: a work item must be selected"
    );
    assert!(
        app.harness_choice.is_empty(),
        "precondition: no harness_choice entries"
    );

    // Open the Ctrl+G drawer directly (bypassing the first-run
    // modal path) so `global_drawer_open` is true.
    app.global_drawer_open = true;

    // Name resolution falls through item -> global -> "Codex".
    assert_eq!(
        app.agent_backend_display_name(),
        AgentBackendKind::Codex.display_name(),
        "name must fall through to the global harness when the selected item has no harness_choice"
    );

    // Marker resolution MUST do the same fall-through. This is
    // the bit that regressed pre-fix.
    let marked = app.agent_backend_display_name_with_permission_marker();
    assert_eq!(
        marked,
        format!(
            "{}{}",
            AgentBackendKind::Codex.display_name(),
            App::PERMISSION_MARKER_CODEX
        ),
        "marker must fall through to global=Codex and append \" [!]\", got {marked:?}"
    );
}

/// Pins that the first-run Ctrl+G modal opens when the config
/// harness is unset AND at least one harness is on PATH, and
/// that it does NOT open otherwise (configured harness or no
/// available binary).
#[test]
fn ctrl_g_with_unset_harness_opens_first_run_modal_when_available() {
    let mut app = App::new();
    // Precondition: config has no global_assistant_harness.
    assert!(app.config.defaults.global_assistant_harness.is_none());

    app.handle_ctrl_g();

    // If any harness is on PATH, the modal opens. If none is
    // on PATH, a toast surfaces the "no supported harnesses"
    // hint. Either way, the drawer must NOT open directly.
    assert!(
        !app.global_drawer_open,
        "Ctrl+G with unset harness must not open the drawer directly"
    );
    let any_available = AgentBackendKind::all()
        .iter()
        .any(|k| crate::agent_backend::is_available(*k));
    if any_available {
        assert!(
            app.first_run_global_harness_modal.is_some(),
            "Ctrl+G with unset harness + any harness on PATH must open the first-run modal"
        );
    } else {
        assert!(
            app.first_run_global_harness_modal.is_none(),
            "no modal when there is nothing on PATH to pick"
        );
        assert!(
            !app.toasts.is_empty(),
            "a no-harnesses-on-PATH toast must be shown"
        );
    }
}

/// Pins the configured-harness fast path: Ctrl+G with a set
/// harness opens the drawer directly (no modal).
#[test]
fn ctrl_g_with_set_harness_opens_drawer_directly() {
    let mut app = App::new();
    app.config.defaults.global_assistant_harness = Some("claude".into());

    assert!(!app.global_drawer_open);
    app.handle_ctrl_g();

    assert!(
        app.first_run_global_harness_modal.is_none(),
        "configured harness must not open the modal"
    );
    assert!(
        app.global_drawer_open,
        "configured harness must open the drawer directly"
    );
}

/// Pins the first-run modal persistence path: picking a harness
/// saves the canonical name to the config provider and opens the
/// drawer.
#[test]
fn first_run_modal_pick_persists_to_config_provider() {
    // Start with an in-memory config provider so we can reload
    // and verify persistence without touching disk.
    let provider = Box::new(crate::config::InMemoryConfigProvider::new());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(crate::worktree_service::GitWorktreeService),
        provider,
    );

    // Arm the modal directly (bypassing handle_ctrl_g so the
    // test does not depend on PATH).
    app.first_run_global_harness_modal = Some(FirstRunGlobalHarnessModal {
        available_harnesses: vec![AgentBackendKind::ClaudeCode],
    });

    app.finish_first_run_global_pick(AgentBackendKind::ClaudeCode);

    // Modal closed; drawer open.
    assert!(app.first_run_global_harness_modal.is_none());
    assert!(app.global_drawer_open);
    // Config has the canonical name.
    assert_eq!(
        app.config.defaults.global_assistant_harness.as_deref(),
        Some("claude")
    );
    // Reload via the provider to confirm it was persisted.
    let reloaded = app.config_provider.load().unwrap();
    assert_eq!(
        reloaded.defaults.global_assistant_harness.as_deref(),
        Some("claude")
    );
}

/// Pins the modal-cancel path: Esc closes the modal without
/// mutating the config and without opening the drawer.
#[test]
fn first_run_modal_esc_does_not_persist() {
    let provider = Box::new(crate::config::InMemoryConfigProvider::new());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(StubBackend),
        Arc::new(crate::worktree_service::GitWorktreeService),
        provider,
    );

    app.first_run_global_harness_modal = Some(FirstRunGlobalHarnessModal {
        available_harnesses: vec![AgentBackendKind::ClaudeCode],
    });

    app.cancel_first_run_global_pick();

    assert!(app.first_run_global_harness_modal.is_none());
    assert!(!app.global_drawer_open);
    assert!(app.config.defaults.global_assistant_harness.is_none());
    let reloaded = app.config_provider.load().unwrap();
    assert!(reloaded.defaults.global_assistant_harness.is_none());
}

/// Pins the kk double-press kill FSM happy path: two `k` presses
/// within the 1.5s window end the session.
#[test]
fn double_k_within_window_kills_session() {
    let (mut app, wi_id) =
        app_with_work_item(WorkItemStatus::Implementing, Some("f"), Some("/tmp/r"));
    // Insert a fake alive session for the work item so the FSM
    // has something to kill. No PTY / Session child - the
    // `session: None` branch is specifically supported to keep
    // unit tests hermetic.
    let session_key = (wi_id, WorkItemStatus::Implementing);
    app.sessions.insert(
        session_key.clone(),
        crate::work_item::SessionEntry {
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0))),
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );

    // First press arms the hint.
    app.handle_k_press();
    assert!(app.last_k_press.is_some(), "first k must arm");
    assert!(
        app.sessions.contains_key(&session_key),
        "first k must not kill"
    );

    // Second press within the window kills.
    app.handle_k_press();
    assert!(app.last_k_press.is_none(), "second k must clear the arm");
    assert!(
        !app.sessions.contains_key(&session_key),
        "second k must drop the session"
    );
}

/// Pins that a bare `k` press on a row with no live session is a
/// silent no-op (no toast, no state change).
#[test]
fn k_on_work_item_without_session_does_nothing() {
    let (mut app, _wi_id) =
        app_with_work_item(WorkItemStatus::Implementing, Some("f"), Some("/tmp/r"));
    assert!(app.sessions.is_empty());
    app.handle_k_press();
    assert!(
        app.last_k_press.is_none(),
        "k on no-session row must not arm"
    );
    assert!(
        app.toasts.is_empty(),
        "k on no-session row must not push a toast"
    );
}
