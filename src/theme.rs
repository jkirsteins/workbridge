use ratatui_core::style::{Color, Modifier, Style};

use crate::work_item::WorkItemStatus;

/// All colors used by the TUI, in one place. Swap this struct to change
/// the entire look. Every color in ui.rs comes from here - no inline
/// Color:: literals.
pub struct Theme {
    // -- Borders --
    /// Left panel border when focused.
    pub border_focused: Color,
    /// Left panel border when unfocused.
    pub border_unfocused: Color,
    /// Right panel border when focused (input mode).
    pub border_input: Color,
    /// Right panel border when unfocused.
    pub border_default: Color,
    /// Settings overlay border.
    pub border_overlay: Color,
    /// Alert/error dialog border.
    pub border_alert: Color,
    /// Subtle borders (e.g., inner list in settings).
    pub border_subtle: Color,

    // -- Scrollbar --
    /// Scrollbar thumb (the draggable indicator).
    pub scrollbar_thumb: Color,
    /// Scrollbar track (the gutter).
    pub scrollbar_track: Color,

    // -- Tab list --
    /// Highlight bar: foreground.
    pub tab_highlight_fg: Color,
    /// Highlight bar: background.
    pub tab_highlight_bg: Color,

    // -- Text --
    /// Primary text (readable on any terminal background).
    pub text: Color,
    /// De-emphasized text (placeholders, hints). Must still be readable.
    pub text_muted: Color,
    /// Section headers in settings overlay.
    pub text_heading: Color,
    /// Error / dead session messages.
    pub text_error: Color,

    // -- Titles --
    /// Title foreground color.
    pub title_fg: Color,
    /// Title background color (gives visual separation).
    pub title_bg: Color,

    // -- Status bar --
    /// Normal status bar foreground.
    pub status_fg: Color,
    /// Normal status bar background.
    pub status_bg: Color,
    /// Shutdown status bar foreground.
    pub status_shutdown_fg: Color,
    /// Shutdown status bar background.
    pub status_shutdown_bg: Color,

    // -- Activity indicator --
    /// Activity spinner character color.
    pub activity_spinner: Color,
    /// Activity message text color.
    pub activity_fg: Color,
    /// Activity indicator background.
    pub activity_bg: Color,

    // -- Context bar --
    /// Work-item context bar foreground (title and repo path).
    pub context_fg: Color,
    /// Work-item context bar background.
    pub context_bg: Color,

    // -- Work item groups and badges --
    /// Group header text color (e.g., "TODO (2)").
    pub group_header: Color,
    /// Background for sticky group headers pinned at the top of the list.
    pub sticky_header_bg: Color,
    /// PR badge color (open PR).
    pub badge_pr: Color,
    /// CI passing badge color.
    pub badge_ci_pass: Color,
    /// CI failing badge color.
    pub badge_ci_fail: Color,
    /// CI pending badge color.
    pub badge_ci_pending: Color,
    /// Merge-conflict badge color (PR has conflicts with its base branch).
    pub badge_merge_conflict: Color,
    /// Unlinked item "?" marker color.
    pub unlinked_marker: Color,
    /// Review request "R" marker color (pre-import).
    pub review_request_marker: Color,
    /// Review request kind badge "[RR]" color (post-import).
    pub badge_review_request_kind: Color,
    /// Done item text color (muted, completed work is less prominent).
    pub done_item: Color,

    // -- Session activity badges --
    /// Session exists but idle (filled circle).
    pub badge_session_idle: Color,
    /// Session actively working (animated spinner).
    pub badge_session_working: Color,

    // -- Stage badges --
    /// Backlog stage badge color.
    pub badge_backlog: Color,
    /// Planning stage badge color.
    pub badge_planning: Color,
    /// Implementing stage badge color.
    pub badge_implementing: Color,
    /// Blocked stage badge color.
    pub badge_blocked: Color,
    /// Review stage badge color.
    pub badge_review: Color,
    /// Mergequeue stage badge color.
    pub badge_mergequeue: Color,
    /// Done stage badge color.
    pub badge_done: Color,
}

impl Theme {
    /// Default theme. Uses Reset for text rendered against the terminal's
    /// own background (which we don't control). Absolute colors (Black,
    /// White, DarkGray) are safe when the Theme sets both fg AND bg (e.g.,
    /// highlight bars, status bars) since contrast is guaranteed.
    pub fn default_theme() -> Self {
        Self {
            border_focused: Color::Cyan,
            border_unfocused: Color::Reset,
            border_input: Color::Green,
            border_default: Color::Reset,
            border_overlay: Color::Cyan,
            border_alert: Color::Red,
            border_subtle: Color::Reset,

            scrollbar_thumb: Color::Gray,
            scrollbar_track: Color::Reset,

            tab_highlight_fg: Color::Black,
            tab_highlight_bg: Color::Cyan,

            text: Color::Reset,
            text_muted: Color::Reset,
            text_heading: Color::Cyan,
            text_error: Color::Red,

            title_fg: Color::Reset,
            title_bg: Color::Reset,

            status_fg: Color::Yellow,
            status_bg: Color::Reset,
            status_shutdown_fg: Color::Red,
            status_shutdown_bg: Color::Reset,

            activity_spinner: Color::Cyan,
            activity_fg: Color::Cyan,
            activity_bg: Color::Reset,

            context_fg: Color::Cyan,
            context_bg: Color::Reset,

            group_header: Color::Cyan,
            sticky_header_bg: Color::DarkGray,
            badge_pr: Color::Green,
            badge_ci_pass: Color::Green,
            badge_ci_fail: Color::Red,
            badge_ci_pending: Color::Yellow,
            badge_merge_conflict: Color::Red,
            unlinked_marker: Color::Yellow,
            review_request_marker: Color::Magenta,
            badge_review_request_kind: Color::Magenta,
            done_item: Color::Reset,

            badge_session_idle: Color::Gray,
            badge_session_working: Color::Cyan,

            badge_backlog: Color::Reset,
            badge_planning: Color::Cyan,
            badge_implementing: Color::Green,
            badge_blocked: Color::Red,
            badge_review: Color::Yellow,
            badge_mergequeue: Color::Magenta,
            badge_done: Color::Reset,
        }
    }
}

// -- Convenience style builders --

impl Theme {
    pub fn style_border_focused(&self) -> Style {
        Style::default().fg(self.border_focused)
    }

    pub fn style_border_unfocused(&self) -> Style {
        Style::default().fg(self.border_unfocused)
    }

    pub fn style_border_input(&self) -> Style {
        Style::default().fg(self.border_input)
    }

    pub fn style_border_default(&self) -> Style {
        Style::default().fg(self.border_default)
    }

    pub fn style_border_overlay(&self) -> Style {
        Style::default().fg(self.border_overlay)
    }

    pub fn style_border_alert(&self) -> Style {
        Style::default().fg(self.border_alert)
    }

    pub fn style_border_subtle(&self) -> Style {
        Style::default().fg(self.border_subtle)
    }

    pub fn style_scrollbar_thumb(&self) -> Style {
        Style::default().fg(self.scrollbar_thumb)
    }

    pub fn style_scrollbar_track(&self) -> Style {
        Style::default().fg(self.scrollbar_track)
    }

    pub fn style_tab_highlight(&self) -> Style {
        Style::default()
            .fg(self.tab_highlight_fg)
            .bg(self.tab_highlight_bg)
            .add_modifier(Modifier::BOLD)
    }

    /// Background-only highlight for the List widget. Per-span fg colors
    /// set inside ListItems are preserved; spans apply their own fg
    /// (including highlight-aware overrides) instead of being forced to
    /// a single color by Cell::set_style.
    pub fn style_tab_highlight_bg(&self) -> Style {
        Style::default().bg(self.tab_highlight_bg)
    }

    pub fn style_text(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn style_text_muted(&self) -> Style {
        Style::default()
            .fg(self.text_muted)
            .add_modifier(Modifier::DIM)
    }

    pub fn style_heading(&self) -> Style {
        Style::default().fg(self.text_heading)
    }

    pub fn style_error(&self) -> Style {
        Style::default().fg(self.text_error)
    }

    pub fn style_title(&self) -> Style {
        Style::default()
            .fg(self.title_fg)
            .bg(self.title_bg)
            .add_modifier(Modifier::REVERSED)
    }

    pub fn style_status(&self) -> Style {
        Style::default().fg(self.status_fg).bg(self.status_bg)
    }

    pub fn style_status_shutdown(&self) -> Style {
        Style::default()
            .fg(self.status_shutdown_fg)
            .bg(self.status_shutdown_bg)
    }

    pub fn style_activity_spinner(&self) -> Style {
        Style::default()
            .fg(self.activity_spinner)
            .bg(self.activity_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_activity(&self) -> Style {
        Style::default().fg(self.activity_fg).bg(self.activity_bg)
    }

    pub fn style_context(&self) -> Style {
        Style::default().fg(self.context_fg).bg(self.context_bg)
    }

    pub fn style_group_header(&self) -> Style {
        Style::default()
            .fg(self.group_header)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_group_header_blocked(&self) -> Style {
        Style::default()
            .fg(self.badge_blocked)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_sticky_header(&self) -> Style {
        Style::default()
            .fg(self.group_header)
            .bg(self.sticky_header_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_sticky_header_blocked(&self) -> Style {
        Style::default()
            .fg(self.badge_blocked)
            .bg(self.sticky_header_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_badge_pr(&self) -> Style {
        Style::default().fg(self.badge_pr)
    }

    pub fn style_badge_ci_pass(&self) -> Style {
        Style::default().fg(self.badge_ci_pass)
    }

    pub fn style_badge_ci_fail(&self) -> Style {
        Style::default().fg(self.badge_ci_fail)
    }

    pub fn style_badge_ci_pending(&self) -> Style {
        Style::default().fg(self.badge_ci_pending)
    }

    pub fn style_badge_merge_conflict(&self) -> Style {
        Style::default().fg(self.badge_merge_conflict)
    }

    pub fn style_badge_session_idle(&self) -> Style {
        Style::default().fg(self.badge_session_idle)
    }

    pub fn style_badge_session_working(&self) -> Style {
        Style::default()
            .fg(self.badge_session_working)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_unlinked_marker(&self) -> Style {
        Style::default().fg(self.unlinked_marker)
    }

    pub fn style_review_request_marker(&self) -> Style {
        Style::default().fg(self.review_request_marker)
    }

    pub fn style_badge_review_request_kind(&self) -> Style {
        Style::default()
            .fg(self.badge_review_request_kind)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_done_item(&self) -> Style {
        Style::default()
            .fg(self.done_item)
            .add_modifier(Modifier::DIM)
    }

    // -- View mode header styles --

    /// Style for the inactive tab label in the view mode header.
    pub fn style_view_mode_tab(&self) -> Style {
        Style::default()
            .fg(self.text_muted)
            .add_modifier(Modifier::DIM)
    }

    /// Style for the active (selected) tab label in the view mode header.
    pub fn style_view_mode_tab_active(&self) -> Style {
        Style::default()
            .fg(self.tab_highlight_fg)
            .bg(self.tab_highlight_bg)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for keybinding hints in the view mode header.
    pub fn style_view_mode_hints(&self) -> Style {
        Style::default()
            .fg(self.text_muted)
            .add_modifier(Modifier::DIM)
    }

    // -- Board view styles --
    // Reuse existing colors for visual consistency.

    pub fn style_board_column_focused(&self) -> Style {
        Style::default().fg(self.border_focused)
    }

    pub fn style_board_column_unfocused(&self) -> Style {
        Style::default().fg(self.border_unfocused)
    }

    pub fn style_board_column_header(&self) -> Style {
        Style::default()
            .fg(self.text_heading)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_board_item_highlight(&self) -> Style {
        Style::default()
            .fg(self.tab_highlight_fg)
            .bg(self.tab_highlight_bg)
            .add_modifier(Modifier::BOLD)
    }

    pub fn style_stage_badge(&self, status: &WorkItemStatus) -> Style {
        let color = match status {
            WorkItemStatus::Backlog => self.badge_backlog,
            WorkItemStatus::Planning => self.badge_planning,
            WorkItemStatus::Implementing => self.badge_implementing,
            WorkItemStatus::Blocked => self.badge_blocked,
            WorkItemStatus::Review => self.badge_review,
            WorkItemStatus::Mergequeue => self.badge_mergequeue,
            WorkItemStatus::Done => self.badge_done,
        };
        let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        if *status == WorkItemStatus::Blocked {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if *status == WorkItemStatus::Done {
            style = style.add_modifier(Modifier::DIM);
        }
        style
    }
}
