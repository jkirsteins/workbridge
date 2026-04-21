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

### Lints configuration

The `[lints]` table in `Cargo.toml` is the single source of truth for which
clippy lint groups are enabled and which individual lints are allowed crate
wide. The Phase 3 hygiene campaign landed the current matrix; a quick
summary:

- **Deny (P1 hygiene):** `dbg_macro`, `todo`, `unimplemented`,
  `allow_attributes`, `allow_attributes_without_reason`,
  `broken_intra_doc_links`.
- **Deny (production restriction lints):** `unwrap_used`, `expect_used`,
  `panic`. Tests carve these out via the two-invocation clippy pattern
  implemented in `hooks/clippy-check.sh` (called from both pre-commit
  and CI), rather than any source-level `#[allow]`. The script is the
  single source of truth for the `-A` carve-out flag set; updating
  the carve-out requires editing exactly one file.
- **Warn (groups):** `rust_2018_idioms` (from `[lints.rust]`), `pedantic`,
  `nursery` (both from `[lints.clippy]`). CI promotes warnings to errors
  via `-D warnings`.
- **Allow (with rationale in Cargo.toml):** CLI surface (`print_stdout`,
  `print_stderr`, `exit`), design-doc noise (`module_name_repetitions`,
  `missing_errors_doc`, `missing_panics_doc`, `too_many_lines`,
  `similar_names`), TUI cast math (`cast_possible_truncation`,
  `cast_possible_wrap`, `cast_sign_loss`, `cast_lossless`,
  `cast_precision_loss`), and Phase-4-deferred structural lints
  (`needless_pass_by_value`, `significant_drop_tightening`,
  `struct_excessive_bools`, `unused_self`). Every `allow` has a
  one-line rationale comment in `Cargo.toml`; any new allow entry
  needs the same.

**Do not add source-level `#[allow(...)]`**. The
`clippy::allow_attributes_without_reason` lint denies them, and the
`clippy::allow_attributes` lint denies the shorter `#[allow(...)]` form
too. If a specific site really does need a suppression, add it to
`[lints]` in `Cargo.toml` with a rationale comment and propose the
diff in the PR so a reviewer can evaluate whether the category
genuinely warrants a crate-wide allow.

### Unsafe code policy

`unsafe_code` is at `warn` crate-wide (promoted to a merge-blocker
by CI's `-D warnings`) rather than `forbid` because the crate has
two legitimate unsafe surfaces that cannot be rewritten in safe
Rust:

- `src/session.rs` - PTY FFI (`libc::openpty`, `libc::dup`,
  `libc::fcntl`, `libc::read`/`libc::write`, raw-fd construction)
  for the embedded terminal backend. Covered by unit and integration
  tests. The module opts out via a single file-level
  `#![expect(unsafe_code, reason = "...")]` attribute at the top of
  `src/session.rs` (the entire file is the FFI boundary). The
  file-level `#![expect]` suppresses the `unsafe_code` lint across
  the whole module; it does NOT relieve the per-block SAFETY comment
  requirement described below. Every `unsafe { ... }` block still
  needs its own preceding SAFETY comment, and reviewers must flag
  any new block that lacks one even when the file-level attribute
  would otherwise silence the lint.
- `src/app.rs` - two `libc::killpg(pid, SIGKILL)` blocks: one in the
  rebase-gate drop path (`impl Drop for RebaseGateState`) and one in
  the subprocess cancellation helper (`run_cancellable`). Each
  enclosing function opts out via `#[expect(unsafe_code, reason =
  "...")]` attached to the function, not to the unsafe block itself.

Note that the opt-out uses `#[expect]`, NOT `#[allow]`, because
`clippy::allow_attributes` is denied crate-wide so that no
undocumented `#[allow(...)]` can sneak in. `#[expect]` is the
idiomatic replacement: it behaves like allow but produces its own
warning (`unfulfilled_lint_expectations`) if the lint would NOT have
fired, so a future refactor that removes the unsafe block also
removes the attribute.

Every existing unsafe block carries a preceding SAFETY comment
documenting why the block is sound. Adding a new unsafe block
requires:

1. A SAFETY comment that states the preconditions relied on and why
   they hold at the call site.
2. An `#[expect(unsafe_code, reason = "...")]` attribute on the
   enclosing function (or a file-level `#![expect(...)]` for a new
   FFI-boundary module), with a reason string that links to the
   SAFETY comment and names the FFI surface.
3. A reviewer-visible justification in the PR description explaining
   why the operation cannot be expressed in safe Rust.
4. Matching test coverage, or an explicit note in the PR stating why
   the block is impossible to test in-process (e.g. the FFI path
   needs a real PTY device).

Reviewers must flag any new unsafe block that lacks any of the
above. The `#[expect]` attribute is what lets the `unsafe_code`
warning stay at merge-blocker severity for every other site while
still permitting these two carefully-bounded exceptions.

`forbid` is not an option because even a single legitimate unsafe
block in the crate would then force suppression at the site, and
`forbid` cannot be locally suppressed at all. `warn` plus per-site
`#[expect]` is the enforcement path.

### Optional: nightly rustfmt for import style

`rustfmt.toml` enables two nightly-only options
(`imports_granularity = "Module"`, `group_imports = "StdExternalCrate"`).
Stable rustfmt parses these but silently ignores them, so the stable
`cargo fmt --check` gate used by pre-commit and CI will pass on code
that violates them. Drift prevention lives in `CLAUDE.md`: reviewers
run nightly `fmt --check` and flag any diff.

To preview what the reviewer will flag, install nightly rustfmt:

```sh
rustup toolchain install nightly --component rustfmt
cargo +nightly fmt --all -- --check
```

This is optional; CI does not require nightly.

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

#### Adding another tool that internally runs git

If you add a new pre-commit or pre-push step that shells out to a
cargo tool (or any tool) whose work involves running `git` as a
subprocess - think `cargo-deny`'s advisory-db update, `cargo-audit`'s
RustSec sync, or any future tool that maintains a local git-backed
database - wrap that call in a subshell that unsets the inherited git
env vars:

```sh
(
    unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR
    cargo some-tool check
)
```

Otherwise the child `git` calls inherit `GIT_DIR` / `GIT_INDEX_FILE`
from the `git commit` (or `git push`) process that invoked the hook,
and any `git fetch` / `git reset` the tool runs against its own
database silently operates on THIS worktree instead. The symptoms
range from empty commit trees (staged changes vanish) to
`fatal: cannot lock ref 'HEAD'` at commit time, and they are hard
to diagnose because the failure is in a subprocess, not in your
code. The existing `cargo deny` call in `hooks/pre-commit` and the
top-of-hook unset in `hooks/pre-push` are the reference patterns;
copy whichever fits the surrounding structure.

### File-size budget

`hooks/budget-check.sh` enforces a uniform 700-line ceiling on every
tracked `src/**/*.rs` file (at any nesting depth). It runs locally
via pre-commit and in CI via the `budget` job. The ceiling has no
per-file exception mechanism by design - there is no config file to
bump, no annotation to add, no flag to flip. The only legitimate
response to an over-budget file is to decompose it into logical
submodules.

The uniform ceiling exists because prior experience with a per-file
budget config showed the exception list grew without bound, the
largest file kept growing past every "temporary" bump, and review
quality degraded. A hard ceiling with no escape hatch forces the
structural fix. See the `[ABSOLUTE]` rule in CLAUDE.md that bans
reintroducing any size-exception mechanism.

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
