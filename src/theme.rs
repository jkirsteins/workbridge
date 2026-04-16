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
    /// Meta-line foreground used on selected list rows. Paired with
    /// `tab_highlight_bg`; must retain readable contrast against it.
    /// Lives next to the highlight fields so any retune of the highlight
    /// bg can adjust this in the same place.
    pub meta_selected_fg: Color,

    // -- Text --
    /// Primary text (readable on any terminal background).
    pub text: Color,
    /// De-emphasized text (placeholders, hints). Must still be readable.
    pub text_muted: Color,
    /// Section headers in settings overlay.
    pub text_heading: Color,
    /// Error / dead session messages.
    pub text_error: Color,
    /// Interactive / click-to-copy label accent (used together with an
    /// UNDERLINED modifier to signal a clickable affordance). See
    /// `style_interactive()` for the combined style and docs/UI.md
    /// "Interactive labels" for the convention.
    pub interactive_fg: Color,

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
    /// Unclean-worktree `!cl` badge color. Distinct from `badge_ci_fail`
    /// (red) so a row that is both CI-failing and locally unclean can
    /// render both chips without them visually merging. Yellow/orange
    /// to read as a warning without screaming "error".
    pub badge_worktree_unclean: Color,
    /// Needs-push `!pushed` badge color. Shown when the local branch is
    /// ahead of its upstream (unpushed commits). Magenta, shared with
    /// `!pulled`, because both chips represent the same semantic
    /// category - the local branch has diverged from its upstream -
    /// and the chip label (`!pushed` vs `!pulled`) already carries
    /// the direction. Distinctness is preserved against the red
    /// `!merge` / `fail` chips and the yellow `!cl` chip on the same
    /// row.
    pub badge_pushed: Color,
    /// Needs-pull `!pulled` badge color. Shown when the local branch is
    /// behind its upstream (the remote moved while we were working).
    /// Magenta, shared with `!pushed`, because both chips represent
    /// upstream divergence and the chip label carries the direction.
    /// A row that is diverged in both directions renders
    /// `!pushed !pulled` in the same Magenta; the labels, not the
    /// colors, distinguish the two directions.
    pub badge_pulled: Color,
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
            meta_selected_fg: Color::DarkGray,

            text: Color::Reset,
            text_muted: Color::Reset,
            text_heading: Color::Cyan,
            text_error: Color::Red,
            // Cyan reads well against both dark and light terminal
            // backgrounds and matches the existing heading accent.
            interactive_fg: Color::Cyan,

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
            // LightYellow reads as a warning without colliding with the
            // other chip colors in the row: `fail` is Red, merge-conflict
            // `MC` is Red, pending `...` is Yellow, so LightYellow is the
            // only amber tone left that stays distinct from all of them.
            // A row that is both CI-failing and locally unclean renders
            // `fail !cl` with clearly separate foregrounds.
            badge_worktree_unclean: Color::LightYellow,
            // Both `!pushed` and `!pulled` share Magenta because
            // divergence in either direction is one semantic category;
            // the chip label carries the direction. Distinctness is
            // preserved against `!cl` (LightYellow) and `!merge` /
            // `fail` (Red) on the same row.
            badge_pushed: Color::Magenta,
            // Magenta matches `!pushed` intentionally - both chips
            // signal upstream divergence as a single category - and
            // stays distinct from the green PR badge, the yellow
            // `!cl` chip, and the red conflict chips.
            badge_pulled: Color::Magenta,
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

    /// Meta-line style for selected list rows. Sets only fg (not bg) so
    /// the List widget's `style_tab_highlight_bg` still owns the row
    /// background. Use this in `ListItem` formatters for the muted
    /// meta/detail line when the row is selected.
    pub fn style_meta_selected(&self) -> Style {
        Style::default().fg(self.meta_selected_fg)
    }

    pub fn style_text(&self) -> Style {
        Style::default().fg(self.text)
    }

    /// Style for click-to-copy UI labels: the `interactive_fg` accent
    /// color plus an UNDERLINE modifier. The underline is the persistent
    /// visual affordance that signals "clickable"; apply this style to
    /// any label that is registered in the `ClickRegistry`.
    pub fn style_interactive(&self) -> Style {
        Style::default()
            .fg(self.interactive_fg)
            .add_modifier(Modifier::UNDERLINED)
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

    pub fn style_badge_worktree_unclean(&self) -> Style {
        Style::default().fg(self.badge_worktree_unclean)
    }

    /// Style for the `!pushed` chip rendered when a worktree's branch is
    /// ahead of its upstream (i.e. has unpushed commits). Pure
    /// foreground style, mirroring the other chip helpers in this
    /// section so callers can compose a row of chips without pulling
    /// in modifiers.
    pub fn style_badge_pushed(&self) -> Style {
        Style::default().fg(self.badge_pushed)
    }

    /// Style for the `!pulled` chip rendered when a worktree's branch is
    /// behind its upstream (the remote moved). The foreground
    /// intentionally matches `!pushed` - both chips signal upstream
    /// divergence as a single category, and the labels (not the
    /// colors) distinguish the two directions.
    pub fn style_badge_pulled(&self) -> Style {
        Style::default().fg(self.badge_pulled)
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

    /// Style for the `[RG]` badge rendered next to a work item's state
    /// badge while the async review gate is running (PR existence -> CI
    /// wait -> adversarial review). Yellow + bold, reusing the `[RV]`
    /// target colour so the visual hint "this item is gating its way
    /// towards Review" is obvious at a glance.
    pub fn style_badge_review_gate(&self) -> Style {
        Style::default()
            .fg(self.badge_review)
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
