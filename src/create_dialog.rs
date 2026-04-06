use std::path::PathBuf;

/// Which field has keyboard focus inside the create dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CreateDialogFocus {
    Title,
    Repos,
    Branch,
}

/// A simple inline text input with cursor, insert, delete, and navigation.
///
/// rat-widget's TextInputState uses Rc internally (not Send-safe) and is
/// heavier than we need. This struct covers the basics: character insert,
/// backspace, delete, home, end, left, right.
#[derive(Clone, Debug, Default)]
pub struct SimpleTextInput {
    /// The current text content.
    pub text: String,
    /// Byte offset of the cursor within `text`.
    cursor: usize,
}

impl SimpleTextInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the text content and move the cursor to the end.
    /// Used by tests and dialog initialization.
    #[allow(dead_code)]
    pub fn set_text(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
    }

    /// Get the current text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Get the cursor position as a character offset (for rendering).
    pub fn cursor_char_pos(&self) -> usize {
        self.text[..self.cursor].chars().count()
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete the character before the cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Find the start of the previous character.
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.remove(prev);
        self.cursor = prev;
    }

    /// Delete the character at the cursor position.
    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.text.remove(self.cursor);
    }

    /// Move cursor one character to the left.
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.cursor = prev;
    }

    /// Move cursor one character to the right.
    pub fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.text.len());
        self.cursor = next;
    }

    /// Move cursor to the start of the text.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move cursor to the end of the text.
    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Clear all text and reset cursor.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

/// State for the work item creation modal dialog.
#[derive(Clone, Debug)]
pub struct CreateDialog {
    /// Whether the dialog is currently visible.
    pub visible: bool,
    /// Text input for the work item title.
    pub title_input: SimpleTextInput,
    /// Text input for the optional branch name.
    pub branch_input: SimpleTextInput,
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
}

impl Default for CreateDialog {
    fn default() -> Self {
        Self {
            visible: false,
            title_input: SimpleTextInput::new(),
            branch_input: SimpleTextInput::new(),
            repo_list: Vec::new(),
            repo_cursor: 0,
            focus_field: CreateDialogFocus::Title,
            error_message: None,
            branch_user_edited: false,
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
        self.branch_input.clear();
        self.error_message = None;
        self.branch_user_edited = false;
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

    /// Close the dialog without creating anything.
    pub fn close(&mut self) {
        self.visible = false;
    }

    /// Cycle focus to the next field (Title -> Repos -> Branch -> Title).
    pub fn focus_next(&mut self) {
        self.focus_field = match self.focus_field {
            CreateDialogFocus::Title => CreateDialogFocus::Repos,
            CreateDialogFocus::Repos => CreateDialogFocus::Branch,
            CreateDialogFocus::Branch => CreateDialogFocus::Title,
        };
    }

    /// Cycle focus to the previous field (Title -> Branch -> Repos -> Title).
    pub fn focus_prev(&mut self) {
        self.focus_field = match self.focus_field {
            CreateDialogFocus::Title => CreateDialogFocus::Branch,
            CreateDialogFocus::Branch => CreateDialogFocus::Repos,
            CreateDialogFocus::Repos => CreateDialogFocus::Title,
        };
    }

    /// Toggle selection of the repo at the current cursor position.
    pub fn toggle_repo(&mut self) {
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

    /// Validate the dialog fields. Returns Ok with (title, repos, branch)
    /// or Err with an error message.
    pub fn validate(&self) -> Result<(String, Vec<PathBuf>, Option<String>), String> {
        let title = self.title_input.text().trim().to_string();
        if title.is_empty() {
            return Err("Title cannot be empty".to_string());
        }

        let selected_repos: Vec<PathBuf> = self
            .repo_list
            .iter()
            .filter(|(_, selected)| *selected)
            .map(|(path, _)| path.clone())
            .collect();

        if selected_repos.is_empty() {
            return Err("Select at least one repo".to_string());
        }

        let branch = {
            let b = self.branch_input.text().trim().to_string();
            if b.is_empty() { None } else { Some(b) }
        };

        Ok((title, selected_repos, branch))
    }

    /// Get the currently focused text input mutably.
    pub fn focused_input_mut(&mut self) -> Option<&mut SimpleTextInput> {
        match self.focus_field {
            CreateDialogFocus::Title => Some(&mut self.title_input),
            CreateDialogFocus::Branch => Some(&mut self.branch_input),
            CreateDialogFocus::Repos => None,
        }
    }

    /// Auto-fill the branch field from the title, unless the user has manually
    /// edited the branch. Format: {username}/{slugified-title}.
    pub fn auto_fill_branch(&mut self) {
        if self.branch_user_edited {
            return;
        }
        let slug = slugify(self.title_input.text());
        if slug.is_empty() {
            return;
        }
        let username = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
        self.branch_input.set_text(&format!("{username}/{slug}"));
    }
}

/// Convert a title into a git-branch-safe slug.
///
/// Lowercases, replaces whitespace/hyphens/underscores with a single hyphen,
/// strips non-ASCII-alphanumeric characters, collapses runs of hyphens, and
/// trims leading/trailing hyphens.
fn slugify(title: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_text_input_basic_operations() {
        let mut input = SimpleTextInput::new();
        assert_eq!(input.text(), "");
        assert_eq!(input.cursor_char_pos(), 0);

        input.insert_char('h');
        input.insert_char('e');
        input.insert_char('l');
        input.insert_char('l');
        input.insert_char('o');
        assert_eq!(input.text(), "hello");
        assert_eq!(input.cursor_char_pos(), 5);

        input.backspace();
        assert_eq!(input.text(), "hell");

        input.home();
        assert_eq!(input.cursor_char_pos(), 0);

        input.delete();
        assert_eq!(input.text(), "ell");

        input.end();
        assert_eq!(input.cursor_char_pos(), 3);

        input.move_left();
        assert_eq!(input.cursor_char_pos(), 2);

        input.move_right();
        assert_eq!(input.cursor_char_pos(), 3);
    }

    #[test]
    fn simple_text_input_set_text() {
        let mut input = SimpleTextInput::new();
        input.set_text("preset value");
        assert_eq!(input.text(), "preset value");
        assert_eq!(input.cursor_char_pos(), 12);
    }

    #[test]
    fn simple_text_input_boundary_cases() {
        let mut input = SimpleTextInput::new();

        // Backspace on empty does nothing.
        input.backspace();
        assert_eq!(input.text(), "");

        // Delete on empty does nothing.
        input.delete();
        assert_eq!(input.text(), "");

        // Move left at start does nothing.
        input.move_left();
        assert_eq!(input.cursor_char_pos(), 0);

        // Move right at end does nothing.
        input.move_right();
        assert_eq!(input.cursor_char_pos(), 0);
    }

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
        let (title, selected_repos, branch) = result.unwrap();
        assert_eq!(title, "My feature");
        assert_eq!(selected_repos, vec![PathBuf::from("/repo/a")]);
        assert_eq!(branch, Some("feature/my-branch".to_string()));
    }

    #[test]
    fn dialog_optional_branch_omitted() {
        let repos = vec![PathBuf::from("/repo/a")];
        let mut dialog = CreateDialog::new();
        dialog.open(&repos, Some(&PathBuf::from("/repo/a")));
        dialog.title_input.set_text("Item without branch");
        // branch_input left empty

        let (_, _, branch) = dialog.validate().unwrap();
        assert!(branch.is_none());
    }

    #[test]
    fn dialog_focus_cycling() {
        let mut dialog = CreateDialog::new();
        assert_eq!(dialog.focus_field, CreateDialogFocus::Title);

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

    // -- auto_fill_branch tests --

    #[test]
    fn auto_fill_branch_from_title() {
        let mut dialog = CreateDialog::new();
        dialog.open(&[PathBuf::from("/repo/a")], None);
        dialog.title_input.set_text("Fix Login Bug");
        dialog.auto_fill_branch();

        let branch = dialog.branch_input.text().to_string();
        // Should be {username}/fix-login-bug
        assert!(branch.ends_with("/fix-login-bug"), "got: {branch}");
        assert!(branch.contains('/'), "got: {branch}");
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
}
