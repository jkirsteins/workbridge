use crate::app::{App, FocusPanel, RightPanelTab};
use crate::create_dialog::CreateDialogFocus;
use crate::event::util::any_modal_visible;

/// Convert a paste payload into a single-line-safe form by replacing
/// `\r\n`, `\n`, and `\r` with a single space each. Used for every
/// `TextInputState` (single-line) paste target. The Description
/// `TextAreaState` does NOT use this - it preserves newlines verbatim.
///
/// The replacement order matters: collapsing `\r\n` first prevents a
/// single CRLF from producing two spaces after the subsequent `\n`
/// and `\r` passes.
pub fn flatten_paste_for_single_line(data: &str) -> String {
    // Collapse CRLF to a single space first so a CRLF does not produce
    // two spaces after the subsequent per-char pass.
    data.replace("\r\n", " ").replace(['\n', '\r'], " ")
}

/// Handle a paste event (e.g. drag-and-drop file path, system clipboard,
/// terminal "Paste" menu, OSC 52 injection) by routing it to the focused
/// text input if a modal owns the screen, otherwise forwarding it to the
/// focused PTY session as a bracketed paste sequence so the receiving
/// application handles it atomically.
pub fn handle_paste(app: &mut App, data: &str) -> bool {
    if app.shell.shutting_down {
        return false;
    }

    // Modal text inputs take priority over the underlying PTY: if any
    // text-input field owns focus inside a modal, the paste lands there.
    // Modals without a text-input target swallow the paste (no leak to
    // PTY) - matches the existing key-routing precedence in `handle_key`.
    if any_modal_visible(app) {
        return route_paste_to_modal_input(app, data);
    }

    // No modal: PTY routing as before.
    let bracketed = format!("\x1b[200~{data}\x1b[201~");
    if app.global_drawer.open {
        app.send_bytes_to_global(bracketed.as_bytes());
        return true;
    }
    match app.shell.focus {
        FocusPanel::Right => {
            match app.right_panel_tab {
                RightPanelTab::ClaudeCode => app.send_bytes_to_active(bracketed.as_bytes()),
                RightPanelTab::Terminal => app.send_bytes_to_terminal(bracketed.as_bytes()),
            }
            true
        }
        FocusPanel::Left => false,
    }
}

/// Route a paste event to the focused text input inside whichever modal
/// is currently up. The invariant is behavioral, not textual: for every
/// reachable app state, paste lands in the same field `handle_key`
/// would type into. This holds because all modal flags in
/// `any_modal_visible` are mutually exclusive at runtime (`set_branch`,
/// `create_dialog`, rework, cleanup, settings editing, merge, delete,
/// branch-gone, stale-worktree, no-plan, alert, and in-progress
/// spinners are each opened by flows that refuse to run while another
/// modal is up), so the literal arm order in this function does not
/// have to match `handle_key` - at most one condition is ever true.
///
/// When adding a new modal, verify it is mutually exclusive with every
/// other modal (or extend both handlers to agree on the stacking
/// precedence). Do NOT rely on arm order here as a correctness
/// argument.
///
/// Returns `true` when the paste was inserted into a text input (a
/// re-render is needed), or `false` when the active modal has no text
/// input in focus (e.g. Repos checkbox area, merge-strategy prompt,
/// delete prompt, branch-gone prompt, stale-worktree prompt, no-plan
/// prompt, in-progress spinners, alert message). The `false` return
/// value reaches `salsa.rs`, where it suppresses the re-render and
/// keeps the paste from leaking to any PTY - the modal swallows it.
pub fn route_paste_to_modal_input(app: &mut App, data: &str) -> bool {
    // Each arm checks one modal state. Because every modal in
    // `any_modal_visible` is mutually exclusive with every other
    // modal, at most one arm will match for any reachable app state -
    // so the order below is not load-bearing for correctness.

    // 1. Set-branch recovery modal (single-line branch input).
    if let Some(dlg) = app.set_branch_dialog.as_mut() {
        dlg.input.insert_str(flatten_paste_for_single_line(data));
        return true;
    }

    // 2. Rework reason prompt (single-line input).
    if app.rework_prompt_visible {
        app.rework_prompt_input
            .insert_str(flatten_paste_for_single_line(data));
        return true;
    }

    // 3. Cleanup reason input (single-line input). Only active after the
    //    confirmation prompt has transitioned to "Enter reason" mode; the
    //    plain cleanup_prompt_visible state has no text input target.
    if app.cleanup_reason_input_active {
        app.cleanup_reason_input
            .insert_str(flatten_paste_for_single_line(data));
        return true;
    }

    // 4. Settings review-skill input (single-line, only while editing).
    if app.show_settings && app.settings_review_skill_editing {
        app.settings_review_skill_input
            .insert_str(flatten_paste_for_single_line(data));
        return true;
    }

    // 5. Create dialog: per-focus routing. Title and Branch are
    //    single-line; Description preserves newlines; Repos is a
    //    checkbox area with no text input target.
    if app.create_dialog.visible {
        match app.create_dialog.focus_field {
            CreateDialogFocus::Title => {
                app.create_dialog
                    .title_input
                    .insert_str(flatten_paste_for_single_line(data));
                return true;
            }
            CreateDialogFocus::Branch => {
                app.create_dialog
                    .branch_input
                    .insert_str(flatten_paste_for_single_line(data));
                // Pasting is a content edit, not navigation - mark the
                // branch as user-edited so `auto_fill_branch` does not
                // overwrite the paste on the next Tab off Title.
                app.create_dialog.branch_user_edited = true;
                return true;
            }
            CreateDialogFocus::Description => {
                // Multi-line paste lands verbatim; newlines preserved.
                app.create_dialog.description_input.insert_str(data);
                return true;
            }
            CreateDialogFocus::Repos => {
                // No text input target in the checkbox area.
                return false;
            }
        }
    }

    // 6. Any other modal (merge-strategy prompt, delete prompt,
    //    no-plan prompt, branch-gone prompt, stale-worktree prompt,
    //    cleanup confirmation prompt before reason-input is active,
    //    in-progress spinners, alert message) - no text input target.
    false
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::app::SettingsTab;

    /// `flatten_paste_for_single_line` must collapse CRLF to a single
    /// space, not two. Without the explicit `\r\n` -> ` ` pass (ordered
    /// before `\n` and `\r`), each CRLF would be replaced twice and
    /// produce two spaces instead of one.
    #[test]
    fn flatten_paste_collapses_crlf() {
        assert_eq!(
            flatten_paste_for_single_line("first\r\nsecond"),
            "first second"
        );
        assert_eq!(flatten_paste_for_single_line("a\r\nb\r\nc"), "a b c");
    }

    /// Bare LF is also replaced with a single space.
    #[test]
    fn flatten_paste_collapses_lf() {
        assert_eq!(
            flatten_paste_for_single_line("first\nsecond"),
            "first second"
        );
    }

    /// Bare CR (classic Mac line ending, or a terminal that reports
    /// Enter as `\r`) is replaced with a single space.
    #[test]
    fn flatten_paste_collapses_cr() {
        assert_eq!(
            flatten_paste_for_single_line("first\rsecond"),
            "first second"
        );
    }

    /// Pasting multi-line text into Title strips newlines and does not
    /// trigger `auto_fill_branch` on its own. Auto-fill happens on Tab
    /// off Title; pasting must NOT eagerly populate Branch.
    #[test]
    fn paste_into_create_dialog_title_strips_newlines() {
        let mut app = App::new();
        app.create_dialog.open(&[PathBuf::from("/repo/a")], None);
        assert_eq!(app.create_dialog.focus_field, CreateDialogFocus::Title);

        let changed = handle_paste(&mut app, "first line\nsecond line");
        assert!(changed, "paste into Title should return true");
        assert_eq!(
            app.create_dialog.title_input.text(),
            "first line second line",
            "newline in pasted text must be flattened to a space",
        );
        assert_eq!(
            app.create_dialog.branch_input.text(),
            "",
            "pasting into Title must not auto-fill Branch on its own",
        );
    }

    /// Pasting into the Branch field marks `branch_user_edited = true`
    /// so a later Tab off Title will not overwrite the pasted branch.
    #[test]
    fn paste_into_create_dialog_branch_marks_user_edited() {
        let mut app = App::new();
        app.create_dialog.open(&[PathBuf::from("/repo/a")], None);
        app.create_dialog.focus_field = CreateDialogFocus::Branch;
        assert!(
            !app.create_dialog.branch_user_edited,
            "sanity: branch_user_edited starts false",
        );

        let changed = handle_paste(&mut app, "user/custom-branch");
        assert!(changed, "paste into Branch should return true");
        assert_eq!(app.create_dialog.branch_input.text(), "user/custom-branch",);
        assert!(
            app.create_dialog.branch_user_edited,
            "pasting into Branch must mark branch_user_edited so a \
             later auto_fill_branch cannot overwrite the paste",
        );
    }

    /// Pasting into the Description `TextAreaState` preserves newlines
    /// verbatim so a multi-line paste lands as multiple lines.
    #[test]
    fn paste_into_create_dialog_description_preserves_newlines() {
        let mut app = App::new();
        app.create_dialog.open(&[PathBuf::from("/repo/a")], None);
        app.create_dialog.focus_field = CreateDialogFocus::Description;

        let changed = handle_paste(&mut app, "a\nb\nc");
        assert!(changed, "paste into Description should return true");
        let text = app.create_dialog.description_input.text();
        assert!(
            text.contains('a') && text.contains('b') && text.contains('c'),
            "description must contain all pasted lines, got: {text:?}",
        );
        assert!(
            text.contains('\n'),
            "description paste must preserve newlines, got: {text:?}",
        );
    }

    /// Pasting while focus is on the Repos checkbox area is a silent
    /// no-op: all text inputs must stay empty and the function returns
    /// `false` so `salsa.rs` skips the re-render.
    #[test]
    fn paste_into_create_dialog_repos_is_noop() {
        let mut app = App::new();
        app.create_dialog.open(&[PathBuf::from("/repo/a")], None);
        app.create_dialog.focus_field = CreateDialogFocus::Repos;

        let changed = handle_paste(&mut app, "this must not land anywhere");
        assert!(!changed, "paste into Repos focus must return false (no-op)");
        assert_eq!(app.create_dialog.title_input.text(), "");
        assert_eq!(app.create_dialog.branch_input.text(), "");
        assert_eq!(app.create_dialog.description_input.text(), "");
    }

    /// Pasting into the rework prompt text input inserts the text and
    /// strips newlines (single-line field).
    #[test]
    fn paste_into_rework_prompt_inserts_text() {
        let mut app = App::new();
        app.rework_prompt_visible = true;
        app.rework_prompt_wi = Some(crate::work_item::WorkItemId::LocalFile(PathBuf::from(
            "/tmp/test.json",
        )));

        let changed = handle_paste(&mut app, "needs more tests\nand docs");
        assert!(changed, "paste into rework prompt should return true");
        assert_eq!(app.rework_prompt_input.text(), "needs more tests and docs",);
    }

    /// Pasting into the cleanup-reason input inserts the text and
    /// strips newlines. Must only route while the reason-input sub-mode
    /// is active; the prior confirmation prompt has no text input.
    #[test]
    fn paste_into_cleanup_reason_input_inserts_text() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        app.cleanup_reason_input_active = true;

        let changed = handle_paste(&mut app, "abandoned\nsee ticket");
        assert!(
            changed,
            "paste into cleanup reason input should return true",
        );
        assert_eq!(app.cleanup_reason_input.text(), "abandoned see ticket",);
    }

    /// While the cleanup confirmation prompt is visible but the reason
    /// input has not been opened yet, paste is a no-op (matches the key
    /// handler - the prompt only accepts [Enter]/[d]/[Esc]).
    #[test]
    fn paste_into_cleanup_prompt_before_reason_input_is_noop() {
        let mut app = App::new();
        app.cleanup_prompt_visible = true;
        // cleanup_reason_input_active stays false.

        let changed = handle_paste(&mut app, "stray paste");
        assert!(
            !changed,
            "cleanup prompt without reason input must swallow paste",
        );
        assert_eq!(app.cleanup_reason_input.text(), "");
    }

    /// Pasting into the "Set branch name" recovery modal inserts the
    /// text with newlines stripped.
    #[test]
    fn paste_into_set_branch_dialog_inserts_text() {
        use crate::create_dialog::{PendingBranchAction, SetBranchDialog};
        use crate::work_item::WorkItemId;

        let mut app = App::new();
        let mut input = rat_widget::text_input::TextInputState::new();
        input.set_text("user/");
        app.set_branch_dialog = Some(SetBranchDialog {
            wi_id: WorkItemId::LocalFile(PathBuf::from("/tmp/test.json")),
            input,
            pending: PendingBranchAction::SpawnSession,
        });

        // Cursor is at the start by default on a fresh TextInputState;
        // reposition to end so the paste appends to the existing text.
        if let Some(dlg) = app.set_branch_dialog.as_mut() {
            dlg.input.move_to_line_end(false);
        }

        let changed = handle_paste(&mut app, "feature\nname");
        assert!(changed, "paste into set-branch dialog should return true");
        let text = app
            .set_branch_dialog
            .as_ref()
            .map(|d| d.input.text().to_string())
            .unwrap_or_default();
        assert_eq!(text, "user/feature name");
    }

    /// Pasting into the settings review-skill input works only while
    /// editing mode is active; newlines are stripped.
    #[test]
    fn paste_into_settings_review_skill_input_inserts_text() {
        let mut app = App::new();
        app.show_settings = true;
        app.settings_tab = SettingsTab::ReviewGate;
        app.settings_review_skill_editing = true;
        app.settings_review_skill_input.clear();

        let changed = handle_paste(&mut app, "claude-toolkit:principled-review");
        assert!(
            changed,
            "paste into settings review-skill input should return true",
        );
        assert_eq!(
            app.settings_review_skill_input.text(),
            "claude-toolkit:principled-review",
        );
    }

    /// When settings is open but edit mode is NOT active, paste is a
    /// no-op - the overlay has no text input target in that state.
    #[test]
    fn paste_into_settings_without_edit_mode_is_noop() {
        let mut app = App::new();
        app.show_settings = true;
        app.settings_tab = SettingsTab::ReviewGate;
        app.settings_review_skill_editing = false;
        app.settings_review_skill_input.clear();

        let changed = handle_paste(&mut app, "stray paste");
        assert!(
            !changed,
            "settings overlay without edit mode must swallow paste",
        );
        assert_eq!(app.settings_review_skill_input.text(), "");
    }

    /// Regression guard: with no modal visible and focus on the left
    /// panel, paste returns `false` (matches the pre-change PTY path).
    /// The left panel never receives pasted text.
    #[test]
    fn paste_with_no_modal_returns_false_when_left_focused() {
        let mut app = App::new();
        app.shell.focus = FocusPanel::Left;
        // Sanity: no modal is up.
        assert!(!any_modal_visible(&app));

        let changed = handle_paste(&mut app, "stray paste");
        assert!(
            !changed,
            "paste with left focus and no modal must return false",
        );
    }

    /// Shutdown short-circuits paste handling entirely: no modal
    /// routing, no PTY write, returns `false` so `salsa.rs` skips the
    /// re-render.
    #[test]
    fn paste_during_shutdown_returns_false() {
        let mut app = App::new();
        app.shell.shutting_down = true;
        app.create_dialog.open(&[PathBuf::from("/repo/a")], None);

        let changed = handle_paste(&mut app, "must be ignored");
        assert!(
            !changed,
            "paste during shutdown must return false (no routing)",
        );
        assert_eq!(
            app.create_dialog.title_input.text(),
            "",
            "shutdown must short-circuit before any text input insert",
        );
    }
}
