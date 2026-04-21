    #[test]
    fn board_view_item_in_every_column_120x40() {
        let items = vec![
            make_work_item(
                "bl1",
                "Add response caching layer",
                WorkItemStatus::Backlog,
                None,
                1,
            ),
            make_work_item(
                "pl1",
                "Refactor auth middleware",
                WorkItemStatus::Planning,
                None,
                1,
            ),
            make_work_item(
                "im1",
                "Fix race condition in fetcher",
                WorkItemStatus::Implementing,
                None,
                1,
            ),
            make_work_item(
                "rv1",
                "Update CI pipeline config",
                WorkItemStatus::Review,
                Some(make_pr_info(42, CheckStatus::Passing)),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        app.view_mode = ViewMode::Board;
        app.sync_board_cursor();
        // At 120x40, each column is 30 wide (28 inner). No title should clip.
        insta::assert_snapshot!(render(&mut app, 120, 40));
    }

    // -- Prompt dialog snapshot tests --


    #[test]
    fn merge_prompt_dialog() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_wi_id = Some(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn merge_progress_dialog() {
        let mut app = App::new();
        app.confirm_merge = true;
        app.merge_in_progress = true;
        app.merge_wi_id = Some(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        app.spinner_tick = 3;
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn rework_prompt_dialog() {
        let mut app = App::new();
        app.rework_prompt_visible = true;
        app.rework_prompt_wi = Some(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        app.rework_prompt_input
            .set_text("needs more error handling");
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn no_plan_prompt_dialog() {
        let mut app = App::new();
        app.no_plan_prompt_visible = true;
        app.no_plan_prompt_queue
            .push_back(WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")));
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn cleanup_confirm_dialog() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        app.cleanup_unlinked_target =
            Some((PathBuf::from("/tmp/repo"), "feature-branch".to_string(), 42));
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn cleanup_reason_dialog() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        app.cleanup_reason_input_active = true;
        app.cleanup_unlinked_target =
            Some((PathBuf::from("/tmp/repo"), "feature-branch".to_string(), 42));
        app.cleanup_reason_input.set_text("closing - abandoned");
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn cleanup_progress_dialog() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        // Simulate an in-flight unlinked cleanup by admitting the
        // helper entry directly and then ending the visible
        // status-bar activity. This mirrors spawn_unlinked_cleanup,
        // which hides the status-bar spinner so only the in-dialog
        // spinner is shown.
        let aid = app
            .try_begin_user_action(
                UserActionKey::UnlinkedCleanup,
                std::time::Duration::ZERO,
                "Cleaning up unlinked PR...",
            )
            .expect("helper admit should succeed");
        app.end_activity(aid);
        app.cleanup_progress_pr_number = Some(42);
        app.spinner_tick = 3; // deterministic spinner frame
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn alert_dialog() {
        let mut app = App::new();
        app.alert_message = Some("PR close failed: permission denied".to_string());
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    // -- Sticky group header tests --


    #[test]
    fn sticky_header_visible_when_scrolled() {
        // Create enough items to force scrolling in a short viewport.
        // All items are in the same repo so they share one ACTIVE header.
        let items = vec![
            make_work_item("a1", "Item A1", WorkItemStatus::Implementing, None, 1),
            make_work_item("a2", "Item A2", WorkItemStatus::Implementing, None, 1),
            make_work_item("a3", "Item A3", WorkItemStatus::Implementing, None, 1),
            make_work_item("a4", "Item A4", WorkItemStatus::Implementing, None, 1),
            make_work_item("a5", "Item A5", WorkItemStatus::Implementing, None, 1),
            make_work_item("b1", "Item B1", WorkItemStatus::Backlog, None, 1),
            make_work_item("b2", "Item B2", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Select the last selectable item to force the viewport to scroll.
        // Set the recenter flag so the render simulates the viewport
        // that keyboard navigation would produce (the new decoupled
        // viewport does NOT auto-scroll on selection - wheel scrolls
        // park it, keyboard navigation recenters it).
        if let Some(pos) = app.display_list.iter().rposition(is_selectable) {
            app.selected_item = Some(pos);
            app.recenter_viewport_on_selection.set(true);
        }
        // Short viewport forces the ACTIVE header off-screen -> sticky.
        insta::assert_snapshot!(render(&mut app, 80, 12));
    }


    #[test]
    fn no_sticky_header_at_top_of_list() {
        // With only a few items, the header is always visible at the top.
        let items = vec![
            make_work_item("a", "Item A", WorkItemStatus::Implementing, None, 1),
            make_work_item("b", "Item B", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Offset 0, header is visible - no sticky header should appear.
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn no_sticky_header_in_drill_down() {
        // In board drill-down mode, the display list has no group headers.
        let items = vec![
            make_work_item("a", "Item A", WorkItemStatus::Implementing, None, 1),
            make_work_item("b", "Item B", WorkItemStatus::Implementing, None, 1),
            make_work_item("c", "Item C", WorkItemStatus::Implementing, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        app.board_drill_stage = Some(WorkItemStatus::Implementing);
        app.board_drill_down = true;
        app.build_display_list();
        app.selected_item = app.display_list.iter().rposition(is_selectable);
        insta::assert_snapshot!(render(&mut app, 80, 12));
    }


    #[test]
    fn work_item_mergequeue_hint_and_pr_url() {
        let items = vec![make_work_item(
            "mq-1",
            "Waiting for merge",
            WorkItemStatus::Mergequeue,
            Some(make_pr_info(42, CheckStatus::Passing)),
            1,
        )];
        let mut app = app_with_items(items, vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);

        let rendered = render(&mut app, 100, 30);
        assert!(
            rendered.contains("Waiting for PR to be merged"),
            "should render mergequeue hint: {rendered}"
        );
        assert!(
            rendered.contains("Shift+Left"),
            "should mention Shift+Left to cancel: {rendered}"
        );
        assert!(
            rendered.contains("https://github.com/o/r/pull/42"),
            "should render full PR URL: {rendered}"
        );
    }

    /// Regression for F-2: long PR URLs must not lose horizontal space
    /// to the field-label prefix. Before the fix the URL was rendered as
    /// a labelled row (`  PR URL      <url>`), which left only ~40 cols
    /// of value space; a real URL would clip well before the panel edge.
    /// The fix renders the URL on its own dedicated line after the field
    /// block, so it uses the full inner width of the right pane and only
    /// clips at the terminal boundary itself.

    #[test]
    fn work_item_long_pr_url_uses_full_panel_width() {
        let mut item = make_work_item(
            "long-url",
            "Has long URL",
            WorkItemStatus::Review,
            Some(make_pr_info(123_456, CheckStatus::Passing)),
            1,
        );
        let long_url =
            "https://github.com/very-long-org-name/very-long-repo-name/pull/123456".to_string();
        item.repo_associations[0].pr.as_mut().unwrap().url = long_url.clone();

        let mut app = app_with_items(vec![item], vec![]);
        app.selected_item = app.display_list.iter().position(is_selectable);

        // At a wide terminal the entire URL fits and must appear in full.
        let wide = render(&mut app, 160, 30);
        assert!(
            wide.contains(&long_url),
            "long PR URL should appear in full at 160-col width:\n{wide}"
        );

        // At 80 cols the right pane is narrower than the URL, so the URL
        // necessarily clips at the panel boundary - but it must clip
        // strictly later than the old labelled-row layout would have. The
        // old layout reserved ~14 cols for the label prefix, so any
        // visible URL prefix longer than 14 chars + 14 chars (~28) of URL
        // body proves the dedicated-line layout is in use. Use 40 chars
        // as a comfortable lower bound that the labelled-row layout could
        // never have produced.
        let narrow = render(&mut app, 80, 24);
        let prefix = &long_url[..40];
        assert!(
            narrow.contains(prefix),
            "narrow render should still show at least the first 40 chars of \
             the URL on a dedicated line; got:\n{narrow}"
        );
    }


    #[test]
    fn sticky_header_shows_correct_group_when_multiple_groups() {
        // Create items across two groups - when scrolled to the second group,
        // the second group's header should be sticky (not the first).
        let items = vec![
            make_work_item("a1", "Active 1", WorkItemStatus::Implementing, None, 1),
            make_work_item("a2", "Active 2", WorkItemStatus::Implementing, None, 1),
            make_work_item("a3", "Active 3", WorkItemStatus::Implementing, None, 1),
            make_work_item("b1", "Backlog 1", WorkItemStatus::Backlog, None, 1),
            make_work_item("b2", "Backlog 2", WorkItemStatus::Backlog, None, 1),
            make_work_item("b3", "Backlog 3", WorkItemStatus::Backlog, None, 1),
            make_work_item("b4", "Backlog 4", WorkItemStatus::Backlog, None, 1),
            make_work_item("b5", "Backlog 5", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Select the last backlog item to scroll deep into the BACKLOGGED group.
        // Set the recenter flag so the render simulates keyboard navigation
        // (the new decoupled viewport does not auto-scroll).
        if let Some(pos) = app.display_list.iter().rposition(is_selectable) {
            app.selected_item = Some(pos);
            app.recenter_viewport_on_selection.set(true);
        }
        // Short viewport so the BACKLOGGED header scrolls off -> sticky.
        insta::assert_snapshot!(render(&mut app, 80, 12));
    }

    /// Regression: the sticky group header must NEVER paint over the first
    /// wrapped line of the topmost visible (and in particular the selected)
    /// work item. Before the structural-slot fix the sticky `Paragraph`
    /// overlay overwrote the first row of the list body, hiding the title
    /// of the selected item when it was the topmost visible entry and its
    /// group header had scrolled above the viewport.
    ///
    /// This test uses a text-based assertion (not a snapshot) so small
    /// unrelated layout changes do not require re-blessing the expectation.
    /// It picks a title with a unique wrap-friendly substring and asserts
    /// that:
    ///   1. the sticky header is still displayed (the fix did not disable it),
    ///   2. the selected item's first line is still present in the rendered
    ///      output (the fix did not merely hide the sticky).

    #[test]
    fn sticky_header_does_not_overlap_selected_item() {
        // Two groups. The first BACKLOGGED item gets a distinctive title
        // chosen to mirror the user's screenshot - when it is selected and
        // the ACTIVE group has scrolled above the viewport, the buggy
        // overlay would paint "BACKLOGGED (repo)" over "show cwd in...".
        let items = vec![
            make_work_item("a1", "Active one", WorkItemStatus::Implementing, None, 1),
            make_work_item("a2", "Active two", WorkItemStatus::Implementing, None, 1),
            make_work_item("a3", "Active three", WorkItemStatus::Implementing, None, 1),
            make_work_item("a4", "Active four", WorkItemStatus::Implementing, None, 1),
            make_work_item(
                "b1",
                "show cwd in status bar for workitems",
                WorkItemStatus::Backlog,
                None,
                1,
            ),
            make_work_item("b2", "Backlog other", WorkItemStatus::Backlog, None, 1),
        ];
        let mut app = app_with_items(items, vec![]);
        // Select the first BACKLOGGED item specifically. With a short
        // viewport this forces the list to scroll so that the BACKLOGGED
        // group header sits at the top of the body and the ACTIVE group
        // header is above the viewport - the exact scenario where the old
        // overlay clobbered the selected item's first wrapped line.
        let target = app
            .display_list
            .iter()
            .position(|e| matches!(e, DisplayEntry::WorkItemEntry(idx) if *idx == 4))
            .expect("target BACKLOGGED item must be in display list");
        app.selected_item = Some(target);
        // Simulate the keyboard navigation that would have set this
        // selection in production - the new decoupled viewport only
        // scrolls to the selection when this flag is set, wheel
        // scrolls deliberately leave it alone.
        app.recenter_viewport_on_selection.set(true);

        let rendered = render(&mut app, 40, 12);

        // The sticky (or real) BACKLOGGED header must still be shown.
        assert!(
            rendered.contains("BACKLOGGED"),
            "BACKLOGGED header must still render after the fix:\n{rendered}"
        );
        // The distinctive first-line substring of the selected item must
        // be present in the output. Before the fix it was painted over.
        assert!(
            rendered.contains("show cwd"),
            "selected item's first wrapped line must be visible, \
             not overlapped by the sticky header:\n{rendered}"
        );
    }

