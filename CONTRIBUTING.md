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

### Optional local tools

The pre-commit and pre-push hooks call a few third-party cargo tools.
They are optional locally (the hooks skip with an install hint) but
CI runs them as hard gates. To match CI locally:

```sh
cargo install cargo-audit cargo-deny cargo-machete typos-cli
```

### File-size budgets

`ci/file-size-budgets.toml` declares a maximum line count per source
file. `hooks/budget-check.sh` enforces it (locally via pre-commit and
in CI via the `budget` job). If you legitimately need a file to grow
past its budget, bump the entry in the budget file as part of your
PR and explain the growth in the commit message. The budget exists
to prevent silent module bloat, not to ban growth - it wants the
growth to be an explicit, reviewable decision.

## Error Handling

Never silently ignore errors. Every error must be either:
1. Handled (recovered from with a concrete fallback), or
2. Surfaced to the user (status message, stderr, UI indicator)

`unwrap_or_default()` on a Result that could contain a meaningful error is
a bug. If you want to fall back to a default, log or display the error first.

## Changelog

Workbridge keeps a `CHANGELOG.md` in [Keep a Changelog](https://keepachangelog.com)
format. When you open a PR, add a bullet under `## [Unreleased]`
describing the user-visible change in one line. Tag the bullet with the
PR number:

    - Fix worktree cleanup when the upstream branch is gone ([#123](https://github.com/jkirsteins/workbridge/pull/123))

Internal-only changes (refactors, test-only work, doc-only tweaks) do
not need an entry.

Never rename or remove the `## [Unreleased]` heading - the release
tooling relies on it matching the literal string `## [Unreleased]` so
the pre-release replacement can fire. Renaming it causes `cargo release`
to abort with `min = 1` not met.

## UI and Color

When choosing colors for TUI elements, consider contrast and readability on
dark terminal backgrounds. DarkGray text on a dark background is unreadable.
Use White or light colors for content the user needs to read. Reserve DarkGray
for truly de-emphasized elements like empty-state placeholders.

This rule applies to text rendered against the terminal's own background
(which we don't control). When we set both foreground AND background (e.g.,
highlight bars, status bars), contrast is guaranteed by the Theme struct and
this rule does not apply - the Theme controls both sides.
