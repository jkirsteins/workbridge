# Contributing to Workbridge

## Getting Started

Before contributing, enable the git hooks:

```sh
git config core.hooksPath hooks
```

This enforces formatting and lint checks on commit, and runs tests on push.

## Linting and Formatting

Never suppress, ignore, or work around linter or formatter errors. If clippy
or `cargo fmt --check` complains, fix the code - do not add `#[allow(...)]`,
`// nolint`, or similar annotations to silence warnings.

### Optional: Claude Code post-edit `cargo fmt` hook

If you use Claude Code, `.claude/settings.json` configures a PostToolUse
hook that runs `cargo fmt` on each `.rs` file as Claude writes it. The
hook shells out to `jq` to parse the tool-input payload, so it requires
`jq` on `PATH` (`brew install jq` on macOS, `apt install jq` on Debian/
Ubuntu). If `jq` is missing the hook silently skips formatting; the
pre-commit hook in `hooks/` catches anything the convenience hook missed,
so contributors without Claude Code (or without `jq`) are not affected
in CI or at commit time.

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
