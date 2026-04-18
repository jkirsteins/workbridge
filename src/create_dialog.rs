use std::path::PathBuf;

use rat_widget::text_input::TextInputState;
use rat_widget::textarea::TextAreaState;

use crate::work_item::{WorkItemId, WorkItemStatus};

/// Which field has keyboard focus inside the create dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CreateDialogFocus {
    Title,
    Description,
    Repos,
    Branch,
}

/// State for the work item creation modal dialog.
#[derive(Clone, Debug)]
pub struct CreateDialog {
    /// Whether the dialog is currently visible.
    pub visible: bool,
    /// Text input for the work item title.
    pub title_input: TextInputState,
    /// Multi-line text area for the optional description.
    pub description_input: TextAreaState,
    /// Text input for the optional branch name.
    pub branch_input: TextInputState,
    /// List of repos with selection state: (repo_path, selected).
    pub repo_list: Vec<(PathBuf, bool)>,
    /// Cursor position in the repo list.
    pub repo_cursor: usize,
    /// Which field currently has keyboard focus.
    pub focus_field: CreateDialogFocus,
    /// Validation error message (shown inline, cleared on next input).
    pub error_message: Option<String>,
    /// Whether the user has manually edited the branch field.
    /// When true, auto_fill_branch() will not overwrite their input.
    pub branch_user_edited: bool,
    /// When true, submitting the dialog creates a Planning item and spawns
    /// a Claude session immediately (quick-start mode) instead of creating
    /// a Backlog item.
    pub quickstart_mode: bool,
}

impl Default for CreateDialog {
    fn default() -> Self {
        Self {
            visible: false,
            title_input: TextInputState::new(),
            description_input: TextAreaState::new(),
            branch_input: TextInputState::new(),
            repo_list: Vec::new(),
            repo_cursor: 0,
            focus_field: CreateDialogFocus::Title,
            error_message: None,
            branch_user_edited: false,
            quickstart_mode: false,
        }
    }
}

impl CreateDialog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the dialog, populating the repo list from active repos.
    /// Pre-selects the repo that contains `cwd`, if any.
    pub fn open(&mut self, active_repos: &[PathBuf], cwd_repo: Option<&PathBuf>) {
        self.visible = true;
        self.title_input.clear();
        // `TextAreaState::clear` only clears the rope; it does NOT reset
        // the scrollbar offset, sub-row offset, or the `scroll_to_cursor`
        // latch (see rat-text-3.1.0/src/text_area.rs: `clear` vs
        // `set_text`). If the user had scrolled the description on a
        // previous open, that stale offset would carry into the next
        // open and hide the first row of typed characters. Follow
        // `clear()` with `set_text("")` to force a full scroll-state
        // reset - the same reset `set_text` uses for normal assignment.
        self.description_input.clear();
        self.description_input.set_text("");
        self.branch_input.clear();
        self.error_message = None;
        self.branch_user_edited = false;
        self.quickstart_mode = false;
        self.focus_field = CreateDialogFocus::Title;

        self.repo_list = active_repos
            .iter()
            .map(|path| {
                let selected = cwd_repo.is_some_and(|cwd| cwd == path);
                (path.clone(), selected)
            })
            .collect();

        // If no CWD repo matched but there is exactly one repo, select it.
        if !self.repo_list.iter().any(|(_, s)| *s) && self.repo_list.len() == 1 {
            self.repo_list[0].1 = true;
        }

        self.repo_cursor = 0;
    }

    /// Open in quick-start mode: the user only selects a repo. On submit,
    /// a Planning item is created and a Claude session spawns immediately.
    ///
    /// Focus is parked on the repo list (the only meaningful field) so the
    /// user does not have to Tab past the now-hidden Title/Description/Branch
    /// fields. The render path keys off `quickstart_mode` to draw a compact
    /// dialog with only the repo list.
    pub fn open_quickstart(&mut self, active_repos: &[PathBuf]) {
        self.open(active_repos, None);
        self.quickstart_mode = true;
        self.focus_field = CreateDialogFocus::Repos;
    }

    /// Close the dialog without creating anything.
    pub fn close(&mut self) {
        self.visible = false;
    }

    /// Cycle focus to the next field (Title -> Description -> Repos -> Branch -> Title).
    ///
    /// In quick-start mode the only visible field is the repo list, so this
    /// is a no-op: Tab/BackTab must not be able to sneak focus onto an
    /// invisible Title/Description/Branch field.
    pub fn focus_next(&mut self) {
        if self.quickstart_mode {
            return;
        }
        self.focus_field = match self.focus_field {
            CreateDialogFocus::Title => CreateDialogFocus::Description,
            CreateDialogFocus::Description => CreateDialogFocus::Repos,
            CreateDialogFocus::Repos => CreateDialogFocus::Branch,
            CreateDialogFocus::Branch => CreateDialogFocus::Title,
        };
    }

    /// Cycle focus to the previous field (Title -> Branch -> Repos -> Description -> Title).
    ///
    /// In quick-start mode this is a no-op for the same reason as
    /// [`focus_next`].
    pub fn focus_prev(&mut self) {
        if self.quickstart_mode {
            return;
        }
        self.focus_field = match self.focus_field {
            CreateDialogFocus::Title => CreateDialogFocus::Branch,
            CreateDialogFocus::Branch => CreateDialogFocus::Repos,
            CreateDialogFocus::Repos => CreateDialogFocus::Description,
            CreateDialogFocus::Description => CreateDialogFocus::Title,
        };
    }

    /// Toggle selection of the repo at the current cursor position.
    ///
    /// In quick-start mode this enforces single-select: any other selected
    /// row is cleared first, so at most one repo is ever marked `[x]`. The
    /// quickstart submission path in `event.rs` only consumes
    /// `selected[0]` and would silently discard any additional checked
    /// rows, so the dialog must never let the user check more than one
    /// repo at a time. In normal (Ctrl+B) mode this stays a plain toggle
    /// because the full create dialog supports multi-repo work items.
    pub fn toggle_repo(&mut self) {
        if self.quickstart_mode {
            let Some(was_selected) = self.repo_list.get(self.repo_cursor).map(|(_, s)| *s) else {
                return;
            };
            for (_, sel) in self.repo_list.iter_mut() {
                *sel = false;
            }
            if let Some(entry) = self.repo_list.get_mut(self.repo_cursor) {
                entry.1 = !was_selected;
            }
            return;
        }
        if let Some(entry) = self.repo_list.get_mut(self.repo_cursor) {
            entry.1 = !entry.1;
        }
    }

    /// Move the repo cursor up.
    pub fn repo_up(&mut self) {
        self.repo_cursor = self.repo_cursor.saturating_sub(1);
    }

    /// Move the repo cursor down.
    pub fn repo_down(&mut self) {
        if self.repo_cursor + 1 < self.repo_list.len() {
            self.repo_cursor += 1;
        }
    }

    /// Validate the dialog fields. Returns Ok with (title, description, repos, branch)
    /// or Err with an error message. Description is None when empty.
    pub fn validate(&self) -> Result<(String, Option<String>, Vec<PathBuf>, String), String> {
        let title = self.title_input.text().trim().to_string();
        if title.is_empty() {
            return Err("Title cannot be empty".to_string());
        }

        let desc_text = self.description_input.text();
        let desc_trimmed = desc_text.trim();
        let description = if desc_trimmed.is_empty() {
            None
        } else {
            Some(desc_trimmed.to_string())
        };

        let selected_repos: Vec<PathBuf> = self
            .repo_list
            .iter()
            .filter(|(_, selected)| *selected)
            .map(|(path, _)| path.clone())
            .collect();

        if selected_repos.is_empty() {
            return Err("Select at least one repo".to_string());
        }

        let branch = self.branch_input.text().trim().to_string();
        if branch.is_empty() {
            return Err("Branch name is required".to_string());
        }

        Ok((title, description, selected_repos, branch))
    }

    /// Get the currently focused single-line text input mutably.
    /// Returns None for Description (uses TextAreaState) and Repos.
    pub fn focused_input_mut(&mut self) -> Option<&mut TextInputState> {
        match self.focus_field {
            CreateDialogFocus::Title => Some(&mut self.title_input),
            CreateDialogFocus::Branch => Some(&mut self.branch_input),
            CreateDialogFocus::Description | CreateDialogFocus::Repos => None,
        }
    }

    /// Auto-fill the branch field from the title, unless the user has manually
    /// edited the branch. Format: {username}/{slugified-title}-{suffix}.
    pub fn auto_fill_branch(&mut self) {
        if self.branch_user_edited {
            return;
        }
        let slug = slugify(self.title_input.text());
        if slug.is_empty() {
            return;
        }
        let slug = truncate_slug(&slug, MAX_SLUG_LEN);
        let suffix = random_suffix();
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        self.branch_input
            .set_text(format!("{username}/{slug}-{suffix}"));
    }
}

/// Maximum length of the slugified portion of a branch name.
pub(crate) const MAX_SLUG_LEN: usize = 50;

/// Convert a title into a git-branch-safe slug.
///
/// Lowercases, replaces whitespace/hyphens/underscores with a single hyphen,
/// strips non-ASCII-alphanumeric characters, collapses runs of hyphens, and
/// trims leading/trailing hyphens.
pub(crate) fn slugify(title: &str) -> String {
    let lower = title.to_lowercase();
    let mut result = String::with_capacity(lower.len());
    let mut prev_hyphen = false;

    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            prev_hyphen = false;
            result.push(c);
        } else if (c.is_whitespace() || c == '-' || c == '_') && !prev_hyphen && !result.is_empty()
        {
            result.push('-');
            prev_hyphen = true;
        }
        // All other characters are silently dropped.
    }

    // Trim trailing hyphen.
    if result.ends_with('-') {
        result.pop();
    }

    result
}

/// Truncate a slug to at most `max_len` bytes, cutting at the last hyphen
/// boundary to avoid mid-word breaks. Falls back to a hard cut when the
/// slug contains no hyphens within the limit.
pub(crate) fn truncate_slug(slug: &str, max_len: usize) -> String {
    if slug.len() <= max_len {
        return slug.to_string();
    }
    // Find last hyphen at or before max_len.
    if let Some(pos) = slug[..max_len].rfind('-') {
        slug[..pos].to_string()
    } else {
        slug[..max_len].to_string()
    }
}

/// Generate a 4-character hex suffix for branch name uniqueness.
pub(crate) fn random_suffix() -> String {
    let bytes = uuid::Uuid::new_v4();
    let b = bytes.as_bytes();
    format!("{:02x}{:02x}", b[0], b[1])
}

/// Which follow-up action should be re-driven after the user confirms a
/// `SetBranchDialog`. The dialog itself only persists the branch name;
/// the caller who triggered it recorded its intent here so
/// `confirm_set_branch_dialog` can complete the original gesture.
#[derive(Clone, Debug)]
pub enum PendingBranchAction {
    /// The user pressed Enter on a branchless Planning/Implementing item,
    /// which should open a Claude session once the branch is set.
    SpawnSession,
    /// The user tried to advance a Backlog item past Planning without a
    /// branch; re-attempt the stage change once the branch is persisted.
    Advance {
        from: WorkItemStatus,
        to: WorkItemStatus,
    },
}

/// State for the "Set branch name" recovery modal. Shown when a work item
/// has reached a stage where a branch is required but its repo
/// associations all have `branch.is_none()`. The dialog reuses
/// `rat_widget::text_input::TextInputState` and prefills a slug generated
/// from the item's title.
#[derive(Clone, Debug)]
pub struct SetBranchDialog {
    /// The work item that needs a branch.
    pub wi_id: WorkItemId,
    /// The branch-name text input, prefilled with a slug default.
    pub input: TextInputState,
    /// What to do after the branch is persisted.
    pub pending: PendingBranchAction,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Dialog tests --

    #[test]
    fn dialog_opens_with_cwd_repo_preselected() {
        let repos = vec![
            PathBuf::from("/repo/a"),
            PathBuf::from("/repo/b"),
            PathBuf::from("/repo/c"),
        ];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, Some(&PathBuf::from("/repo/b")));

        assert!(dialog.visible);
        assert_eq!(dialog.repo_list.len(), 3);
        assert!(!dialog.repo_list[0].1); // a not selected
        assert!(dialog.repo_list[1].1); // b selected (CWD)
        assert!(!dialog.repo_list[2].1); // c not selected
        assert_eq!(dialog.focus_field, CreateDialogFocus::Title);
    }

    #[test]
    fn dialog_single_repo_auto_selected() {
        let repos = vec![PathBuf::from("/repo/only")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, None); // no CWD match

        assert!(dialog.repo_list[0].1); // auto-selected when only one
    }

    #[test]
    fn dialog_closes_on_esc() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);
        assert!(dialog.visible);
        dialog.close();
        assert!(!dialog.visible);
    }

    #[test]
    fn dialog_validates_empty_title() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);
        // title is empty, repo is auto-selected (single repo)
        let result = dialog.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Title cannot be empty");
    }

    #[test]
    fn dialog_validates_no_repos_selected() {
        let repos = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, None);
        dialog.title_input.set_text("My item");
        // No repos selected (no CWD match, more than one repo)
        let result = dialog.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Select at least one repo");
    }

    #[test]
    fn dialog_creates_work_item_on_valid_input() {
        let repos = vec![PathBuf::from("/repo/a")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, Some(&PathBuf::from("/repo/a")));
        dialog.title_input.set_text("My feature");
        dialog.branch_input.set_text("feature/my-branch");

        let result = dialog.validate();
        assert!(result.is_ok());
        let (title, description, selected_repos, branch) = result.unwrap();
        assert_eq!(title, "My feature");
        assert!(description.is_none());
        assert_eq!(selected_repos, vec![PathBuf::from("/repo/a")]);
        assert_eq!(branch, "feature/my-branch");
    }

    #[test]
    fn description_long_text_roundtrips_through_validate() {
        // A long single-line string must not be silently truncated or
        // rejected by validate(). The TextArea state handles wrapping as a
        // rendering concern; the underlying text is preserved.
        let long: String = "x".repeat(250);
        let repos = vec![PathBuf::from("/repo/a")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, Some(&PathBuf::from("/repo/a")));
        dialog.title_input.set_text("My feature");
        dialog.branch_input.set_text("feature/my-branch");
        dialog.description_input.set_text(&long);

        let result = dialog.validate();
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let (_, description, _, _) = result.unwrap();
        assert_eq!(
            description.expect("description should be Some for non-empty text"),
            long,
            "description must round-trip unchanged",
        );
    }

    #[test]
    fn dialog_empty_branch_rejected() {
        let repos = vec![PathBuf::from("/repo/a")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, Some(&PathBuf::from("/repo/a")));
        dialog.title_input.set_text("Item without branch");
        // branch_input left empty

        let result = dialog.validate();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Branch name is required");
    }

    #[test]
    fn dialog_focus_cycling() {
        let mut dialog = CreateDialog::new();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Title);

        dialog.focus_next();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Description);

        dialog.focus_next();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Repos);

        dialog.focus_next();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Branch);

        dialog.focus_next();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Title);

        // Reverse
        dialog.focus_prev();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Branch);

        dialog.focus_prev();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Repos);

        dialog.focus_prev();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Description);
    }

    #[test]
    fn dialog_repo_toggle() {
        let repos = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, None);

        // Initially nothing selected (2 repos, no CWD match)
        assert!(!dialog.repo_list[0].1);
        assert!(!dialog.repo_list[1].1);

        // Toggle first repo
        dialog.toggle_repo();
        assert!(dialog.repo_list[0].1);

        // Move down and toggle second
        dialog.repo_down();
        dialog.toggle_repo();
        assert!(dialog.repo_list[1].1);

        // Toggle first again to deselect
        dialog.repo_up();
        dialog.toggle_repo();
        assert!(!dialog.repo_list[0].1);
    }

    #[test]
    fn dialog_repo_cursor_bounds() {
        let repos = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, None);

        // At start, up does nothing
        dialog.repo_up();
        assert_eq!(dialog.repo_cursor, 0);

        // Move to end
        dialog.repo_down();
        assert_eq!(dialog.repo_cursor, 1);

        // Past end does nothing
        dialog.repo_down();
        assert_eq!(dialog.repo_cursor, 1);
    }

    // -- slugify tests --

    #[test]
    fn slugify_basic_title() {
        assert_eq!(slugify("Fix Login Bug"), "fix-login-bug");
    }

    #[test]
    fn slugify_special_chars_stripped() {
        assert_eq!(slugify("Fix Login Bug!!"), "fix-login-bug");
    }

    #[test]
    fn slugify_collapses_whitespace() {
        assert_eq!(slugify("a   b"), "a-b");
    }

    #[test]
    fn slugify_underscores_and_hyphens() {
        assert_eq!(slugify("my_cool--feature"), "my-cool-feature");
    }

    #[test]
    fn slugify_empty_input() {
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn slugify_all_special_chars() {
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn slugify_leading_trailing_whitespace() {
        assert_eq!(slugify("  hello world  "), "hello-world");
    }

    // -- truncate_slug tests --

    #[test]
    fn truncate_slug_no_truncation_needed() {
        assert_eq!(truncate_slug("fix-login-bug", 50), "fix-login-bug");
    }

    #[test]
    fn truncate_slug_at_word_boundary() {
        let slug = "implement-comprehensive-authentication-system-with-oauth2-and-saml-support";
        let result = truncate_slug(slug, 50);
        assert!(result.len() <= 50, "got len {}: {result}", result.len());
        // Should cut at a hyphen boundary
        assert!(
            !result.ends_with('-'),
            "should not end with hyphen: {result}"
        );
        assert_eq!(result, "implement-comprehensive-authentication-system");
    }

    #[test]
    fn truncate_slug_single_long_word() {
        let slug = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz0123456789";
        let result = truncate_slug(slug, 50);
        assert_eq!(result.len(), 50);
        assert_eq!(result, &slug[..50]);
    }

    #[test]
    fn truncate_slug_exact_boundary() {
        let slug = "a".repeat(50);
        assert_eq!(truncate_slug(&slug, 50), slug);
    }

    // -- random_suffix tests --

    #[test]
    fn random_suffix_is_4_hex_chars() {
        let s = random_suffix();
        assert_eq!(s.len(), 4);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {s}");
    }

    // -- auto_fill_branch tests --

    #[test]
    fn auto_fill_branch_from_title() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);
        dialog.title_input.set_text("Fix Login Bug");
        dialog.auto_fill_branch();

        let branch = dialog.branch_input.text().to_string();
        // Should be {username}/fix-login-bug-{4hex}
        assert!(branch.contains("/fix-login-bug-"), "got: {branch}");
        // Verify 4-char hex suffix at end
        let suffix = &branch[branch.len() - 4..];
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix not hex: {suffix}"
        );
    }

    #[test]
    fn auto_fill_branch_long_title_truncated() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);
        dialog.title_input.set_text(
            "Implement comprehensive authentication system with OAuth2 and SAML support for enterprise",
        );
        dialog.auto_fill_branch();

        let branch = dialog.branch_input.text().to_string();
        // Extract slug portion: after the first '/' and before the last '-xxxx'
        let after_slash = branch.split('/').nth(1).unwrap();
        let slug_end = after_slash.len() - 5; // strip "-xxxx"
        let slug = &after_slash[..slug_end];
        assert!(
            slug.len() <= MAX_SLUG_LEN,
            "slug too long ({} > {MAX_SLUG_LEN}): {slug}",
            slug.len()
        );
    }

    #[test]
    fn auto_fill_branch_respects_user_edited() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);
        dialog.title_input.set_text("Fix Login Bug");
        dialog.branch_input.set_text("my-custom-branch");
        dialog.branch_user_edited = true;

        dialog.auto_fill_branch();
        assert_eq!(dialog.branch_input.text(), "my-custom-branch");
    }

    #[test]
    fn auto_fill_branch_skips_empty_slug() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);
        dialog.title_input.set_text("!!!");

        dialog.auto_fill_branch();
        assert_eq!(dialog.branch_input.text(), "");
    }

    #[test]
    fn open_resets_branch_user_edited() {
        let mut dialog = CreateDialog::new();
        dialog.branch_user_edited = true;
        dialog.open(&[PathBuf::from("/repo/a")], None);
        assert!(!dialog.branch_user_edited);
    }

    /// Defense-in-depth: reopening the dialog must reset the
    /// description textarea's scroll state. `TextAreaState::clear()`
    /// alone only clears the rope; it does NOT reset the vertical
    /// offset, sub-row offset, or `scroll_to_cursor` latch (see the
    /// rat-text source). If the user scrolled the description on a
    /// previous open and then reopened the dialog, a stale offset
    /// would carry forward and hide the first row of newly-typed
    /// characters - the exact symptom the UI-layout fix addresses,
    /// but via a different mechanism.
    ///
    /// `CreateDialog::open` calls `set_text("")` after `clear()` to
    /// force a full scroll-state reset. This test pins that behavior.
    #[test]
    fn open_resets_description_scroll_state() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);

        // Simulate the user having scrolled the description by
        // pushing a non-zero vertical offset. We drive the low-level
        // setters directly (not insert_char) so this test does not
        // depend on the layout or a rendered viewport; the point is
        // to prove that `open` clears scroll state regardless of how
        // it got set.
        dialog
            .description_input
            .set_text("line1\nline2\nline3\nline4");
        let _ = dialog.description_input.set_vertical_offset(2);
        let _ = dialog.description_input.set_sub_row_offset(1);
        assert_eq!(
            dialog.description_input.vertical_offset(),
            2,
            "precondition: vertical offset is non-zero before reopen"
        );

        // Close and reopen. After open, the description must be
        // empty AND the scroll state must be reset to the origin.
        dialog.close();
        dialog.open(&[PathBuf::from("/repo/a")], None);

        assert_eq!(
            dialog.description_input.text(),
            "",
            "description text must be empty after reopen"
        );
        assert_eq!(
            dialog.description_input.vertical_offset(),
            0,
            "vertical scroll offset must be reset to 0 on reopen"
        );
        assert_eq!(
            dialog.description_input.sub_row_offset(),
            0,
            "sub-row scroll offset must be reset to 0 on reopen"
        );
    }

    // -- quick-start mode tests --

    #[test]
    fn open_quickstart_focuses_repos_field() {
        // With more than one repo, the quick-start dialog must land focus
        // directly on the repo list and start with no repo pre-selected, so
        // the user explicitly picks one.
        let repos = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        let mut dialog = CreateDialog::new();
        dialog.open_quickstart(&repos);

        assert!(dialog.visible);
        assert!(dialog.quickstart_mode);
        assert_eq!(dialog.focus_field, CreateDialogFocus::Repos);
        assert_eq!(dialog.repo_list.len(), 2);
        assert!(
            dialog.repo_list.iter().all(|(_, sel)| !*sel),
            "no repo should be pre-selected in quick-start mode"
        );
    }

    #[test]
    fn focus_next_no_op_in_quickstart_mode() {
        // Tab / BackTab must not move focus off the repo list while in
        // quick-start mode - the other fields are not rendered, so any
        // movement would land focus on an invisible field.
        let repos = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        let mut dialog = CreateDialog::new();
        dialog.open_quickstart(&repos);

        dialog.focus_next();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Repos);

        dialog.focus_prev();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Repos);
    }

    #[test]
    fn toggle_repo_is_single_select_in_quickstart_mode() {
        // Quick-start mode must enforce single-select: the event handler
        // only consumes `selected[0]` and would silently drop any extra
        // checked rows. The dialog must never show two `[x]` markers at
        // once, otherwise the user is misled into thinking both repos
        // will be acted on.
        let repos = vec![
            PathBuf::from("/repo/a"),
            PathBuf::from("/repo/b"),
            PathBuf::from("/repo/c"),
        ];
        let mut dialog = CreateDialog::new();
        dialog.open_quickstart(&repos);

        // Select /repo/a.
        dialog.toggle_repo();
        assert!(dialog.repo_list[0].1);
        assert!(!dialog.repo_list[1].1);
        assert!(!dialog.repo_list[2].1);

        // Move to /repo/b and select it: /repo/a must be cleared.
        dialog.repo_down();
        dialog.toggle_repo();
        assert!(!dialog.repo_list[0].1);
        assert!(dialog.repo_list[1].1);
        assert!(!dialog.repo_list[2].1);
        assert_eq!(
            dialog.repo_list.iter().filter(|(_, sel)| *sel).count(),
            1,
            "exactly one repo must be selected at any time in quickstart mode"
        );

        // Move to /repo/c and select it: /repo/b must be cleared.
        dialog.repo_down();
        dialog.toggle_repo();
        assert!(!dialog.repo_list[0].1);
        assert!(!dialog.repo_list[1].1);
        assert!(dialog.repo_list[2].1);

        // Toggling the current row off should leave nothing selected,
        // matching plain checkbox semantics so the user can back out.
        dialog.toggle_repo();
        assert!(
            dialog.repo_list.iter().all(|(_, sel)| !*sel),
            "toggling the selected row off should clear all selections"
        );
    }

    #[test]
    fn toggle_repo_remains_multi_select_in_normal_mode() {
        // Regression guard: the Ctrl+B path must still allow multiple
        // repos to be checked, since the full create dialog supports
        // multi-repo work items via `validate()`.
        let repos = vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/b")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, None);

        dialog.toggle_repo();
        dialog.repo_down();
        dialog.toggle_repo();

        assert!(
            dialog.repo_list[0].1 && dialog.repo_list[1].1,
            "normal-mode toggle_repo must allow multiple repos to be checked"
        );
    }
}
