//! Shared helpers used across the `ui` submodules: word-wrap, truncation,
//! small geometry helpers, and badge-dim style overrides.
use ratatui_core::buffer::Buffer;
use ratatui_core::layout::{Position, Rect};
use ratatui_core::style::Modifier;
use unicode_width::UnicodeWidthStr;

/// Braille-dot spinner frames for the activity indicator.
/// 10 frames at 200ms per tick = 2-second full rotation.
pub const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Word-wrap a string to fit within `max_width` display columns.
/// Breaks at word boundaries (space, /, -, paren) when possible.
/// Wraps to as many lines as needed - no artificial cap.
/// When `indent` is true, continuation lines are indented with 4 spaces.
/// Every output line is guaranteed to be <= `max_width` display columns.
pub fn wrap_text_impl(s: &str, max_width: usize, indent: bool) -> Vec<String> {
    const INDENT_STR: &str = "    ";
    let indent_width = if indent { INDENT_STR.width() } else { 0 };

    if max_width == 0 {
        return vec![];
    }

    if s.width() <= max_width {
        return vec![s.to_string()];
    }

    let mut lines = Vec::new();
    let mut remaining = s;

    while !remaining.is_empty() {
        // Continuation lines have less space due to indent
        let effective_width = if lines.is_empty() {
            max_width
        } else {
            max_width.saturating_sub(indent_width)
        };

        // Guard: if effective_width is 0 (max_width < indent), force at least 1 char
        let effective_width = effective_width.max(1);

        if remaining.width() <= effective_width {
            lines.push(remaining.to_string());
            break;
        }

        // Find the byte index where cumulative display width reaches effective_width
        let byte_limit = remaining
            .char_indices()
            .scan(0usize, |acc, (i, c)| {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                *acc += w;
                Some((i, *acc))
            })
            .take_while(|&(_, cum_w)| cum_w <= effective_width)
            .last()
            .map_or_else(
                || {
                    // First char is already wider than effective_width; take it anyway
                    remaining.chars().next().map_or(0, char::len_utf8)
                },
                |(i, _)| {
                    // Advance past this char to get the end byte index
                    i + remaining[i..].chars().next().map_or(0, char::len_utf8)
                },
            );

        // Try to break at a word boundary within the limit
        let break_at = remaining[..byte_limit]
            .rfind([' ', '/', '-', '('])
            .map_or(byte_limit, |i| i + 1);

        let (line, rest) = remaining.split_at(break_at);
        lines.push(line.to_string());

        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        remaining = trimmed;
    }

    // Prepend indent to continuation lines, but only if max_width can
    // accommodate it without exceeding the width guarantee.
    if indent && max_width > indent_width {
        for line in lines.iter_mut().skip(1) {
            *line = format!("{INDENT_STR}{line}");
        }
    }

    lines
}

/// Word-wrap with 4-space continuation indent (default behavior).
pub fn wrap_text(s: &str, max_width: usize) -> Vec<String> {
    wrap_text_impl(s, max_width, true)
}

/// Word-wrap with no continuation indent.
pub fn wrap_text_flat(s: &str, max_width: usize) -> Vec<String> {
    wrap_text_impl(s, max_width, false)
}

/// Word-wrap where the first line has a narrower budget than subsequent lines.
/// Used for titles where line 1 shares space with badge + right badges.
pub fn wrap_two_widths(s: &str, first_width: usize, rest_width: usize) -> Vec<String> {
    if first_width == 0 || s.is_empty() {
        return vec![];
    }
    // If it fits on the first line, done.
    if s.width() <= first_width {
        return vec![s.to_string()];
    }
    // Break the first line at first_width.
    let first_lines = wrap_text_flat(s, first_width);
    let first = first_lines[0].clone();
    // Reconstruct the remainder from the original string.
    let used_bytes = first.trim_end().len();
    let rest = s[used_bytes..].trim_start();
    if rest.is_empty() {
        return vec![first];
    }
    let mut lines = vec![first];
    lines.extend(wrap_text_flat(rest, rest_width));
    lines
}

/// Truncate a string to fit within `max_len` display columns.
/// If truncated, appends "..".
pub fn truncate_str(s: &str, max_len: usize) -> String {
    if s.width() <= max_len {
        s.to_string()
    } else if max_len <= 2 {
        truncate_to_width(s, max_len)
    } else {
        let mut result = truncate_to_width(s, max_len - 2);
        result.push_str("..");
        result
    }
}

/// Take chars from `s` until their cumulative display width reaches `max_cols`.
pub fn truncate_to_width(s: &str, max_cols: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > max_cols {
            break;
        }
        width += cw;
        result.push(c);
    }
    result
}

/// Return a centered rect using the given percentage of the outer rect.
pub const fn centered_rect(percent_x: u16, percent_y: u16, outer: Rect) -> Rect {
    let popup_width = outer.width * percent_x / 100;
    let popup_height = outer.height * percent_y / 100;
    let x = outer.x + (outer.width.saturating_sub(popup_width)) / 2;
    let y = outer.y + (outer.height.saturating_sub(popup_height)) / 2;
    Rect::new(x, y, popup_width, popup_height)
}

/// Configure rat-text to render the cursor into the ratatui `Buffer`
/// instead of driving the terminal cursor. Called from the text-style
/// helper so it is applied before the first dialog render - after that
/// the atomic store is a no-op. Keeps tests deterministic (the
/// `TestBackend` does not have a real terminal cursor).
pub fn ensure_rendered_cursor() {
    use rat_widget::text::cursor::{CursorType, set_cursor_type};
    set_cursor_type(CursorType::RenderedCursor);
}

/// Return a centered rect with fixed width and height within the outer rect.
pub fn centered_rect_fixed(width: u16, height: u16, outer: Rect) -> Rect {
    let w = width.min(outer.width);
    let h = height.min(outer.height);
    let x = outer.x + (outer.width.saturating_sub(w)) / 2;
    let y = outer.y + (outer.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Dim every cell in the buffer area to visually push content behind an overlay.
///
/// Applies `Modifier::DIM` and overrides foreground to `Color::DarkGray`. The
/// dual approach is necessary because DIM alone does not reliably dim borders
/// and colored elements on all terminals.
pub fn dim_background(buf: &mut Buffer, area: Rect) {
    let dim_fg = ratatui_core::style::Color::DarkGray;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                let style = cell.style().add_modifier(Modifier::DIM).fg(dim_fg);
                cell.set_style(style);
            }
        }
    }
}

/// Apply the "no active session" dim treatment to a single badge style.
///
/// Mirrors `dim_background`'s approach at the style level: adds
/// `Modifier::DIM` and forces foreground to `Color::DarkGray`. The
/// `DarkGray` override is load-bearing - on some terminals DIM alone
/// is indistinguishable from normal, and `DarkGray` collapses all
/// per-badge hues to a single neutral so the dim reads consistently
/// across themes.
///
/// Returns `style` unchanged when `has_session` is true, so callers
/// can wrap every badge style unconditionally without branching.
pub const fn dim_badge_style(
    style: ratatui_core::style::Style,
    has_session: bool,
) -> ratatui_core::style::Style {
    if has_session {
        style
    } else {
        style
            .add_modifier(Modifier::DIM)
            .fg(ratatui_core::style::Color::DarkGray)
    }
}

#[cfg(test)]
mod wrap_tests {
    use super::wrap_text;

    /// Every output line must fit within `max_width` (measured in display columns).
    fn assert_all_lines_fit(lines: &[String], max_width: usize) {
        use unicode_width::UnicodeWidthStr;
        for (i, line) in lines.iter().enumerate() {
            let display_width = line.width();
            assert!(
                display_width <= max_width,
                "line {i} is {display_width} cols but max_width is {max_width}: {line:?}",
            );
        }
    }

    #[test]
    fn short_string_no_wrap() {
        let result = wrap_text("hello", 20);
        assert_eq!(result, vec!["hello"]);
        assert_all_lines_fit(&result, 20);
    }

    #[test]
    fn exact_fit_no_wrap() {
        let s = "exactly twenty chars";
        assert_eq!(s.len(), 20);
        let result = wrap_text(s, 20);
        assert_eq!(result.len(), 1);
        assert_all_lines_fit(&result, 20);
    }

    #[test]
    fn wraps_at_word_boundary() {
        let result = wrap_text("  hello world foo bar", 14);
        assert_all_lines_fit(&result, 14);
        assert!(result.len() >= 2, "should wrap: {result:?}");
    }

    #[test]
    fn wraps_at_slash_boundary() {
        // Simulates a branch like "janiskirsteins/agent-specific-labels"
        let result = wrap_text("  janiskirsteins/agent-specific-labels (walleyboard)", 25);
        assert_all_lines_fit(&result, 25);
        assert!(result.len() >= 2, "should wrap: {result:?}");
    }

    #[test]
    fn continuation_lines_indented() {
        let result = wrap_text("  long content that must wrap to next line", 20);
        assert_all_lines_fit(&result, 20);
        if result.len() > 1 {
            assert!(
                result[1].starts_with("    "),
                "continuation should be indented: {:?}",
                result[1]
            );
        }
    }

    #[test]
    fn all_content_preserved() {
        let input = "  [no branch] (workbridge) [no wt]";
        let result = wrap_text(input, 20);
        assert_all_lines_fit(&result, 20);
        // All key content words must appear somewhere in the output.
        // Words may be split across lines by the wrapper.
        let flat: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["no", "branch", "workbridge", "wt"] {
            assert!(flat.contains(word), "missing '{word}': {flat}");
        }
    }

    #[test]
    fn very_narrow_width() {
        let result = wrap_text("  hello world", 8);
        assert_all_lines_fit(&result, 8);
        assert!(!result.is_empty());
    }

    #[test]
    fn empty_string() {
        let result = wrap_text("", 20);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn zero_width() {
        let result = wrap_text("hello", 0);
        assert!(result.is_empty());
    }

    #[test]
    fn realistic_workitem_narrow_panel() {
        // 23 chars inner width (100 col terminal, 25% left panel)
        let input = "  [no branch] (workbridge) [no wt]";
        let result = wrap_text(input, 23);
        assert_all_lines_fit(&result, 23);
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("[no wt]"), "must not clip [no wt]");
    }

    #[test]
    fn realistic_branch_narrow_panel() {
        let input = "  janiskirsteins/agent-specific-labels (walleyboard)";
        let result = wrap_text(input, 23);
        assert_all_lines_fit(&result, 23);
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("walleyboard"), "must not clip repo name");
    }

    #[test]
    fn multibyte_utf8_no_panic() {
        // Accented chars (2 bytes each in UTF-8), must not panic on slice
        let input = "  feature/korrektur-andern-loschen (projekt)";
        let result = wrap_text(input, 20);
        assert_all_lines_fit(&result, 20);
        assert!(!result.is_empty());
        let joined: String = result
            .iter()
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("projekt"), "must preserve repo name");
    }

    #[test]
    fn wide_cjk_characters_respect_display_width() {
        use unicode_width::UnicodeWidthStr;
        // CJK ideographs: each is 2 display columns, 1 char, 3 bytes
        // \u{4e16}\u{754c} = 2 chars, 4 display columns
        let input = "  \u{4e16}\u{754c}/test (repo)";
        assert!(
            input.width() > input.chars().count(),
            "CJK should be wider than char count: width={}, chars={}",
            input.width(),
            input.chars().count()
        );
        let result = wrap_text(input, 12);
        assert_all_lines_fit(&result, 12);
        assert!(!result.is_empty());
    }

    #[test]
    fn emoji_display_width() {
        // \u{1f600} = grinning face, 2 display columns, 1 char, 4 bytes
        let input = "  fix/\u{1f600}bug (my-repo)";
        let result = wrap_text(input, 14);
        assert_all_lines_fit(&result, 14);
        assert!(!result.is_empty());
    }
}

#[cfg(test)]
mod wrap_variant_tests {
    use unicode_width::UnicodeWidthStr;

    use super::{wrap_text_flat, wrap_two_widths};

    #[test]
    fn wrap_text_flat_no_indent_on_continuation() {
        let result = wrap_text_flat("hello world foo bar baz", 12);
        assert_eq!(result[0], "hello world ");
        // Continuation has no leading spaces
        assert!(!result[1].starts_with(' '));
    }

    #[test]
    fn wrap_two_widths_first_line_narrow() {
        // First line budget 10, rest budget 25
        let result = wrap_two_widths(
            "Add Kanban board view with column-based work item organization",
            10,
            25,
        );
        // First line fits within 10 columns
        assert!(
            result[0].width() <= 10,
            "first line too wide: {:?}",
            result[0]
        );
        // Continuation lines use the wider budget
        for line in result.iter().skip(1) {
            assert!(line.width() <= 25, "continuation too wide: {line:?}");
        }
        // All words present
        let joined: String = result.join(" ");
        assert!(joined.contains("organization"));
    }

    #[test]
    fn wrap_two_widths_fits_first_line() {
        let result = wrap_two_widths("Short title", 20, 40);
        assert_eq!(result, vec!["Short title"]);
    }

    #[test]
    fn wrap_two_widths_empty() {
        let result = wrap_two_widths("", 10, 20);
        assert!(result.is_empty());
    }
}
