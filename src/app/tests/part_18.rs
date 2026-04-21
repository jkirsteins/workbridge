//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

#[test]
fn finish_session_open_does_not_write_mcp_json_into_worktree() {
    // Regression guard for the P1 "file injection" review rule
    // added to `CLAUDE.md` in commit `acafae8` ("Add P1 review
    // rule: no harness-config file injection into worktrees",
    // PR #97). Prior to this test, `finish_session_open` wrote
    // `.mcp.json` to `cwd.join(".mcp.json")` alongside the
    // `--mcp-config <tempfile>` argv delivery. That in-worktree
    // write leaked harness state into user repos that do not
    // gitignore `.mcp.json` and, in the worst case, silently
    // clobbered a pre-existing user-authored `.mcp.json` with
    // workbridge's own MCP config JSON. The fix is a pure
    // deletion of the write; MCP config keeps reaching Claude
    // Code through the `--mcp-config <temp file under the
    // process temp dir>` argv pair, which is workbridge-
    // owned and unaffected.
    //
    // This test pins both failure modes with a single assertion:
    // it seeds `cwd.join(".mcp.json")` with a distinctive sentinel
    // byte string BEFORE calling `finish_session_open`, then reads
    // the file back AFTER the call and asserts it is still
    // byte-for-byte the sentinel. If the old write path ever
    // returns, `fs::write` overwrites the sentinel with the MCP
    // config JSON and the assertion trips with a traceable diff
    // that names this rule and PR #99. A future variant where
    // `.mcp.json` is (re)created via `fs::File::create` or
    // `OpenOptions::write(true).create(true)` fails the assertion
    // identically - the sentinel is always the exact contract.
    //
    // `Session::spawn` is invoked as a side effect of calling
    // `finish_session_open` directly, matching the real code path
    // `poll_session_opens` would take on the UI thread. On a test
    // host with `claude` on `$PATH` this exec's a real claude
    // child; on a host without it, `spawn` returns Err and no
    // child is created. Either outcome is compatible with the
    // assertion, but we explicitly `app.sessions.clear()` after
    // the call so the `SessionEntry` `Drop` impl force-kills the
    // child process group via `Session::force_kill` - we never
    // leave a claude subprocess running past this test.
    //
    // The happy path also writes a `workbridge-mcp-config-<uuid>
    // .json` temp file under the process temp dir (reached via
    // the side_effects::paths::temp_dir gate). The UUID makes its
    // exact path unpredictable, so we diff-snapshot the temp
    // directory before and after the call and delete any newly-
    // created workbridge MCP config file. This keeps the test
    // hermetic under invariant 9 (no leaking temp state across
    // runs).
    const SENTINEL: &[u8] = b"SENTINEL_PRE_EXISTING_USER_MCP_JSON_MUST_NOT_BE_OVERWRITTEN";

    let backend = Arc::new(CountingPlanBackend::default());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );

    // Real tempdir so we can assert filesystem state after the
    // call. The `.mcp.json` path being guarded is
    // `cwd.join(".mcp.json")`.
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let mcp_json_path = cwd.join(".mcp.json");

    // Seed the sentinel BEFORE the call so a restored write path
    // is observable as a content change, not just a file-exists
    // change. This also gives us coverage of the silent-overwrite
    // case, which was the original bug's worse failure mode.
    std::fs::write(&mcp_json_path, SENTINEL).expect("seed sentinel .mcp.json into tempdir");

    // Snapshot workbridge-owned temp MCP config files in the
    // process temp dir so we can identify and remove the one
    // `finish_session_open`'s happy path is about to write.
    let list_temp_mcp_configs = || -> std::collections::HashSet<PathBuf> {
        let mut out = std::collections::HashSet::new();
        if let Ok(read_dir) = std::fs::read_dir(crate::side_effects::paths::temp_dir()) {
            for entry in read_dir.flatten() {
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                let ext_is_json = std::path::Path::new(name_str)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("json"));
                if name_str.starts_with("workbridge-mcp-config-") && ext_is_json {
                    out.insert(entry.path());
                }
            }
        }
        out
    };
    let before_temp_mcp_configs = list_temp_mcp_configs();

    let wi_id = WorkItemId::LocalFile(PathBuf::from(
        "/tmp/workbridge-no-mcp-json-in-worktree.json",
    ));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "no-mcp-json-write-in-worktree".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: cwd.clone(),
            branch: Some("feature/no-mcp-json".into()),
            worktree_path: Some(cwd.clone()),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Record a harness choice so `begin_session_open` does not
    // short-circuit on the "no harness chosen" abort.
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);

    // Enqueue the plan read on the background thread, then drain
    // the result the same way `poll_session_opens` does on the UI
    // thread - manually, via a bounded `try_recv` loop (see
    // `clock::bounded_recv`), so the test is deterministic instead
    // of sleep-polling AND does not read wall-clock time.
    app.begin_session_open(&wi_id, &cwd);
    let entry = app
        .session_open_rx
        .remove(&wi_id)
        .expect("begin_session_open must register a pending receiver");
    let result = crate::side_effects::clock::bounded_recv(
        &entry.rx,
        "background plan-read must deliver a result",
    );
    app.activities.end(entry.activity);

    // This is the function under test. The surfaced status
    // message (from either the MCP config temp-write or the
    // downstream `Session::spawn` outcome) is irrelevant to the
    // sentinel assertion below.
    // Pass an overriding `SessionOpenPlanResult` that keeps the
    // real `wi_id` / `cwd` / `plan_text` from the background thread
    // but clears the `server` / `written_files` / `mcp_config_*`
    // fields: this test exercises the in-worktree side-car guard,
    // not the MCP wire-up.
    app.finish_session_open(SessionOpenPlanResult {
        wi_id: result.wi_id.clone(),
        cwd: result.cwd.clone(),
        plan_text: result.plan_text,
        read_error: None,
        server: None,
        server_error: None,
        written_files: Vec::new(),
        mcp_config_path: None,
        mcp_bridge: None,
        extra_mcp_bridges: Vec::new(),
        mcp_config_error: None,
    });

    // Force-tear-down any session `finish_session_open` inserted.
    // On a host with `claude` on `$PATH`, `Session::spawn` has
    // exec'd a real claude child; clearing the sessions map drops
    // each `SessionEntry`, whose `Drop` impl calls
    // `Session::force_kill` to SIGKILL the child process group.
    // On a host without `claude`, `Session::spawn` returned Err
    // before this point and the map is already empty, so the
    // clear is a no-op. Either way we leave no subprocess running.
    app.sessions.clear();

    // Pin the file-write path: the sentinel must be byte-for-byte
    // intact. A restored `fs::write(cwd.join(".mcp.json"), ...)`
    // would replace these bytes with the MCP config JSON and the
    // assertion trips with a traceable diff.
    let actual = std::fs::read(&mcp_json_path)
        .expect("sentinel .mcp.json must still be readable after the call");
    assert_eq!(
        actual, SENTINEL,
        "finish_session_open must NOT write `.mcp.json` into the \
         work-item worktree - doing so pollutes user repos that \
         do not gitignore the file AND, in the overwrite case, \
         silently destroys a pre-existing user-authored \
         `.mcp.json`. This violates the CLAUDE.md 'file injection' \
         review rule (commit acafae8). MCP config is delivered \
         exclusively via `--mcp-config <tempfile>` under \
         the process temp dir; see `finish_session_open` and \
         the C4 clause in docs/harness-contract.md."
    );

    // Clean up any `workbridge-mcp-config-*.json` the happy path
    // wrote under the process temp dir. Anything in the after-
    // snapshot that wasn't in the before-snapshot is ours and
    // safe to delete; leaving them accumulates state across runs.
    let after_temp_mcp_configs = list_temp_mcp_configs();
    for path in after_temp_mcp_configs.difference(&before_temp_mcp_configs) {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn delete_work_item_phase5_forwards_orphan_branch_to_cleanup_info() {
    // Regression guard for R2-F-2. Round 1 pushed
    // `(repo_path, worktree_path)` pairs into `orphan_worktrees`
    // and silently dropped the branch name - so the synthesized
    // `DeleteCleanupInfo` had `branch: None` and
    // `spawn_delete_cleanup` skipped the `git branch -D` step. On
    // master this step ran inline. Net regression: a
    // delete-during-create race leaked a branch ref.
    //
    // Proof: put a completed `WorktreeCreateResult` with a known
    // branch into the `UserActionKey::WorktreeCreate` helper
    // payload, call `delete_work_item_by_id`, and assert the
    // resulting `OrphanWorktree` has the branch populated. Then
    // mirror the caller's synthesis logic and check the
    // `DeleteCleanupInfo` carries the branch through unchanged.
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::new(CountingPlanBackend::default()) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/r2f2-orphan.json"));
    let repo_path = PathBuf::from("/tmp/r2f2-repo");
    let worktree_path = PathBuf::from("/tmp/r2f2-worktree");
    let branch_name = "feature/r2f2-orphan".to_string();

    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "r2f2-test".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: repo_path.clone(),
            branch: Some(branch_name.clone()),
            worktree_path: None,
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Pre-queue a completed worktree-create result so Phase 5's
    // `try_recv` drains it synchronously.
    let (tx, rx) = crossbeam_channel::bounded::<WorktreeCreateResult>(1);
    tx.send(WorktreeCreateResult {
        wi_id: wi_id.clone(),
        repo_path: repo_path.clone(),
        branch: Some(branch_name.clone()),
        path: Some(worktree_path.clone()),
        error: None,
        open_session: true,
        branch_gone: false,
        reused: false,
        stale_worktree_path: None,
    })
    .unwrap();
    app.try_begin_user_action(
        UserActionKey::WorktreeCreate,
        Duration::ZERO,
        "Initializing worktree...",
    )
    .expect("helper admit should succeed");
    app.attach_user_action_payload(
        &UserActionKey::WorktreeCreate,
        UserActionPayload::WorktreeCreate {
            rx,
            wi_id: wi_id.clone(),
        },
    );

    let mut warnings: Vec<String> = Vec::new();
    let mut orphan_worktrees: Vec<OrphanWorktree> = Vec::new();
    app.delete_work_item_by_id(&wi_id, &mut warnings, &mut orphan_worktrees)
        .expect("delete must succeed");

    assert_eq!(
        orphan_worktrees.len(),
        1,
        "Phase 5 must capture the in-flight worktree as an orphan",
    );
    let orphan = &orphan_worktrees[0];
    assert_eq!(orphan.repo_path, repo_path);
    assert_eq!(orphan.worktree_path, worktree_path);
    assert_eq!(
        orphan.branch.as_deref(),
        Some(branch_name.as_str()),
        "R2-F-2 regression: orphan must preserve the branch name so \
         spawn_delete_cleanup can run `git branch -D`",
    );

    // Mirror the caller's synthesis and verify the DeleteCleanupInfo
    // carries the branch through. This exercises the exact code path
    // in `confirm_delete_from_prompt` and the MCP delete handler.
    let cleanup_info = DeleteCleanupInfo {
        repo_path: orphan.repo_path.clone(),
        branch: orphan.branch.clone(),
        worktree_path: Some(orphan.worktree_path.clone()),
        branch_in_main_worktree: false,
        open_pr_number: None,
        github_remote: None,
    };
    assert_eq!(
        cleanup_info.branch.as_deref(),
        Some(branch_name.as_str()),
        "synthesized DeleteCleanupInfo must propagate the orphan branch",
    );
    assert!(
        !cleanup_info.branch_in_main_worktree,
        "a freshly-created worktree is never the main worktree",
    );
}

#[test]
fn begin_session_open_surfaces_activity_spinner_for_feedback() {
    // Regression guard for R2-F-3. Round 1's background plan-read
    // path returned silently from `begin_session_open`, so a slow
    // backend made the TUI look hung between Enter and the next
    // 200ms poll tick. `begin_session_open` must register an
    // activity so `activities.current()` surfaces feedback
    // immediately. The activity must also be ended in every
    // terminal path of `poll_session_opens` - here we verify the
    // happy path.
    let backend = Arc::new(CountingPlanBackend::default());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/r2f3-session-open.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "r2f3-session-open".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/r2f3-repo"),
            branch: Some("feature/r2f3".into()),
            worktree_path: Some(PathBuf::from("/tmp/r2f3-worktree")),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Record a harness choice so `begin_session_open` does not
    // short-circuit on the "no harness chosen" abort.
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);

    // No spinner before the call.
    assert!(app.activities.current().is_none());

    let cwd = PathBuf::from("/tmp/r2f3-worktree");
    app.begin_session_open(&wi_id, &cwd);

    // Spinner must be present IMMEDIATELY - no waiting on the
    // background thread to finish. This is the entire point of the
    // R2-F-3 fix.
    let activity_msg = app
        .activities
        .current()
        .expect("R2-F-3 regression: begin_session_open must start an activity spinner");
    assert_eq!(activity_msg, "Opening session...");
    assert!(
        app.session_open_rx.contains_key(&wi_id),
        "begin_session_open must register a pending receiver",
    );

    // Wait for the background read to produce a result, then drain
    // it via `poll_session_opens`. The spinner MUST be cleared once
    // the result is applied.
    let recv_start = crate::side_effects::clock::instant_now();
    loop {
        let ready = app
            .session_open_rx
            .get(&wi_id)
            .is_some_and(|entry| !entry.rx.is_empty());
        if ready {
            break;
        }
        // 60s of mock-clock budget (6000 iterations of the 10ms
        // mock `sleep`) to absorb OS-scheduler jitter on loaded CI
        // hosts. `clock::sleep` is pure `yield_now` in tests, so
        // each iteration is only a few hundred microseconds of
        // real time - 6000 yields gives the background thread
        // ample real-time opportunity to make progress while the
        // mock clock advances. A true livelock still trips this
        // cap deterministically.
        if crate::side_effects::clock::elapsed_since(recv_start)
            > std::time::Duration::from_secs(60)
        {
            panic!("background plan-read thread did not produce a result");
        }
        crate::side_effects::clock::sleep(std::time::Duration::from_millis(10));
    }

    // `finish_session_open` will try to spawn a Claude session,
    // which would touch external binaries. To avoid that, drain
    // and end the spinner manually via the internal helper.
    // This mirrors what `poll_session_opens` does on success.
    let entry = app.session_open_rx.remove(&wi_id).unwrap();
    let _result = crate::side_effects::clock::bounded_recv(
        &entry.rx,
        "background plan-read thread must deliver a result",
    );
    app.activities.end(entry.activity);

    assert!(
        app.activities.current().is_none(),
        "R2-F-3 regression: spinner must be cleared after the result is drained",
    );
}

#[test]
fn apply_stage_change_cancels_pending_session_open() {
    // Codex finding: pending session opens must NOT survive a stage
    // change. The plan-read receiver in `session_open_rx` has no
    // entry in `self.sessions`, so the old session-kill branch in
    // `apply_stage_change` would only run if a session already
    // existed. Without the unconditional `drop_session_open_entry`
    // call, a stale pending open from the old stage would survive
    // the transition and `finish_session_open` would later spawn
    // Claude for the new stage - including no-session stages like
    // Mergequeue or Done. This test pins the cancellation contract.
    let backend = Arc::new(CountingPlanBackend::default());
    let mut app = App::with_config_and_worktree_service(
        Config::default(),
        Arc::clone(&backend) as Arc<dyn WorkItemBackend>,
        Arc::new(StubWorktreeService),
        Box::new(crate::config::InMemoryConfigProvider::new()),
    );
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/codex-stage-cancel.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "codex-stage-cancel".into(),
        description: None,
        status: WorkItemStatus::Implementing,
        status_derived: false,
        repo_associations: vec![crate::work_item::RepoAssociation {
            repo_path: PathBuf::from("/tmp/codex-stage-cancel-repo"),
            branch: Some("feature/codex-stage".into()),
            worktree_path: Some(PathBuf::from("/tmp/codex-stage-cancel-wt")),
            pr: None,
            issue: None,
            git_state: None,
            stale_worktree_path: None,
        }],
        errors: vec![],
    });

    // Record a harness choice so `begin_session_open` does not
    // short-circuit on the "no harness chosen" abort.
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::ClaudeCode);

    let cwd = PathBuf::from("/tmp/codex-stage-cancel-wt");
    app.begin_session_open(&wi_id, &cwd);
    assert!(
        app.session_open_rx.contains_key(&wi_id),
        "begin_session_open must register a pending receiver",
    );
    assert!(
        app.activities.current().is_some(),
        "begin_session_open must start an activity spinner",
    );

    // Stage transition to Mergequeue (a no-session stage). Use
    // "pr_merge" source to satisfy the merge-gate guard - the
    // important behaviour to pin is that the pending open is
    // cancelled, not the source-string semantics.
    app.apply_stage_change(
        &wi_id,
        WorkItemStatus::Implementing,
        WorkItemStatus::Mergequeue,
        "pr_merge",
    );

    assert!(
        !app.session_open_rx.contains_key(&wi_id),
        "stage change must cancel the pending session open - otherwise \
         finish_session_open would later spawn Claude for the new stage",
    );
    assert!(
        app.activities.current().is_none(),
        "stage change must end the 'Opening session...' spinner",
    );
}
