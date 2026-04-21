//! `SettingsOverlay` subsystem - all state owned by the settings modal
//! overlay (the `?` key).
//!
//! `App` used to own `show_settings`, `settings_repo_selected`,
//! `settings_available_selected`,
//! `settings_tab`, `settings_list_focus`, `settings_keybindings_scroll`,
//! `settings_review_skill_input`, and `settings_review_skill_editing` as
//! eight sibling fields on `App`. This file consolidates them into a
//! single owning struct so every settings mutation goes through a
//! narrow interface on `SettingsOverlay`, and the rest of `App` cannot
//! poke the overlay state directly.
//!
//! `active_repo_cache` is **not** part of this subsystem - it is the
//! canonical "managed repos" cache that every work-item reassembly,
//! spawn site, and display read consults. It lives on `App` because
//! its consumers span far beyond the settings overlay.
//!
//! Field-borrow splitting at the event dispatcher lets the settings
//! overlay be borrowed disjointly from the rest of `App` when the
//! overlay's own key handlers run.

use super::{SettingsListFocus, SettingsTab};

/// Owns every field that is **only** read or written while the settings
/// modal overlay is open, or that represents persisted UI state scoped
/// to the settings view (cursors, tab selection, keybindings scroll,
/// review-skill input).
///
/// Rendered via `ui::overlays::settings::draw_settings_overlay` when
/// `visible` is true.
#[derive(Debug, Default)]
pub struct SettingsOverlay {
    /// Whether to show the settings overlay.
    pub visible: bool,
    /// Cursor position in the managed repos list.
    pub repo_selected: usize,
    /// Cursor position in the available repos list.
    pub available_selected: usize,
    /// Which top-level tab is active in the settings overlay.
    pub tab: SettingsTab,
    /// Which column has focus inside the Repos tab.
    pub list_focus: SettingsListFocus,
    /// Scroll offset for the keybindings tab.
    pub keybindings_scroll: u16,
    /// Text input for editing the review skill in the Review Gate tab.
    pub review_skill_input: rat_widget::text_input::TextInputState,
    /// Whether the review skill text input is in editing mode.
    pub review_skill_editing: bool,
}

impl SettingsOverlay {
    /// Construct a closed settings overlay with all defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the settings overlay, resetting the review-skill input to
    /// not-editing. Does not reset cursors so re-opening the overlay
    /// lands the user on the same row they last navigated to.
    pub const fn open(&mut self) {
        self.visible = true;
        self.review_skill_editing = false;
    }

    /// Close the settings overlay AND reset the overlay to its
    /// "fresh open" shape: tab back to Repos, cursors to 0, focus on
    /// the Managed column, scroll to 0, review-skill text input
    /// cleared. This matches the canonical `?`/Esc dismissal
    /// behaviour used by the TUI so re-opening is predictable.
    ///
    /// Used by the close key binding (`?` / Esc). Callers that want
    /// to close without wiping cursor state should set
    /// `self.visible = false` directly (no such caller exists today).
    pub fn close(&mut self) {
        self.visible = false;
        self.tab = SettingsTab::Repos;
        self.repo_selected = 0;
        self.available_selected = 0;
        self.list_focus = SettingsListFocus::Managed;
        self.keybindings_scroll = 0;
        self.review_skill_editing = false;
        self.review_skill_input.clear();
    }

    /// Toggle the overlay. Reset of the editing flag matches `open`.
    pub fn toggle(&mut self) {
        if self.visible {
            self.close();
        } else {
            self.open();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_overlay_is_closed() {
        let s = SettingsOverlay::new();
        assert!(!s.visible);
        assert!(!s.review_skill_editing);
        assert_eq!(s.repo_selected, 0);
        assert_eq!(s.available_selected, 0);
        assert_eq!(s.keybindings_scroll, 0);
        assert_eq!(s.tab, SettingsTab::Repos);
        assert_eq!(s.list_focus, SettingsListFocus::Managed);
    }

    #[test]
    fn open_and_close_flip_visible() {
        let mut s = SettingsOverlay::new();
        s.open();
        assert!(s.visible);
        s.close();
        assert!(!s.visible);
    }

    #[test]
    fn toggle_flips_visibility_each_call() {
        let mut s = SettingsOverlay::new();
        s.toggle();
        assert!(s.visible);
        s.toggle();
        assert!(!s.visible);
    }

    #[test]
    fn open_clears_editing_flag() {
        // Scenario: user was editing the review skill, closed without
        // committing, then reopened - the input must land back in
        // navigate-only mode so arrow keys drive navigation, not the
        // text cursor.
        let mut s = SettingsOverlay::new();
        s.review_skill_editing = true;
        s.visible = true;
        s.close();
        s.open();
        assert!(!s.review_skill_editing);
    }

    #[test]
    fn close_resets_cursors_tab_and_editing() {
        // Error / stateful path: closing the overlay via the `?`/Esc
        // key must reset the overlay to its "fresh open" shape so
        // re-opening does not land on a half-navigated state. This
        // matches the pre-extraction TUI behaviour in `event::handle_key`.
        let mut s = SettingsOverlay::new();
        s.open();
        s.repo_selected = 3;
        s.available_selected = 2;
        s.keybindings_scroll = 17;
        s.tab = SettingsTab::Keybindings;
        s.list_focus = SettingsListFocus::Available;
        s.review_skill_editing = true;
        s.close();
        assert!(!s.visible);
        assert_eq!(s.tab, SettingsTab::Repos);
        assert_eq!(s.list_focus, SettingsListFocus::Managed);
        assert_eq!(s.repo_selected, 0);
        assert_eq!(s.available_selected, 0);
        assert_eq!(s.keybindings_scroll, 0);
        assert!(!s.review_skill_editing);
    }
}
