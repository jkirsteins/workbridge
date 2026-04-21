//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

// -- Branch invariant + "Set branch name" recovery dialog --

/// Helper: spin up an App backed by a real `LocalFileBackend` in a
/// temp directory with one Backlog work item whose repo association
/// has `branch: None`. Returns (app, `wi_id`, `tempdir_guard`); the
/// `TempDir` must be held live by the caller (else its Drop removes
/// the backend's on-disk directory mid-test).
pub fn app_with_branchless_backlog_item(_name: &str) -> (App, WorkItemId, tempfile::TempDir) {
    use crate::work_item_backend::{CreateWorkItem, LocalFileBackend, RepoAssociationRecord};

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let backend = LocalFileBackend::with_dir(dir).unwrap();
    let record = backend
        .create(CreateWorkItem {
            title: "Needs a branch".into(),
            description: None,
            status: WorkItemStatus::Backlog,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/tmp/branchless-repo"),
                branch: None,
                pr_identity: None,
            }],
        })
        .unwrap();
    let wi_id = record.id;

    let mut app = App::with_config(Config::for_test(), Arc::new(backend));
    app.reassemble_work_items();
    app.build_display_list();
    // Position selection on the newly created item.
    app.selected_work_item = Some(wi_id.clone());
    app.build_display_list();

    (app, wi_id, tmp)
}

/// `advance_stage` from a branchless Backlog item must refuse the
/// stage change and open the recovery dialog instead, so the user
/// is not silently moved into Planning with no branch set.
#[test]
fn advance_from_backlog_without_branch_opens_dialog() {
    let (mut app, wi_id, _tmp) = app_with_branchless_backlog_item("advance-opens");

    app.advance_stage();

    assert!(
        app.set_branch_dialog.is_some(),
        "advance_stage should open the Set branch dialog",
    );
    let dlg = app.set_branch_dialog.as_ref().unwrap();
    assert_eq!(dlg.wi_id, wi_id);
    assert!(matches!(
        dlg.pending,
        crate::create_dialog::PendingBranchAction::Advance {
            from: WorkItemStatus::Backlog,
            to: WorkItemStatus::Planning,
        }
    ));
    assert_eq!(
        app.work_items
            .iter()
            .find(|w| w.id == wi_id)
            .unwrap()
            .status,
        WorkItemStatus::Backlog,
        "advance must not mutate status when the branch invariant fails",
    );
}

/// Confirming the Set branch dialog from an advance_stage-triggered
/// open must persist the branch via the backend and then re-drive
/// the same stage change so the work item actually advances.
#[test]
fn confirm_set_branch_dialog_persists_and_advances() {
    let (mut app, wi_id, _tmp) = app_with_branchless_backlog_item("confirm-advance");

    // Open the dialog via the advance path.
    app.advance_stage();
    assert!(app.set_branch_dialog.is_some());

    // Overwrite the prefilled slug with a deterministic value so
    // the assertion below is stable across runs.
    if let Some(dlg) = app.set_branch_dialog.as_mut() {
        dlg.input.clear();
        dlg.input.set_text("user/needs-a-branch-abcd");
    }

    app.confirm_set_branch_dialog();

    assert!(
        app.set_branch_dialog.is_none(),
        "confirm should close the dialog",
    );
    let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
    assert_eq!(
        wi.status,
        WorkItemStatus::Planning,
        "confirm should re-drive the pending stage advance",
    );
    assert_eq!(
        wi.repo_associations[0].branch.as_deref(),
        Some("user/needs-a-branch-abcd"),
        "branch must be persisted to the repo association",
    );
}

/// Confirming the Set branch dialog from a spawn_session-triggered
/// open must persist the branch and re-enter `spawn_session`. Under
/// the `StubWorktreeService`, that path admits a `WorktreeCreate` user
/// action (the background thread never resolves because the stub
/// never sends on its channel, but the single-flight slot IS
/// occupied, which is what we assert here).
#[test]
fn confirm_set_branch_dialog_persists_and_spawns_session() {
    use crate::work_item_backend::{CreateWorkItem, LocalFileBackend, RepoAssociationRecord};

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let backend = LocalFileBackend::with_dir(dir).unwrap();
    // Use Planning so spawn_session proceeds past the
    // Backlog/Done/Mergequeue early-return.
    let record = backend
        .create(CreateWorkItem {
            title: "Resume me".into(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/tmp/branchless-spawn-repo"),
                branch: None,
                pr_identity: None,
            }],
        })
        .unwrap();
    let wi_id = record.id;

    let mut app = App::with_config(Config::for_test(), Arc::new(backend));
    app.reassemble_work_items();
    app.selected_work_item = Some(wi_id.clone());
    app.build_display_list();

    // First Enter press: spawn_session on a branchless item must
    // open the recovery dialog instead of the old dead-end status
    // message.
    app.spawn_session(&wi_id);
    assert!(
        app.set_branch_dialog.is_some(),
        "spawn_session on a branchless item must open the Set branch dialog",
    );
    assert!(matches!(
        app.set_branch_dialog.as_ref().unwrap().pending,
        crate::create_dialog::PendingBranchAction::SpawnSession
    ));

    // Drop the prefilled slug and type a deterministic branch.
    if let Some(dlg) = app.set_branch_dialog.as_mut() {
        dlg.input.clear();
        dlg.input.set_text("user/resume-me-abcd");
    }

    app.confirm_set_branch_dialog();

    assert!(app.set_branch_dialog.is_none());
    let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
    assert_eq!(
        wi.repo_associations[0].branch.as_deref(),
        Some("user/resume-me-abcd"),
        "branch must be persisted before re-driving spawn_session",
    );
    assert!(
        app.is_user_action_in_flight(&UserActionKey::WorktreeCreate),
        "re-driven spawn_session must admit a WorktreeCreate action",
    );
}

/// Esc (`cancel_set_branch_dialog`) must not mutate anything: the
/// work item stays branchless and in Backlog, the backend record
/// on disk is untouched, and there is no lingering dialog state.
#[test]
fn cancel_set_branch_dialog_leaves_item_unchanged() {
    let (mut app, wi_id, _tmp) = app_with_branchless_backlog_item("cancel");

    app.advance_stage();
    assert!(app.set_branch_dialog.is_some());

    app.cancel_set_branch_dialog();

    assert!(app.set_branch_dialog.is_none());
    let wi = app.work_items.iter().find(|w| w.id == wi_id).unwrap();
    assert_eq!(wi.status, WorkItemStatus::Backlog);
    assert!(wi.repo_associations[0].branch.is_none());
}

/// `spawn_session` on a branchless Planning item opens the dialog
/// (regression guard for the old "Set a branch name to start
/// working" dead-end status message at the former `None =>` arm).
#[test]
fn spawn_session_on_branchless_item_opens_dialog_instead_of_message() {
    use crate::work_item_backend::{CreateWorkItem, LocalFileBackend, RepoAssociationRecord};

    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();
    let backend = LocalFileBackend::with_dir(dir).unwrap();
    let record = backend
        .create(CreateWorkItem {
            title: "Dead-end fix".into(),
            description: None,
            status: WorkItemStatus::Planning,
            kind: crate::work_item::WorkItemKind::Own,
            repo_associations: vec![RepoAssociationRecord {
                repo_path: PathBuf::from("/tmp/dead-end-repo"),
                branch: None,
                pr_identity: None,
            }],
        })
        .unwrap();
    let wi_id = record.id;

    let mut app = App::with_config(Config::for_test(), Arc::new(backend));
    app.reassemble_work_items();
    app.selected_work_item = Some(wi_id.clone());
    app.build_display_list();

    app.spawn_session(&wi_id);

    assert!(
        app.set_branch_dialog.is_some(),
        "spawn_session must open the Set branch dialog, not surface a hint string",
    );
    // And it must NOT have left the old dead-end message behind.
    let msg = app.status_message.as_deref().unwrap_or("");
    assert!(
        !msg.contains("Set a branch name"),
        "old dead-end status message should be gone, got: {msg}",
    );
}

/// Session lookup requires matching stage in composite key.
#[test]
fn session_lookup_requires_correct_stage() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/session-key.json"));

    // Insert a mock session entry under (wi_id, Planning).
    let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.sessions.insert(
        (wi_id.clone(), WorkItemStatus::Planning),
        SessionEntry {
            parser,
            alive: true,
            session: None,
            scrollback_offset: 0,
            selection: None,
            agent_written_files: Vec::new(),
        },
    );

    // Lookup with Planning stage finds it.
    assert!(
        app.sessions
            .contains_key(&(wi_id.clone(), WorkItemStatus::Planning))
    );

    // Lookup with Implementing stage does NOT find it.
    assert!(
        !app.sessions
            .contains_key(&(wi_id, WorkItemStatus::Implementing))
    );
}

// -- Issue 7: gh availability check --

/// `check_gh_available` returns a bool (not a panic/error).
/// We do not test for a specific value since CI may or may not have gh.
#[test]
fn check_gh_available_returns_bool() {
    let result: bool = App::check_gh_available();
    // Verify it returns a bool without panicking. The type annotation
    // above confirms the return type at compile time.
    let _ = result;
}

/// `gh_available` is set in the constructor.
#[test]
fn app_constructor_sets_gh_available() {
    let app = App::new();
    // The field should be initialized (to whatever the system has).
    // Just verify the field exists and can be read.
    let _ = app.gh_available;
}

// -- Issue 5: merge conflict detection --

/// Verify the conflict detection string matching logic.
#[test]
fn merge_conflict_detection_logic() {
    // Case-insensitive "conflict" detection in stderr.
    let cases = vec![
        ("CONFLICT (content): Merge conflict in file.rs", true),
        ("error: merge conflict", true),
        ("Conflict detected while merging", true),
        ("Authentication failure", false),
        ("merge was successful", false),
        ("", false),
    ];
    for (stderr, expected) in cases {
        let lower = stderr.to_lowercase();
        let detected = lower.contains("conflict");
        assert_eq!(
            detected, expected,
            "stderr={stderr:?}: expected conflict={expected}, got {detected}",
        );
    }
}

// Argv-shape regression tests for the agent backend live in
// the per-adapter `tests` modules of `crate::agent_backend` (e.g.
// `claude_interactive_argv_for_planning`,
// `claude_interactive_argv_for_blocked_no_auto_start`,
// `claude_review_gate_argv_shape`). They exercise
// `ClaudeCodeBackend::build_command` directly, which is the only
// function that still knows about `claude` flags.

#[test]
fn build_agent_cmd_delegates_to_backend() {
    // Integration-level smoke test: the App-level helper assembles
    // a SpawnConfig and hands it to `self.services.agent_backend`. If a
    // future refactor starts injecting Claude-specific flags here,
    // this test catches it - everything that is not command-name
    // or MCP path must come from the backend.
    let app = App::new();
    let mcp_path = std::path::PathBuf::from("/tmp/fake-mcp.json");
    let cmd = app.build_agent_cmd(
        WorkItemStatus::Planning,
        Some("stage prompt"),
        Some(&mcp_path),
        false,
    );
    assert_eq!(cmd[0], app.services.agent_backend.command_name());
    assert!(
        cmd.iter().any(|s| s == "stage prompt"),
        "system prompt must be forwarded to the backend"
    );
    assert!(
        cmd.iter().any(|s| s == "/tmp/fake-mcp.json"),
        "mcp_config_path must be forwarded to the backend"
    );
    // C7: Planning auto-starts with the `auto_start_default` key.
    // The message content was tightened on 2026-04-18 to defer to
    // the system prompt (RCA: Codex was reading the old
    // "Explain who you are and start working." as a concrete
    // implementation request, bypassing the planning-stage
    // instructions). The current message references the system
    // prompt as the source of truth for the first action; the
    // test pins that substring rather than the full literal
    // so minor wording edits don't require a test update.
    assert!(
        cmd.iter()
            .any(|s| s.to_lowercase().contains("system prompt")),
        "auto_start_default must defer to the system prompt (see prompts/stage_prompts.json, RCA 2026-04-18); got argv: {cmd:?}"
    );
}

#[test]
fn build_agent_cmd_review_with_findings_uses_review_auto_start() {
    let app = App::new();
    let mcp_path = std::path::PathBuf::from("/tmp/fake-mcp.json");
    let cmd = app.build_agent_cmd(
        WorkItemStatus::Review,
        Some("review prompt"),
        Some(&mcp_path),
        true,
    );
    // The auto_start_review message was tightened on 2026-04-18
    // to match `auto_start_default`'s pattern (defer to the
    // system prompt). It now says:
    //   "Follow the instructions in your system prompt. Present
    //    the review gate assessment and the pull request URL
    //    from your system prompt to the user, then wait for
    //    review feedback."
    // We assert the "review gate assessment" substring still
    // reaches the argv (it's the distinguishing token between
    // auto_start_review and auto_start_default).
    assert!(
        cmd.iter()
            .any(|s| s.to_lowercase().contains("review gate assessment")),
        "Review with force_auto_start must use auto_start_review template; got argv: {cmd:?}"
    );
}

#[test]
fn build_agent_cmd_blocked_no_auto_start() {
    let app = App::new();
    let mcp_path = std::path::PathBuf::from("/tmp/fake-mcp.json");
    let cmd = app.build_agent_cmd(
        WorkItemStatus::Blocked,
        Some("prompt"),
        Some(&mcp_path),
        false,
    );
    assert!(
        !cmd.iter().any(|s| s.contains("Explain who you are")),
        "Blocked stage must not auto-start"
    );
}

// -- Feature: global assistant drawer teardown --

/// `teardown_global_session` must clear every piece of global-assistant
/// state: the `SessionEntry`, the MCP server slot, the temp MCP config
/// file (and its path), and any buffered PTY keystrokes. This is what
/// guarantees the next Ctrl+G opening starts from a blank slate.
#[test]
fn teardown_global_session_clears_all_state() {
    let mut app = App::new();

    // Pre-populate a fake SessionEntry with no real PTY child. The
    // `session: None` avoids needing to spawn a real subprocess; the
    // teardown helper skips the `session.kill()` branch when the
    // inner session is None and still runs the rest of the cleanup.
    let parser = Arc::new(std::sync::Mutex::new(vt100::Parser::new(24, 80, 0)));
    app.global_session = Some(SessionEntry {
        parser,
        alive: true,
        session: None,
        scrollback_offset: 0,
        selection: None,
        agent_written_files: Vec::new(),
    });

    // Pre-populate a real temp file as the MCP config path so we
    // can verify teardown actually deletes the file from disk. Use
    // a collision-free unique name under tempfile's tempdir so
    // parallel test threads cannot race on a shared path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let temp_path = tmp.path().join("workbridge-teardown-test.json");
    std::fs::write(&temp_path, b"{}").expect("create temp mcp config");
    assert!(temp_path.exists(), "precondition: temp file exists");
    app.global_mcp_config_path = Some(temp_path.clone());

    // Pre-populate buffered PTY keystrokes that must NOT leak into a
    // freshly-spawned replacement session.
    app.pending_global_pty_bytes
        .extend_from_slice(b"stale-keys");

    app.teardown_global_session();

    assert!(
        app.global_session.is_none(),
        "global_session must be cleared",
    );
    assert!(
        app.global_mcp_server.is_none(),
        "global_mcp_server must be cleared",
    );
    assert!(
        app.global_mcp_config_path.is_none(),
        "global_mcp_config_path must be cleared",
    );
    assert!(
        app.pending_global_pty_bytes.is_empty(),
        "pending_global_pty_bytes must be drained so stale keystrokes \
         don't leak into the next session",
    );
    // File removal runs on a detached background thread via
    // `spawn_agent_file_cleanup` (blocking I/O on the UI thread
    // is forbidden - see `docs/UI.md`), so poll-wait for the
    // file to disappear. The cleanup thread spins up a single
    // `std::fs::remove_file` call, so a short bounded wait is
    // sufficient and deterministic in CI.
    wait_until_file_removed(&temp_path, std::time::Duration::from_secs(5));
    assert!(
        !temp_path.exists(),
        "teardown must delete the temp MCP config file from disk \
         (via the background `spawn_agent_file_cleanup` worker)",
    );
}

/// Calling `teardown_global_session` with no state set must be a no-op
/// and must not panic. The helper runs on every close and every open,
/// so it has to tolerate being called when nothing has been spawned
/// yet (e.g. the very first open of an app run, or the defensive call
/// in the open branch when no previous session exists).
#[test]
fn teardown_global_session_is_idempotent_on_empty_state() {
    let mut app = App::new();
    assert!(app.global_session.is_none());
    assert!(app.global_mcp_config_path.is_none());
    assert!(app.pending_global_pty_bytes.is_empty());

    app.teardown_global_session();

    assert!(app.global_session.is_none());
    assert!(app.global_mcp_server.is_none());
    assert!(app.global_mcp_config_path.is_none());
    assert!(app.pending_global_pty_bytes.is_empty());
}

/// Regression guard for the post-async-spawn keystroke-loss bug:
/// `flush_pty_buffers` must NOT drain `pending_global_pty_bytes`
/// when there is no live `global_session` yet. Before the gate
/// was added, the keystrokes a user typed in the ~one-tick
/// window between `Ctrl+G` opening the drawer and
/// `poll_global_session_open` installing the session were
/// silently lost: `flush_pty_buffers` would `take` the buffer
/// and call `send_bytes_to_global`, which is a no-op when no
/// session exists, dropping the bytes on the floor. The fix is
/// to leave the buffer untouched until a live session can
/// actually consume the bytes.
#[test]
fn flush_pty_buffers_preserves_global_bytes_when_no_session() {
    let mut app = App::new();
    assert!(app.global_session.is_none());

    // User types something while the drawer is opening but
    // before the worker has installed the session.
    app.buffer_bytes_to_global(b"hello");
    assert_eq!(
        app.pending_global_pty_bytes, b"hello",
        "buffer_bytes_to_global should accumulate bytes on the buffer",
    );

    // The next timer tick fires `flush_pty_buffers`. Without
    // the no-session gate this would `take` the buffer and
    // throw the bytes away (the no-op `send_bytes_to_global`
    // path).
    app.flush_pty_buffers();

    assert_eq!(
        app.pending_global_pty_bytes, b"hello",
        "flush_pty_buffers must NOT drain the global buffer when \
         there is no live global_session yet - the keystrokes \
         would be lost otherwise",
    );
}
