//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::{
    AgentBackendKind, App, Config, DisplayEntry, PathBuf, PrIdentityRecord, PrMergePollResult,
    PrMergePollState, PrMergeWatch, RebaseTarget, ReviewGateOrigin, ReviewGateSpawn,
    WorkItemBackend, WorkItemId, WorkItemStatus, install_cached_repo, make_rr_record,
    push_review_work_item, seed_rr_app,
};

/// Feeds a MERGED result. Asserts the merge-gate path ran
/// (activity log carries `source=="pr_merge`"), `pr_identity` was
/// saved, the watch is cleared, the backend status is Done, and
/// the status message is the expected string.
#[test]
fn poll_review_request_merges_merged_advances_to_done_and_clears_watch() {
    let repo_path = PathBuf::from("/tmp/rr-merged");
    let rec = make_rr_record(
        "rr6",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr6"),
    );
    let rec_id = rec.id.clone();
    let (mut app, backend) = seed_rr_app(vec![rec], &repo_path, true);

    // Seed a stale error so we can also confirm it is cleared.
    app.review_request_merge_poll_errors
        .insert(rec_id.clone(), "stale".into());

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: rec_id.clone(),
        pr_state: "MERGED".into(),
        branch: "feature/rr6".into(),
        repo_path,
        pr_identity: Some(PrIdentityRecord {
            number: 42,
            title: "Merged externally".into(),
            url: "https://github.com/owner/repo/pull/42".into(),
        }),
    })
    .unwrap();
    let activity = app.activities.start("test poll");
    app.review_request_merge_polls
        .insert(rec_id.clone(), PrMergePollState { rx, activity });

    app.poll_review_request_merges();

    assert!(
        app.review_request_merge_watches
            .iter()
            .all(|w| w.wi_id != rec_id),
        "watch should be removed after MERGED detection"
    );
    assert!(
        !app.review_request_merge_polls.contains_key(&rec_id),
        "in-flight poll entry should be removed after MERGED detection"
    );
    assert!(
        !app.review_request_merge_poll_errors.contains_key(&rec_id),
        "stale poll error should be cleared on success"
    );

    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("Review request PR merged externally") && msg.contains("[DN]"),
        "status message should reflect external review-request merge, got: {msg}"
    );

    // The backend record is now Done.
    let listed = backend.list().unwrap().records;
    let persisted = listed
        .iter()
        .find(|r| r.id == rec_id)
        .expect("record must still exist");
    assert_eq!(persisted.status, WorkItemStatus::Done);

    // save_pr_identity was called with number=42.
    let saved = backend.saved_identities.lock().unwrap();
    assert!(
        saved
            .iter()
            .any(|(id, _, pr)| *id == rec_id && pr.number == 42),
        "pr_identity must be persisted"
    );

    // The stage_change activity entry carries source=="pr_merge"
    // (the merge-gate invariant).
    let activities = backend.appended_activities.lock().unwrap();
    let stage_change = activities
        .iter()
        .find(|(id, e)| *id == rec_id && e.event_type == "stage_change")
        .expect("stage_change activity must be appended");
    assert_eq!(
        stage_change
            .1
            .payload
            .get("source")
            .and_then(|v| v.as_str()),
        Some("pr_merge"),
        "source must be pr_merge (the merge-gate invariant)"
    );
    // The pr_merged activity entry carries strategy ==
    // "external_review_merge" (distinct from Mergequeue's
    // "external") so metrics can differentiate.
    let merged = activities
        .iter()
        .find(|(id, e)| *id == rec_id && e.event_type == "pr_merged")
        .expect("pr_merged activity must be appended");
    assert_eq!(
        merged.1.payload.get("strategy").and_then(|v| v.as_str()),
        Some("external_review_merge"),
        "strategy must differentiate reviewer-side merges"
    );
}

/// After a merged poll, reassembling with the same backend should
/// now see the persisted `pr_identity` and derive a
/// `PrInfo { state: Merged }` via the existing fallback, with
/// `status_derived = true`.
#[test]
fn poll_review_request_merges_merged_persists_pr_identity_for_fallback() {
    let repo_path = PathBuf::from("/tmp/rr-fallback");
    let rec = make_rr_record(
        "rr7",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr7"),
    );
    let rec_id = rec.id.clone();
    let (mut app, backend) = seed_rr_app(vec![rec], &repo_path, true);

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: rec_id.clone(),
        pr_state: "MERGED".into(),
        branch: "feature/rr7".into(),
        repo_path,
        pr_identity: Some(PrIdentityRecord {
            number: 101,
            title: "Fallback check".into(),
            url: "https://github.com/owner/repo/pull/101".into(),
        }),
    })
    .unwrap();
    let activity = app.activities.start("test poll");
    app.review_request_merge_polls
        .insert(rec_id.clone(), PrMergePollState { rx, activity });

    app.poll_review_request_merges();

    // Reassemble from the backend directly and confirm the
    // assembly fallback produces a Merged PrInfo and
    // status_derived = true.
    let list = backend.list().unwrap();
    let (items, _u, _rr, _reopen) = crate::assembly::reassemble(
        &list.records,
        &app.repo_data,
        &Config::for_test().defaults.branch_issue_pattern,
        &Config::for_test().defaults.worktree_dir,
    );
    let wi = items
        .iter()
        .find(|w| w.id == rec_id)
        .expect("work item should survive reassembly");
    assert_eq!(wi.status, WorkItemStatus::Done);
    assert!(
        wi.status_derived,
        "status_derived must be true so manual transitions are blocked"
    );
    let first_assoc = wi
        .repo_associations
        .first()
        .expect("repo association should exist");
    let pr = first_assoc
        .pr
        .as_ref()
        .expect("assembly fallback should synthesize a PrInfo from pr_identity");
    assert_eq!(pr.state, crate::work_item::PrState::Merged);
    assert_eq!(pr.number, 101);
}

/// Feeds a CLOSED result. The item must NOT transition to Done
/// (that would bypass the merge-gate invariant), the watch stays,
/// and a distinct warning is surfaced.
#[test]
fn poll_review_request_merges_closed_does_not_transition() {
    let repo_path = PathBuf::from("/tmp/rr-closed");
    let rec = make_rr_record(
        "rr8",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr8"),
    );
    let rec_id = rec.id.clone();
    let (mut app, backend) = seed_rr_app(vec![rec], &repo_path, true);

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: rec_id.clone(),
        pr_state: "CLOSED".into(),
        branch: "feature/rr8".into(),
        repo_path,
        pr_identity: None,
    })
    .unwrap();
    let activity = app.activities.start("test poll");
    app.review_request_merge_polls
        .insert(rec_id.clone(), PrMergePollState { rx, activity });

    app.poll_review_request_merges();

    // The backend record must remain in Review.
    let listed = backend.list().unwrap().records;
    let persisted = listed
        .iter()
        .find(|r| r.id == rec_id)
        .expect("record must still exist");
    assert_eq!(persisted.status, WorkItemStatus::Review);

    // The watch stays - the next cycle will retry in case the PR
    // is reopened.
    assert!(
        app.review_request_merge_watches
            .iter()
            .any(|w| w.wi_id == rec_id),
        "watch must remain after CLOSED so retry is possible"
    );

    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("closed without merging") && msg.contains("Ctrl+D"),
        "should warn about closed-without-merge, got: {msg}"
    );
}

/// Feeds an ERROR result. Error is stored in
/// `review_request_merge_poll_errors`, the watch stays, and the
/// next cycle retries.
#[test]
fn poll_review_request_merges_error_persists_on_work_item() {
    let repo_path = PathBuf::from("/tmp/rr-err");
    let rec = make_rr_record(
        "rr9",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr9"),
    );
    let rec_id = rec.id.clone();
    let (mut app, _backend) = seed_rr_app(vec![rec], &repo_path, true);

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: rec_id.clone(),
        pr_state: "ERROR: gh auth failed".into(),
        branch: "feature/rr9".into(),
        repo_path,
        pr_identity: None,
    })
    .unwrap();
    let activity = app.activities.start("test poll");
    app.review_request_merge_polls
        .insert(rec_id.clone(), PrMergePollState { rx, activity });

    app.poll_review_request_merges();

    assert!(
        app.review_request_merge_watches
            .iter()
            .any(|w| w.wi_id == rec_id),
        "watch should remain on ERROR so next cycle retries"
    );
    assert!(
        !app.review_request_merge_polls.contains_key(&rec_id),
        "in-flight poll entry should be drained after ERROR"
    );
    let stored = app
        .review_request_merge_poll_errors
        .get(&rec_id)
        .expect("error should be stored");
    assert!(
        stored.contains("gh auth failed"),
        "stored error should contain gh stderr, got: {stored}"
    );
}

/// Item was deleted between the spawn and the drain. The MERGED
/// result must be discarded and no spurious transition happens.
/// Same safety pattern as `poll_mergequeue`.
#[test]
fn poll_review_request_merges_discards_result_when_item_moved_away() {
    let mut app = App::new();
    let rec_id = WorkItemId::LocalFile(PathBuf::from("/tmp/rr-ghost.json"));
    // Deliberately do NOT push the item into `work_items` -
    // simulates "item was deleted since the poll was spawned."
    app.review_request_merge_watches.push(PrMergeWatch {
        wi_id: rec_id.clone(),
        pr_number: Some(99),
        owner_repo: "owner/repo".into(),
        branch: "feature/ghost".into(),
        repo_path: PathBuf::from("/tmp/ghost-repo"),
        last_polled: Some(crate::side_effects::clock::instant_now()),
    });
    app.review_request_merge_poll_errors
        .insert(rec_id.clone(), "stale".into());

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: rec_id.clone(),
        pr_state: "MERGED".into(),
        branch: "feature/ghost".into(),
        repo_path: PathBuf::from("/tmp/ghost-repo"),
        pr_identity: None,
    })
    .unwrap();
    let activity = app.activities.start("test poll");
    app.review_request_merge_polls
        .insert(rec_id.clone(), PrMergePollState { rx, activity });

    app.poll_review_request_merges();

    assert!(
        app.review_request_merge_watches
            .iter()
            .all(|w| w.wi_id != rec_id),
        "watch for a vanished item should be cleaned up"
    );
    assert!(
        !app.review_request_merge_poll_errors.contains_key(&rec_id),
        "stale error for a vanished item should be cleared"
    );
    let msg = app.shell.status_message.as_deref().unwrap_or("");
    assert!(
        !msg.contains("Review request PR merged externally"),
        "no spurious transition message should be set, got: {msg}"
    );
}

/// Structural test: `poll_review_request_merges` drains
/// pre-populated receivers without shelling out on the UI thread.
/// With no watches in the map, nothing can spawn new subprocess
/// work either. If this test hangs, the function is doing blocking
/// I/O on the main thread.
#[test]
fn poll_review_request_merges_does_not_shell_out_on_ui_thread() {
    let mut app = App::new();
    let rec_id = WorkItemId::LocalFile(PathBuf::from("/tmp/rr-noshell.json"));
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: rec_id.clone(),
        pr_state: "OPEN".into(),
        branch: "feature/noshell".into(),
        repo_path: PathBuf::from("/tmp/noshell-repo"),
        pr_identity: None,
    })
    .unwrap();
    let activity = app.activities.start("test poll");
    app.review_request_merge_polls
        .insert(rec_id, PrMergePollState { rx, activity });

    assert_eq!(app.review_request_merge_polls.len(), 1);

    // Runs synchronously on this thread. No watches are present
    // so Phase 2 cannot spawn anything new.
    app.poll_review_request_merges();

    assert_eq!(
        app.review_request_merge_polls.len(),
        0,
        "ready entry must be drained"
    );
    assert!(
        app.review_request_merge_watches.is_empty(),
        "no watches should have been created by the poll path"
    );
}

/// End-to-end: a `ReviewRequest` item is selected in the left list;
/// after the poll auto-closes it to Done, the existing
/// `WorkItemId`-keyed selection restoration in `build_display_list`
/// must follow it to the DONE group.
#[test]
fn reviewrequest_in_review_stays_selected_after_auto_close_to_done() {
    let repo_path = PathBuf::from("/tmp/rr-sel");
    let rec = make_rr_record(
        "rr10",
        crate::work_item::WorkItemKind::ReviewRequest,
        WorkItemStatus::Review,
        &repo_path,
        Some("feature/rr10"),
    );
    let rec_id = rec.id.clone();
    let (mut app, _backend) = seed_rr_app(vec![rec], &repo_path, true);

    app.selected_work_item = Some(rec_id.clone());
    app.build_display_list();
    assert!(
        app.selected_item.is_some(),
        "selection should be restored for the ReviewRequest item before the poll"
    );

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: rec_id.clone(),
        pr_state: "MERGED".into(),
        branch: "feature/rr10".into(),
        repo_path,
        pr_identity: Some(PrIdentityRecord {
            number: 7,
            title: "Follow selection".into(),
            url: "https://github.com/owner/repo/pull/7".into(),
        }),
    })
    .unwrap();
    let activity = app.activities.start("test poll");
    app.review_request_merge_polls
        .insert(rec_id.clone(), PrMergePollState { rx, activity });

    app.poll_review_request_merges();

    assert_eq!(
        app.selected_work_item.as_ref(),
        Some(&rec_id),
        "selection must stick to the work item across the auto-close"
    );
    let sel = app
        .selected_item
        .expect("selected_item should be set after reassembly");
    let entry = app
        .display_list
        .get(sel)
        .expect("selected index must be valid");
    match entry {
        DisplayEntry::WorkItemEntry(idx) => {
            let wi = &app.work_items[*idx];
            assert_eq!(wi.id, rec_id);
            assert_eq!(wi.status, WorkItemStatus::Done);
        }
        other => panic!("selected entry should be the work item, got {other:?}"),
    }
}

// ---- Milestone 5: harness-selection tests ----

/// Pins that `spawn_review_gate` honors `App::harness_choice` for
/// the per-work-item harness and aborts when the entry is missing
/// (the plan's "abort rather than default to claude" rule).
/// Exercises both halves in one test so a regression that flips
/// either direction is flagged.
#[test]
fn harness_choice_applied_to_review_gate_spawn() {
    // --- Half 1: no harness chosen -> gate aborts with a
    // user-facing "Cannot run review gate" reason, does NOT start
    // a background thread, and does NOT insert a gate entry. ---
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/harness-choice-review-repo");
    install_cached_repo(&mut app, &repo, Some("feature/a"), Some(true));
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/harness-choice-review.json"));
    push_review_work_item(
        &mut app,
        &wi_id,
        &repo,
        "feature/a",
        WorkItemStatus::Implementing,
    );
    // Deliberately do NOT populate `harness_choice`.
    let result = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
    match result {
        ReviewGateSpawn::Blocked(reason) => {
            assert!(
                reason.contains("no harness chosen"),
                "abort reason must name the missing harness choice, got: {reason}"
            );
        }
        ReviewGateSpawn::Spawned => {
            panic!("review gate must abort when no harness is chosen, got Spawned")
        }
    }
    assert!(
        app.review_gates.is_empty(),
        "aborted review gate must not insert a gate entry"
    );

    // --- Half 2: harness chosen -> the gate reaches the branch
    // that inspects the repo state (we assert it gets past the
    // harness check by hitting later branches; `spawn_review_gate`
    // has additional guards for branch / assoc that may still
    // reject, but they fire AFTER our check, so confirming we no
    // longer see "no harness chosen" is enough.) ---
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::Codex);
    let result2 = app.spawn_review_gate(&wi_id, ReviewGateOrigin::Mcp);
    match result2 {
        ReviewGateSpawn::Blocked(reason) => {
            assert!(
                !reason.contains("no harness chosen"),
                "with a chosen harness, the reason must not be the no-harness abort, got: {reason}"
            );
        }
        ReviewGateSpawn::Spawned => {
            // Also acceptable: the gate progressed all the way to
            // spawning the background thread.
        }
    }
}

/// Mirror of `harness_choice_applied_to_review_gate_spawn` for the
/// rebase gate (`App::spawn_rebase_gate`).
#[test]
fn harness_choice_applied_to_rebase_gate_spawn() {
    let mut app = App::new();
    let repo = PathBuf::from("/tmp/harness-choice-rebase-repo");
    install_cached_repo(&mut app, &repo, Some("feature/r"), Some(true));
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/harness-choice-rebase.json"));
    push_review_work_item(
        &mut app,
        &wi_id,
        &repo,
        "feature/r",
        WorkItemStatus::Implementing,
    );

    // Half 1: no harness chosen. `spawn_rebase_gate` returns
    // quietly and populates `status_message` with the no-harness
    // reason; it must NOT admit a user action.
    app.spawn_rebase_gate(RebaseTarget {
        wi_id: wi_id.clone(),
        worktree_path: repo.join(".worktrees/feature/r"),
        branch: "feature/r".to_string(),
    });
    let msg = app
        .shell
        .status_message
        .clone()
        .expect("no-harness abort must surface a status message");
    assert!(
        msg.contains("no harness chosen"),
        "rebase gate abort reason must name the missing harness, got: {msg}"
    );
    assert!(
        !app.rebase_gates.contains_key(&wi_id),
        "aborted rebase must not insert a gate entry"
    );

    // Half 2: populate harness_choice; the gate progresses past
    // the harness check (we verify by the absence of the abort
    // reason; further branches may still reject on worktree state
    // but they fire after the harness check).
    app.harness_choice
        .insert(wi_id.clone(), AgentBackendKind::Codex);
    app.shell.status_message = None;
    app.spawn_rebase_gate(RebaseTarget {
        wi_id: wi_id.clone(),
        worktree_path: repo.join(".worktrees/feature/r"),
        branch: "feature/r".to_string(),
    });
    let post_msg = app.shell.status_message.clone().unwrap_or_default();
    assert!(
        !post_msg.contains("no harness chosen"),
        "with a chosen harness, the abort reason must not appear, got: {post_msg}"
    );
}

/// Pins C14 (harness-contract.md): `agent_backend_display_name`
/// returns the neutral `SESSION_TITLE_NONE` placeholder when no
/// harness is committed to the current context. The UI tab
/// title previously fell through to `self.services.agent_backend.kind()`
/// which is hardcoded `ClaudeCodeBackend`, causing the tab to
/// read "Claude Code" for users who had picked Codex (or no
/// harness at all). That was a user-facing lie; the rule is
/// now `[ABSOLUTE]` in CLAUDE.md.
#[test]
fn agent_backend_display_name_is_neutral_without_committed_harness() {
    let app = App::new();
    // Preconditions: no selected work item, no harness choice,
    // no global drawer, no global-assistant harness configured.
    assert!(app.selected_work_item_id().is_none());
    assert!(app.harness_choice.is_empty());
    assert!(!app.global_drawer.open);
    assert!(
        app.services
            .config
            .defaults
            .global_assistant_harness
            .is_none()
    );

    assert_eq!(
        app.agent_backend_display_name(),
        App::SESSION_TITLE_NONE,
        "uncommitted context must return the neutral placeholder, not a vendor name"
    );
    assert_eq!(
        App::SESSION_TITLE_NONE,
        "Session",
        "the neutral placeholder's literal value is load-bearing for snapshot tests"
    );
    // Must NOT equal any known harness display name.
    for kind in AgentBackendKind::all() {
        assert_ne!(
            app.agent_backend_display_name(),
            kind.display_name(),
            "neutral placeholder must not collide with a vendor display name ({})",
            kind.display_name()
        );
    }
}
