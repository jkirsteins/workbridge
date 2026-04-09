use std::path::PathBuf;

/// Which field has keyboard focus inside the create dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CreateDialogFocus {
    Title,
    Description,
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

/// A simple multi-line text area with cursor movement and scrolling.
///
/// Stores text as a vector of lines. Supports insert, delete, newlines,
/// and basic cursor movement (arrows, home, end, up, down).
#[derive(Clone, Debug)]
pub struct SimpleTextArea {
    /// Lines of text. Always has at least one entry (possibly empty).
    lines: Vec<String>,
    /// Current cursor row (0-based line index).
    cursor_row: usize,
    /// Byte offset of the cursor within the current line.
    cursor_col: usize,
    /// First visible line index (for vertical scrolling).
    pub scroll_offset: usize,
}

impl Default for SimpleTextArea {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: 0,
        }
    }
}

impl SimpleTextArea {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set text content and move cursor to end.
    #[allow(dead_code)]
    pub fn set_text(&mut self, s: &str) {
        self.lines = s.split('\n').map(|l| l.to_string()).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_row].len();
        self.scroll_offset = 0;
    }

    /// Get the full text content with newlines joining lines.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Get the cursor position as (row, char_col) for rendering.
    pub fn cursor_pos(&self) -> (usize, usize) {
        let char_col = self.lines[self.cursor_row][..self.cursor_col]
            .chars()
            .count();
        (self.cursor_row, char_col)
    }

    /// Get visible lines for rendering, given the visible height.
    pub fn visible_lines(&self, height: usize) -> &[String] {
        let start = self.scroll_offset;
        let end = (start + height).min(self.lines.len());
        &self.lines[start..end]
    }

    /// Ensure the cursor row is visible given the viewport height.
    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.cursor_row < self.scroll_offset {
            self.scroll_offset = self.cursor_row;
        } else if self.cursor_row >= self.scroll_offset + height {
            self.scroll_offset = self.cursor_row - height + 1;
        }
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.lines[self.cursor_row].insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
    }

    /// Insert a newline, splitting the current line at the cursor.
    pub fn insert_newline(&mut self) {
        let rest = self.lines[self.cursor_row][self.cursor_col..].to_string();
        self.lines[self.cursor_row].truncate(self.cursor_col);
        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, rest);
        self.cursor_col = 0;
    }

    /// Delete the character before the cursor (backspace).
    /// At the start of a line, joins with the previous line.
    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let prev = self.lines[self.cursor_row][..self.cursor_col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.lines[self.cursor_row].remove(prev);
            self.cursor_col = prev;
        } else if self.cursor_row > 0 {
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&current_line);
        }
    }

    /// Delete the character at the cursor position.
    /// At the end of a line, joins with the next line.
    pub fn delete(&mut self) {
        let line_len = self.lines[self.cursor_row].len();
        if self.cursor_col < line_len {
            self.lines[self.cursor_row].remove(self.cursor_col);
        } else if self.cursor_row + 1 < self.lines.len() {
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
    }

    /// Move cursor one character to the left. Wraps to end of previous line.
    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            let prev = self.lines[self.cursor_row][..self.cursor_col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor_col = prev;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
        }
    }

    /// Move cursor one character to the right. Wraps to start of next line.
    pub fn move_right(&mut self) {
        let line_len = self.lines[self.cursor_row].len();
        if self.cursor_col < line_len {
            let next = self.lines[self.cursor_row][self.cursor_col..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor_col + i)
                .unwrap_or(line_len);
            self.cursor_col = next;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    /// Move cursor up one line, preserving column position as best as possible.
    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            let char_col = self.lines[self.cursor_row][..self.cursor_col]
                .chars()
                .count();
            self.cursor_row -= 1;
            self.cursor_col = char_to_byte_offset(&self.lines[self.cursor_row], char_col);
        }
    }

    /// Move cursor down one line, preserving column position as best as possible.
    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            let char_col = self.lines[self.cursor_row][..self.cursor_col]
                .chars()
                .count();
            self.cursor_row += 1;
            self.cursor_col = char_to_byte_offset(&self.lines[self.cursor_row], char_col);
        }
    }

    /// Move cursor to the start of the current line.
    pub fn home(&mut self) {
        self.cursor_col = 0;
    }

    /// Move cursor to the end of the current line.
    pub fn end(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].len();
    }

    /// Clear all text and reset cursor.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_offset = 0;
    }
}

/// Convert a character offset to a byte offset within a string, clamping
/// to the string length if the char offset exceeds available characters.
fn char_to_byte_offset(s: &str, char_offset: usize) -> usize {
    s.char_indices()
        .nth(char_offset)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

/// State for the work item creation modal dialog.
#[derive(Clone, Debug)]
pub struct CreateDialog {
    /// Whether the dialog is currently visible.
    pub visible: bool,
    /// Text input for the work item title.
    pub title_input: SimpleTextInput,
    /// Multi-line text area for the optional description.
    pub description_input: SimpleTextArea,
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
            description_input: SimpleTextArea::new(),
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
        self.description_input.clear();
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

    /// Cycle focus to the next field (Title -> Description -> Repos -> Branch -> Title).
    pub fn focus_next(&mut self) {
        self.focus_field = match self.focus_field {
            CreateDialogFocus::Title => CreateDialogFocus::Description,
            CreateDialogFocus::Description => CreateDialogFocus::Repos,
            CreateDialogFocus::Repos => CreateDialogFocus::Branch,
            CreateDialogFocus::Branch => CreateDialogFocus::Title,
        };
    }

    /// Cycle focus to the previous field (Title -> Branch -> Repos -> Description -> Title).
    pub fn focus_prev(&mut self) {
        self.focus_field = match self.focus_field {
            CreateDialogFocus::Title => CreateDialogFocus::Branch,
            CreateDialogFocus::Branch => CreateDialogFocus::Repos,
            CreateDialogFocus::Repos => CreateDialogFocus::Description,
            CreateDialogFocus::Description => CreateDialogFocus::Title,
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
    /// Returns None for Description (uses SimpleTextArea) and Repos.
    pub fn focused_input_mut(&mut self) -> Option<&mut SimpleTextInput> {
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
            .set_text(&format!("{username}/{slug}-{suffix}"));
    }
}

/// Maximum length of the slugified portion of a branch name.
const MAX_SLUG_LEN: usize = 50;

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

/// Truncate a slug to at most `max_len` bytes, cutting at the last hyphen
/// boundary to avoid mid-word breaks. Falls back to a hard cut when the
/// slug contains no hyphens within the limit.
fn truncate_slug(slug: &str, max_len: usize) -> String {
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

    // -- SimpleTextArea tests --

    #[test]
    fn textarea_basic_insert_and_newline() {
        let mut ta = SimpleTextArea::new();
        ta.insert_char('a');
        ta.insert_char('b');
        ta.insert_newline();
        ta.insert_char('c');
        assert_eq!(ta.text(), "ab\nc");
        assert_eq!(ta.cursor_pos(), (1, 1));
    }

    #[test]
    fn textarea_backspace_joins_lines() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("hello\nworld");
        // cursor at end of "world"
        ta.home(); // start of "world"
        ta.backspace(); // should join with previous line
        assert_eq!(ta.text(), "helloworld");
        assert_eq!(ta.cursor_pos(), (0, 5));
    }

    #[test]
    fn textarea_delete_joins_lines() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("hello\nworld");
        // Move to end of first line
        ta.cursor_row = 0;
        ta.cursor_col = 5;
        ta.delete(); // should join with next line
        assert_eq!(ta.text(), "helloworld");
    }

    #[test]
    fn textarea_move_up_down() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("abc\nde\nfghij");
        ta.cursor_row = 0;
        ta.cursor_col = 3; // end of "abc"

        ta.move_down();
        assert_eq!(ta.cursor_row, 1);
        // char col 3 clamps to len of "de" = 2
        assert_eq!(ta.cursor_col, 2);

        ta.move_down();
        assert_eq!(ta.cursor_row, 2);
        // restores to char col 2 (byte 2 in "fghij")
        assert_eq!(ta.cursor_col, 2);

        ta.move_up();
        assert_eq!(ta.cursor_row, 1);
    }

    #[test]
    fn textarea_move_left_wraps_to_prev_line() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("ab\ncd");
        ta.cursor_row = 1;
        ta.cursor_col = 0;
        ta.move_left(); // wrap to end of "ab"
        assert_eq!(ta.cursor_row, 0);
        assert_eq!(ta.cursor_col, 2);
    }

    #[test]
    fn textarea_move_right_wraps_to_next_line() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("ab\ncd");
        ta.cursor_row = 0;
        ta.cursor_col = 2; // end of "ab"
        ta.move_right(); // wrap to start of "cd"
        assert_eq!(ta.cursor_row, 1);
        assert_eq!(ta.cursor_col, 0);
    }

    #[test]
    fn textarea_clear() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("some\ntext");
        ta.clear();
        assert_eq!(ta.text(), "");
        assert_eq!(ta.cursor_pos(), (0, 0));
    }

    #[test]
    fn textarea_visible_lines_with_scroll() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("line0\nline1\nline2\nline3\nline4");
        ta.scroll_offset = 2;
        let visible = ta.visible_lines(2);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0], "line2");
        assert_eq!(visible[1], "line3");
    }

    #[test]
    fn textarea_ensure_visible() {
        let mut ta = SimpleTextArea::new();
        ta.set_text("a\nb\nc\nd\ne");
        ta.cursor_row = 4;
        ta.ensure_visible(3);
        assert_eq!(ta.scroll_offset, 2); // lines 2,3,4 visible
    }

    #[test]
    fn textarea_boundary_no_panic() {
        let mut ta = SimpleTextArea::new();
        // Operations on empty
        ta.backspace();
        ta.delete();
        ta.move_left();
        ta.move_right();
        ta.move_up();
        ta.move_down();
        assert_eq!(ta.text(), "");
    }

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
}
