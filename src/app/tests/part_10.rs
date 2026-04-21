//! Subset of app tests; see `src/app/tests/mod.rs` for shared setup.

use super::*;

#[test]
fn poll_pr_merge_no_pr_blocks_done() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-no-pr.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "No PR".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergeResult {
        wi_id: wi_id.clone(),
        branch: "feature/test".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        outcome: PrMergeOutcome::NoPr,
    })
    .unwrap();
    app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
        .expect("helper admit should succeed");
    app.attach_user_action_payload(&UserActionKey::PrMerge, UserActionPayload::PrMerge { rx });
    app.poll_pr_merge();
    let status = app
        .work_items
        .iter()
        .find(|w| w.id == wi_id)
        .unwrap()
        .status;
    assert_eq!(status, WorkItemStatus::Review, "must stay in Review");
    let msg = app.alert_message.as_deref().unwrap_or("");
    assert!(msg.contains("no PR found"), "got: {msg}");
}

#[test]
fn poll_pr_merge_merged_advances_to_done() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/merge-ok.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Merged OK".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergeResult {
        wi_id,
        branch: "feature/test".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        outcome: PrMergeOutcome::Merged {
            strategy: "squash".into(),
            pr_identity: None,
        },
    })
    .unwrap();
    app.try_begin_user_action(UserActionKey::PrMerge, Duration::ZERO, "Merging PR...")
        .expect("helper admit should succeed");
    app.attach_user_action_payload(&UserActionKey::PrMerge, UserActionPayload::PrMerge { rx });
    app.poll_pr_merge();
    // After apply_stage_change, reassemble rebuilds from StubBackend (empty),
    // so we verify via the status message that the merge path was taken.
    let msg = app.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("PR merged") && msg.contains("[DN]"),
        "should confirm merge and Done, got: {msg}",
    );
}

// -- Feature: mergequeue polling --

/// `poll_mergequeue` should advance the item to Done and clear the watch
/// when the drained result reports the PR as MERGED.
#[test]
fn poll_mergequeue_merged_advances_to_done_and_clears_watch() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-merged.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "In mergequeue".into(),
        description: None,
        status: WorkItemStatus::Mergequeue,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.mergequeue_watches.push(PrMergeWatch {
        wi_id: wi_id.clone(),
        pr_number: Some(77),
        owner_repo: "owner/repo".into(),
        branch: "feature/x".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        last_polled: Some(crate::side_effects::clock::instant_now()),
    });
    // Seed a stale poll error to confirm it is cleared on the successful
    // merge detection.
    app.mergequeue_poll_errors
        .insert(wi_id.clone(), "previous failure".into());

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: wi_id.clone(),
        pr_state: "MERGED".into(),
        branch: "feature/x".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        pr_identity: Some(PrIdentityRecord {
            number: 77,
            title: "Feature X".into(),
            url: "https://github.com/owner/repo/pull/77".into(),
        }),
    })
    .unwrap();
    let activity = app.start_activity("test poll");
    app.mergequeue_polls
        .insert(wi_id.clone(), PrMergePollState { rx, activity });

    app.poll_mergequeue();

    assert!(
        app.mergequeue_watches.iter().all(|w| w.wi_id != wi_id),
        "watch should be removed after MERGED detection",
    );
    assert!(
        !app.mergequeue_polls.contains_key(&wi_id),
        "in-flight poll entry should be removed after MERGED detection",
    );
    assert!(
        !app.mergequeue_poll_errors.contains_key(&wi_id),
        "stale poll error should be cleared on success",
    );
    let msg = app.status_message.as_deref().unwrap_or("");
    assert!(
        msg.contains("PR merged") && msg.contains("[DN]"),
        "should confirm external merge and Done, got: {msg}",
    );
}

/// `poll_mergequeue` should record a poll error on ERROR state and leave the
/// watch in place so the next cycle retries.
#[test]
fn poll_mergequeue_error_persists_on_work_item() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-err.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "In mergequeue".into(),
        description: None,
        status: WorkItemStatus::Mergequeue,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.mergequeue_watches.push(PrMergeWatch {
        wi_id: wi_id.clone(),
        pr_number: Some(88),
        owner_repo: "owner/repo".into(),
        branch: "feature/y".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        last_polled: Some(crate::side_effects::clock::instant_now()),
    });

    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: wi_id.clone(),
        pr_state: "ERROR: gh auth failed".into(),
        branch: "feature/y".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        pr_identity: None,
    })
    .unwrap();
    let activity = app.start_activity("test poll");
    app.mergequeue_polls
        .insert(wi_id.clone(), PrMergePollState { rx, activity });

    app.poll_mergequeue();

    assert!(
        app.mergequeue_watches.iter().any(|w| w.wi_id == wi_id),
        "watch should remain on ERROR so next cycle retries",
    );
    assert!(
        !app.mergequeue_polls.contains_key(&wi_id),
        "in-flight poll entry should be drained after ERROR",
    );
    let stored = app
        .mergequeue_poll_errors
        .get(&wi_id)
        .expect("error should be recorded");
    assert!(
        stored.contains("gh auth failed"),
        "error should contain gh stderr, got: {stored}",
    );
}

/// When a watch has `pr_number` = None (the restart path, where the
/// first poll has to fall back to `gh pr view <branch>`) and the
/// result carries a resolved `pr_identity`, the watch's `pr_number`
/// must be backfilled so the next poll targets the exact PR
/// unambiguously. This is the fix for R1-F-3: after the first
/// branch-resolved cycle the watch is pinned and the closed-then-
/// reopened-on-same-branch race can no longer redirect the poll.
#[test]
fn poll_mergequeue_backfills_pr_number_on_first_success() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-backfill.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Restarted mergequeue item".into(),
        description: None,
        status: WorkItemStatus::Mergequeue,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    // Watch starts with pr_number = None, as if reconstructed from a
    // backend record after an app restart where the open-PR fetch had
    // not yet populated assoc.pr.
    app.mergequeue_watches.push(PrMergeWatch {
        wi_id: wi_id.clone(),
        pr_number: None,
        owner_repo: "owner/repo".into(),
        branch: "feature/backfill".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        last_polled: Some(crate::side_effects::clock::instant_now()),
    });

    // Simulate a successful poll returning the PR as still OPEN.
    // The key point is that the result carries a pr_identity with
    // number = 321, which the drain path must pin onto the watch.
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(PrMergePollResult {
        wi_id: wi_id.clone(),
        pr_state: "OPEN".into(),
        branch: "feature/backfill".into(),
        repo_path: PathBuf::from("/tmp/repo"),
        pr_identity: Some(PrIdentityRecord {
            number: 321,
            title: "Backfill test".into(),
            url: "https://github.com/owner/repo/pull/321".into(),
        }),
    })
    .unwrap();
    let activity = app.start_activity("test poll");
    app.mergequeue_polls
        .insert(wi_id.clone(), PrMergePollState { rx, activity });

    app.poll_mergequeue();

    let watch = app
        .mergequeue_watches
        .iter()
        .find(|w| w.wi_id == wi_id)
        .expect("watch should still be present after OPEN result");
    assert_eq!(
        watch.pr_number,
        Some(321),
        "pr_number should be backfilled from the first successful poll",
    );
}

/// Retreating one Mergequeue item must not affect another Mergequeue
/// item's in-flight poll. This is the regression test for the bug
/// the singleton `mergequeue_poll_rx` + activity field caused before
/// the refactor: with two items A and B in Mergequeue and an
/// in-flight poll for A, retreating B used to drop A's poll and
/// activity unconditionally.
#[test]
fn retreat_one_mergequeue_item_does_not_disturb_another_in_flight_poll() {
    let mut app = App::new();

    let wi_a = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-a.json"));
    let wi_b = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-b.json"));
    for (id, branch) in [(&wi_a, "feature/a"), (&wi_b, "feature/b")] {
        app.work_items.push(crate::work_item::WorkItem {
            display_id: None,
            id: id.clone(),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: format!("MQ {branch}"),
            description: None,
            status: WorkItemStatus::Mergequeue,
            status_derived: false,
            repo_associations: vec![],
            errors: vec![],
        });
        app.mergequeue_watches.push(PrMergeWatch {
            wi_id: id.clone(),
            pr_number: Some(1000),
            owner_repo: "owner/repo".into(),
            branch: branch.into(),
            repo_path: PathBuf::from("/tmp/repo"),
            last_polled: Some(crate::side_effects::clock::instant_now()),
        });
    }
    // Build the display list and select item B so retreat_stage acts
    // on it.
    app.display_list.push(DisplayEntry::WorkItemEntry(0));
    app.display_list.push(DisplayEntry::WorkItemEntry(1));
    app.selected_item = Some(1);

    // Spawn a fake in-flight poll for A only. Use a never-completing
    // channel - we only need the entry to exist; we are not calling
    // poll_mergequeue so the rx is never drained.
    let (_tx_a, rx_a) = crossbeam_channel::bounded(1);
    let activity_a = app.start_activity("polling A");
    app.mergequeue_polls.insert(
        wi_a.clone(),
        PrMergePollState {
            rx: rx_a,
            activity: activity_a,
        },
    );

    // Retreat B.
    app.retreat_stage();

    // A's poll must still be present and its activity must still be
    // alive. B must be gone from the watches.
    assert!(
        app.mergequeue_polls.contains_key(&wi_a),
        "retreating B must not drop A's in-flight poll",
    );
    assert!(
        app.activities.iter().any(|a| a.id == activity_a),
        "retreating B must not end A's polling activity",
    );
    assert!(
        app.mergequeue_watches.iter().any(|w| w.wi_id == wi_a),
        "A's watch should remain",
    );
    assert!(
        app.mergequeue_watches.iter().all(|w| w.wi_id != wi_b),
        "B's watch should be removed",
    );
}

/// `reconstruct_mergequeue_watches` should rebuild a watch from just the
/// backend record's branch + the resolved GitHub remote from the
/// cached `repo_data` entry (populated earlier by the background
/// fetcher), with no live `assoc.pr` and no persisted `pr_identity`.
/// This is the critical restart scenario: the PR was merged
/// externally while the app was closed, so the open-PR fetch no
/// longer returns it. The watch must still come back so polling can
/// resume, and the rebuild must never shell out to `git remote
/// get-url` on the UI thread.
#[test]
fn reconstruct_mergequeue_watches_from_branch_only() {
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-restart.json"));
    let repo_path = PathBuf::from("/tmp/repo");
    // Deliberately no pr_identity on the record: this simulates an
    // existing Mergequeue ticket created before `pr_identity` was ever
    // persisted for Mergequeue (the motivating case from the user's
    // report). Reconstruction must still rebuild the watch.
    let record = crate::work_item_backend::WorkItemRecord {
        display_id: None,
        id: wi_id.clone(),
        title: "Was polling".into(),
        description: None,
        status: WorkItemStatus::Mergequeue,
        kind: crate::work_item::WorkItemKind::Own,
        repo_associations: vec![RepoAssociationRecord {
            repo_path: repo_path.clone(),
            branch: Some("feature/z".into()),
            pr_identity: None,
        }],
        plan: None,
        done_at: None,
    };

    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![record]),
    };
    let mut app = App::with_config(Config::for_test(), Arc::new(backend));
    // Seed repo_data with a cached github_remote so reconstruction
    // finds the owner/repo without shelling out. This mirrors the
    // real flow: the background fetcher has already populated
    // repo_data by the time reassemble_work_items runs.
    app.repo_data.insert(
        repo_path.clone(),
        crate::work_item::RepoFetchResult {
            repo_path,
            github_remote: Some(("owner".into(), "repo".into())),
            worktrees: Ok(Vec::new()),
            prs: Ok(Vec::new()),
            review_requested_prs: Ok(Vec::new()),
            current_user_login: None,
            issues: Vec::new(),
        },
    );
    app.reassemble_work_items();

    let watch = app
        .mergequeue_watches
        .iter()
        .find(|w| w.wi_id == wi_id)
        .expect("reconstruction should rebuild the watch from cached github_remote");
    assert_eq!(watch.owner_repo, "owner/repo");
    assert_eq!(watch.branch, "feature/z");
}

/// `reconstruct_mergequeue_watches` must not call
/// `worktree_service.github_remote` (which shells out to `git remote
/// get-url`). When the cached `repo_data.github_remote` is missing,
/// the watch is simply skipped this cycle and will be rebuilt on the
/// next reassembly once the fetcher publishes a result for the repo.
#[test]
fn reconstruct_mergequeue_watches_skips_when_repo_data_missing() {
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/mq-unfetched.json"));
    let record = crate::work_item_backend::WorkItemRecord {
        display_id: None,
        id: wi_id.clone(),
        title: "Not yet fetched".into(),
        description: None,
        status: WorkItemStatus::Mergequeue,
        kind: crate::work_item::WorkItemKind::Own,
        repo_associations: vec![RepoAssociationRecord {
            repo_path: PathBuf::from("/tmp/unfetched-repo"),
            branch: Some("feature/unfetched".into()),
            pr_identity: None,
        }],
        plan: None,
        done_at: None,
    };

    let backend = ArchiveTestBackend {
        records: std::sync::Mutex::new(vec![record]),
    };
    let mut app = App::with_config(Config::for_test(), Arc::new(backend));
    // Deliberately do not seed repo_data, mirroring the cold-start
    // window before the first fetch completes.
    app.reassemble_work_items();

    assert!(
        app.mergequeue_watches.iter().all(|w| w.wi_id != wi_id),
        "watch should be skipped when repo_data has no cached github_remote",
    );
}

// -- Feature: rework prompt on Review -> Implementing --

/// `retreat_stage` from Review sets `rework_prompt_visible` instead of
/// immediately retreating.
#[test]
fn retreat_stage_review_to_implementing_shows_rework_prompt() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/rework-test.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id.clone(),
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Rework test".into(),
        description: None,
        status: WorkItemStatus::Review,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    app.retreat_stage();

    assert!(app.rework_prompt_visible, "should show rework prompt");
    assert_eq!(
        app.rework_prompt_wi.as_ref(),
        Some(&wi_id),
        "rework_prompt_wi should be set",
    );
    // The rework prompt is now a dialog overlay; it no longer sets status_message.
}

/// Rework reasons are stored per work item and influence prompt key.
#[test]
fn rework_reason_stored_per_work_item() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/rework-store.json"));
    app.rework_reasons
        .insert(wi_id.clone(), "Fix the tests".into());

    assert_eq!(
        app.rework_reasons
            .get(&wi_id)
            .map(std::string::String::as_str),
        Some("Fix the tests"),
    );
}

/// `advance_stage` from non-Review stages does NOT show merge prompt.
#[test]
fn advance_stage_non_review_skips_merge_prompt() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/no-merge.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id,
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Backlog item".into(),
        description: None,
        status: WorkItemStatus::Backlog,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    app.advance_stage();

    assert!(
        !app.confirm_merge,
        "merge prompt should not appear for Backlog -> Planning",
    );
}

/// Manual advance from Planning to Implementing is blocked.
#[test]
fn advance_stage_planning_to_implementing_blocked() {
    let mut app = App::new();
    let wi_id = WorkItemId::LocalFile(PathBuf::from("/tmp/plan.json"));
    app.work_items.push(crate::work_item::WorkItem {
        display_id: None,
        id: wi_id,
        backend_type: BackendType::LocalFile,
        kind: crate::work_item::WorkItemKind::Own,
        title: "Planning item".into(),
        description: None,
        status: WorkItemStatus::Planning,
        status_derived: false,
        repo_associations: vec![],
        errors: vec![],
    });
    app.display_list
        .push(DisplayEntry::WorkItemEntry(app.work_items.len() - 1));
    app.selected_item = Some(app.display_list.len() - 1);

    app.advance_stage();

    // Status should still be Planning - manual advance blocked.
    assert_eq!(app.work_items[0].status, WorkItemStatus::Planning);
    assert!(
        app.status_message
            .as_deref()
            .unwrap_or("")
            .contains("workbridge_set_plan"),
    );
}
