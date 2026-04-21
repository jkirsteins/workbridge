    #[test]
    fn settings_overlay_with_config() {
        use crate::config::Config;

        // Require a minimum prefix length (see Pass 1 below) so the
        // regex does not accidentally match `/v` or `/t` against
        // unrelated paths; a tmp path always has the form
        // `/<root>/<randomized dir>` which is well beyond `MIN_PREFIX`.
        const MIN_PREFIX: usize = 6;

        // Use `tempfile::tempdir()` so the test stays inside the
        // process temp root, auto-cleans on drop, and cannot collide
        // with sibling test threads. The rendered base-dir path is
        // volatile across machines (on macOS the root resolves to
        // `/var/folders/...`), so after rendering we normalize the
        // output string: redact the tempdir prefix to `<TMPDIR>` and
        // collapse the variable-width trailing padding that ratatui
        // leaves between the redacted content and the column border.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("discovered-a/.git")).unwrap();
        std::fs::create_dir_all(tmp.path().join("discovered-b/.git")).unwrap();

        let base_str = tmp.path().display().to_string();
        let discovered_a = tmp.path().join("discovered-a").display().to_string();

        let config = Config {
            base_dirs: vec![base_str],
            // Use an absolute path instead of ~ to avoid tilde expansion
            // which produces different paths on different machines.
            repos: vec!["/root/Forks/special-repo".into()],
            included_repos: vec![discovered_a],
            ..Config::for_test()
        };
        let mut app = App::with_config(config, Arc::new(StubBackend));
        app.show_settings = true;
        let raw_output = render(&mut app, 80, 24);

        // The tempdir root differs per platform (`/var/folders/...`
        // on macOS, `/tmp/.tmp...` on Linux) and per invocation. The
        // snapshot asserts on the rendered bytes of the Settings
        // overlay, which includes the configured `base_dirs[0]` and
        // the two discovered sub-repos, so without normalization the
        // snapshot diverges per machine. Normalize the rendered
        // string BEFORE handing it to insta so the snapshot stores a
        // canonical form.
        //
        // Pass 1: replace any tempdir-rooted path with `<TMPDIR>`.
        // The prefix is DERIVED from `tmp.path()` rather than
        // hard-coded to `/var/folders/.../tmp/.tmp...`, so the
        // redaction is host-agnostic: a CI or dev environment with
        // `TMPDIR=$HOME/scratch` (or any other exotic temp root)
        // normalizes identically.
        //
        // The rendered Settings overlay truncates long paths to fit
        // the panel width, so the verbatim `tmp.path()` string does
        // NOT always appear in the output - the UI lops off the
        // tail at an arbitrary character boundary. To match both
        // the full path and every truncated form, we construct the
        // regex as the first path component (`/var` or `/tmp` or
        // `/private` etc.) followed by the literal-escaped tail as
        // a sequence of individually-optional characters, each one
        // a greedy `?`-suffixed group. Any prefix-truncation of
        // `tmp.path()` that appears in the rendered output is
        // matched exactly once, starting at the tmp-root, and
        // replaced with `<TMPDIR>`. `regex::escape` on each
        // character guards against path metacharacters.
        // Pass 2: canonicalize runs of spaces between `<TMPDIR>` (or
        // a truncated prefix of it) and the next `│` border so the
        // variable-width trailing padding left behind by the
        // substitution is squashed to a single space. Both the
        // overlay-internal `│` borders and the outer-right `│` border
        // are normalized this way.
        let tmp_str = tmp.path().display().to_string();
        // Build a regex that matches any non-empty prefix of
        // `tmp_str` of length >= `MIN_PREFIX` chars. Each character
        // after the minimum is an optional group, so the regex matches
        // the longest prefix available at every site.
        let chars: Vec<char> = tmp_str.chars().collect();
        let (required, optional) = if chars.len() <= MIN_PREFIX {
            (chars.as_slice(), &[] as &[char])
        } else {
            chars.split_at(MIN_PREFIX)
        };
        let mut pattern = String::new();
        for c in required {
            pattern.push_str(&regex::escape(&c.to_string()));
        }
        for c in optional {
            use std::fmt::Write as _;
            let _ = write!(pattern, "(?:{})?", regex::escape(&c.to_string()));
        }
        // After the tmp-path prefix, greedily eat any path-continuation
        // characters (slash + anything that is not whitespace or the
        // column-border `│`). On Linux the tmp prefix is short (e.g.
        // `/tmp/.tmpAbCdEf`, 14 chars) so the rendered cell has room
        // for the suffix `/discovered-a` after the tmp root - without
        // this tail the regex would redact only the prefix and leave
        // `<TMPDIR>/disco` in the snapshot. On macOS the tmp prefix
        // (`/var/folders/...`, 40+ chars) already fills the cell so
        // there is no suffix to match and the optional group is a
        // no-op. The group is optional so a rendered path that was
        // truncated mid-prefix still matches.
        pattern.push_str(r"(?:/[^\s│]*)?");
        let path_re = regex::Regex::new(&pattern).expect("valid regex");
        let redacted = path_re.replace_all(&raw_output, "<TMPDIR>").into_owned();
        // Collapse 2+ spaces preceding a `│` on any line that contains
        // `<TMPDIR>` so trailing padding is stable. Use multi-line
        // mode so each line is considered independently.
        let pad_re = regex::Regex::new(r"(?m)^(.*<TMPDIR>.*?) +(│)").expect("valid regex");
        let mut normalized = redacted;
        // Apply repeatedly until the pattern stops matching (handles
        // multiple `│`-bounded padding runs on the same line).
        loop {
            let next = pad_re.replace_all(&normalized, "$1 $2").into_owned();
            if next == normalized {
                break;
            }
            normalized = next;
        }

        insta::assert_snapshot!(normalized);
    }

    // -- Work item display tests --


    #[test]
    fn work_item_list_grouped() {
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Backlog,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
            make_work_item(
                "todo-2",
                "Add user settings page",
                WorkItemStatus::Backlog,
                None,
                1,
            ),
            make_work_item(
                "prog-1",
                "Refactor backend API",
                WorkItemStatus::Implementing,
                Some(make_pr_info(88, CheckStatus::Failing)),
                2,
            ),
            make_work_item(
                "prog-2",
                "Update dependencies",
                WorkItemStatus::Implementing,
                Some(make_pr_info(12, CheckStatus::Pending)),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn work_item_list_with_unlinked() {
        let items = vec![make_work_item(
            "prog-1",
            "Active feature",
            WorkItemStatus::Implementing,
            Some(make_pr_info(30, CheckStatus::Passing)),
            1,
        )];
        let unlinked = vec![
            make_unlinked_pr("fix-typo", 45, false),
            make_unlinked_pr("update-deps", 12, true),
        ];
        let mut app = app_with_items(items, unlinked);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn work_item_list_empty_groups() {
        let mut app = app_with_items(vec![], vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn work_item_list_with_done_group() {
        let items = vec![
            make_work_item(
                "todo-1",
                "Fix authentication bug",
                WorkItemStatus::Backlog,
                Some(make_pr_info(14, CheckStatus::Passing)),
                1,
            ),
            make_work_item(
                "prog-1",
                "Refactor backend API",
                WorkItemStatus::Implementing,
                Some(make_pr_info(88, CheckStatus::Failing)),
                1,
            ),
            make_work_item(
                "done-1",
                "Update dependencies",
                WorkItemStatus::Done,
                Some(PrInfo {
                    number: 50,
                    title: "Update deps".to_string(),
                    state: PrState::Merged,
                    is_draft: false,
                    review_decision: ReviewDecision::None,
                    checks: CheckStatus::Passing,
                    mergeable: MergeableState::Unknown,
                    url: "https://github.com/o/r/pull/50".to_string(),
                }),
                1,
            ),
        ];
        let mut app = app_with_items(items, vec![]);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    /// Test helper: mark the given work item id as currently at a
    /// review gate by inserting a minimal `ReviewGateState` into
    /// `app.review_gates`. Starts a status-bar activity so the
    /// production `drop_review_gate` invariant (every drop site ends
    /// the activity) stays exercisable.
    ///
    /// The receiver is a dead-end `unbounded()` channel: we never poll
    /// the gate in this test, so no messages ever need to flow.
    fn mark_at_review_gate(app: &mut App, wi_id: &WorkItemId) {
        let (_tx, rx) = crossbeam_channel::unbounded();
        let activity = app.activities.start("test review gate");
        app.review_gates.insert(
            wi_id.clone(),
            ReviewGateState {
                rx,
                progress: None,
                origin: ReviewGateOrigin::Tui,
                activity,
            },
        );
    }


    #[test]
    fn work_item_list_review_gate() {
        // Baseline: plain `[IM]` item (no gate) to confirm adjacent rows
        // are unaffected.
        let plain = make_work_item(
            "plain-im",
            "Plain implementing item",
            WorkItemStatus::Implementing,
            None,
            1,
        );
        // `[IM]` item sitting at a review gate -> `[IM][RG]`.
        let gated_im = make_work_item(
            "gated-im",
            "Implementing at review gate",
            WorkItemStatus::Implementing,
            None,
            1,
        );
        // `[BK]` item sitting at a review gate -> `[BK][RG]`. The gate
        // can still be active when a work item retreats from
        // Implementing to Blocked (see `docs/work-items.md`).
        let gated_bk = make_work_item(
            "gated-bk",
            "Blocked at review gate",
            WorkItemStatus::Blocked,
            None,
            1,
        );
        // Review-request kind at a gate -> `[RR][IM][RG]`, confirming
        // the [RG] badge composes correctly with the [RR] kind badge.
        let mut gated_rr = make_work_item(
            "gated-rr",
            "Review request at gate",
            WorkItemStatus::Implementing,
            None,
            1,
        );
        gated_rr.kind = crate::work_item::WorkItemKind::ReviewRequest;

        let gated_im_id = gated_im.id.clone();
        let gated_bk_id = gated_bk.id.clone();
        let gated_rr_id = gated_rr.id.clone();

        let items = vec![plain, gated_im, gated_bk, gated_rr];
        let mut app = app_with_items(items, vec![]);
        mark_at_review_gate(&mut app, &gated_im_id);
        mark_at_review_gate(&mut app, &gated_bk_id);
        mark_at_review_gate(&mut app, &gated_rr_id);
        // Rebuild the display list after mutating review-gate state in
        // case grouping/ordering depends on it. (It doesn't today, but
        // keeping this call defensive matches how `app_with_items`
        // primes the list.)
        app.build_display_list();
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn work_item_with_errors_no_session() {
        let items = vec![WorkItem {
            display_id: None,
            id: WorkItemId::LocalFile(PathBuf::from("/data/err.json")),
            backend_type: BackendType::LocalFile,
            kind: crate::work_item::WorkItemKind::Own,
            title: "Broken work item".to_string(),
            description: None,
            status: WorkItemStatus::Implementing,
            status_derived: false,
            repo_associations: vec![RepoAssociation {
                repo_path: PathBuf::from("/repo/alpha"),
                branch: Some("42-fix-bug".to_string()),
                worktree_path: None,
                pr: None,
                issue: None,
                git_state: None,
                stale_worktree_path: None,
            }],
            errors: vec![
                WorkItemError::MultiplePrsForBranch {
                    repo_path: PathBuf::from("/repo/alpha"),
                    branch: "42-fix-bug".to_string(),
                    count: 2,
                },
                WorkItemError::IssueNotFound {
                    repo_path: PathBuf::from("/repo/alpha"),
                    issue_number: 42,
                },
            ],
        }];
        let mut app = app_with_items(items, vec![]);
        // Select the first selectable work item entry (skipping group headers).
        app.selected_item = app.display_list.iter().position(is_selectable);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn create_dialog_default_view() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![
            PathBuf::from("/Volumes/X10/Projects/workbridge"),
            PathBuf::from("/Volumes/X10/Projects/other-repo"),
        ];
        app.create_dialog.open(
            &repos,
            Some(&PathBuf::from("/Volumes/X10/Projects/workbridge")),
        );
        assert!(app.create_dialog.visible);
        assert_eq!(app.create_dialog.focus_field, CreateDialogFocus::Title);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn create_dialog_with_input_and_repos_focused() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![
            PathBuf::from("/repo/alpha"),
            PathBuf::from("/repo/beta"),
            PathBuf::from("/repo/gamma"),
        ];
        app.create_dialog
            .open(&repos, Some(&PathBuf::from("/repo/beta")));
        // Type a title
        app.create_dialog.title_input.set_text("My feature");
        // Focus on repos
        app.create_dialog.focus_field = CreateDialogFocus::Repos;
        app.create_dialog.repo_cursor = 1; // beta is selected
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }


    #[test]
    fn create_dialog_with_error() {
        let mut app = App::new();
        let repos = vec![PathBuf::from("/repo/only")];
        app.create_dialog.open(&repos, None);
        app.create_dialog.error_message = Some("Title cannot be empty".to_string());
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    /// Ctrl+N with multiple managed repos opens a compact quick-start
    /// dialog containing only the repo list. The render must:
    /// - show the "Quick start - select repo" title and "Repos:" label
    /// - NOT render any of the Title/Description/Branch fields that the
    ///   full Ctrl+B dialog uses
    /// - show the quickstart-specific hint (Up/Down/Space, no Tab)
    ///
    /// Rendered at 120 columns so the full hint line fits inside the
    /// dialog's 60%-of-width box (the 80-col default would truncate it
    /// like `create_dialog_default_view` already does).

    #[test]
    fn create_dialog_quickstart_view() {
        use crate::create_dialog::CreateDialogFocus;

        let mut app = App::new();
        let repos = vec![PathBuf::from("/repo/alpha"), PathBuf::from("/repo/beta")];
        app.create_dialog.open_quickstart(&repos);
        assert!(app.create_dialog.visible);
        assert!(app.create_dialog.quickstart_mode);
        assert_eq!(app.create_dialog.focus_field, CreateDialogFocus::Repos);

        let rendered = render(&mut app, 120, 24);

        assert!(
            rendered.contains("Quick start - select repo"),
            "expected dialog title 'Quick start - select repo':\n{rendered}"
        );
        assert!(
            rendered.contains("Repos:"),
            "expected 'Repos:' label:\n{rendered}"
        );
        assert!(
            rendered.contains("/repo/alpha"),
            "expected first repo path to be listed:\n{rendered}"
        );
        assert!(
            rendered.contains("/repo/beta"),
            "expected second repo path to be listed:\n{rendered}"
        );
        assert!(
            !rendered.contains("Title:"),
            "Title: field must not be rendered in quick-start mode:\n{rendered}"
        );
        assert!(
            !rendered.contains("Description (optional)"),
            "Description field label must not be rendered in quick-start mode:\n{rendered}"
        );
        assert!(
            !rendered.contains("Branch (optional)"),
            "Branch field label must not be rendered in quick-start mode:\n{rendered}"
        );
        assert!(
            rendered.contains("Up/Down: Move"),
            "expected quickstart-specific hint 'Up/Down: Move':\n{rendered}"
        );
        assert!(
            rendered.contains("Space: Select repo"),
            "expected quickstart-specific hint 'Space: Select repo':\n{rendered}"
        );
        assert!(
            !rendered.contains("Tab: Next field"),
            "hint must not mention 'Tab: Next field' in quick-start mode:\n{rendered}"
        );
    }
