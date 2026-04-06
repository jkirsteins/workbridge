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
    /// Subtle borders (e.g., inner list in settings).
    pub border_subtle: Color,

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

    // -- Context bar --
    /// Work-item context bar foreground (title and repo path).
    pub context_fg: Color,
    /// Work-item context bar background.
    pub context_bg: Color,

    // -- Work item groups and badges --
    /// Group header text color (e.g., "TODO (2)").
    pub group_header: Color,
    /// PR badge color (open PR).
    pub badge_pr: Color,
    /// CI passing badge color.
    pub badge_ci_pass: Color,
    /// CI failing badge color.
    pub badge_ci_fail: Color,
    /// CI pending badge color.
    pub badge_ci_pending: Color,
    /// Unlinked item "?" marker color.
    pub unlinked_marker: Color,
    /// Done item text color (muted, completed work is less prominent).
    pub done_item: Color,

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
            border_subtle: Color::Reset,

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

            context_fg: Color::Cyan,
            context_bg: Color::Reset,

            group_header: Color::Cyan,
            badge_pr: Color::Green,
            badge_ci_pass: Color::Green,
            badge_ci_fail: Color::Red,
            badge_ci_pending: Color::Yellow,
            unlinked_marker: Color::Yellow,
            done_item: Color::Reset,

            badge_backlog: Color::Reset,
            badge_planning: Color::Cyan,
            badge_implementing: Color::Green,
            badge_blocked: Color::Red,
            badge_review: Color::Yellow,
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

    pub fn style_border_subtle(&self) -> Style {
        Style::default().fg(self.border_subtle)
    }

    pub fn style_tab_highlight(&self) -> Style {
        Style::default()
            .fg(self.tab_highlight_fg)
            .bg(self.tab_highlight_bg)
            .add_modifier(Modifier::BOLD)
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

    pub fn style_context(&self) -> Style {
        Style::default().fg(self.context_fg).bg(self.context_bg)
    }

    pub fn style_group_header(&self) -> Style {
        Style::default()
            .fg(self.group_header)
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

    pub fn style_unlinked_marker(&self) -> Style {
        Style::default().fg(self.unlinked_marker)
    }

    pub fn style_done_item(&self) -> Style {
        Style::default()
            .fg(self.done_item)
            .add_modifier(Modifier::DIM)
    }

    pub fn style_stage_badge(&self, status: &WorkItemStatus) -> Style {
        let color = match status {
            WorkItemStatus::Backlog => self.badge_backlog,
            WorkItemStatus::Planning => self.badge_planning,
            WorkItemStatus::Implementing => self.badge_implementing,
            WorkItemStatus::Blocked => self.badge_blocked,
            WorkItemStatus::Review => self.badge_review,
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
