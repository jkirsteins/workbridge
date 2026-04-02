# Workbridge Development Rules

## Error Handling

Never silently ignore errors. Every error must be either:
1. Handled (recovered from with a concrete fallback), or
2. Surfaced to the user (status message, stderr, UI indicator)

`unwrap_or_default()` on a Result that could contain a meaningful error is
a bug. If you want to fall back to a default, log or display the error first.

## UI and Color

When choosing colors for TUI elements, consider contrast and readability on
dark terminal backgrounds. DarkGray text on a dark background is unreadable.
Use White or light colors for content the user needs to read. Reserve DarkGray
for truly de-emphasized elements like empty-state placeholders.

This rule applies to text rendered against the terminal's own background
(which we don't control). When we set both foreground AND background (e.g.,
highlight bars, status bars), contrast is guaranteed by the Theme struct and
this rule does not apply - the Theme controls both sides.
