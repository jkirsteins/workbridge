# Phase 3 Clippy Calibration

This document is the baseline lint-category inventory captured before
Phase 3 of the hygiene campaign flipped clippy's pedantic + nursery
groups from implicit `allow` to `warn` (promoted to error via CI's
`-D warnings`) and denied the `unwrap_used` / `expect_used` / `panic`
restriction lints outside tests.

The goal is two-fold:

1. Give reviewers of the Phase 3 PR a concrete picture of where the
   findings clustered so the mechanical-vs-structural split is
   auditable.
2. Leave future hygiene campaigns a "then vs now" comparison point so
   they can see which categories regressed and which stayed clean.

## Raw source

The lint-count data below was produced by running

```sh
cargo clippy \
    --all-targets --all-features --message-format=json \
    -- \
    -W clippy::pedantic -W clippy::nursery \
    -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic \
    -W clippy::allow_attributes -W clippy::allow_attributes_without_reason \
    > /tmp/workbridge-p3-clippy.json
```

against the tip of `master` on 2026-04-21 (commit `c5d56b2`), then
post-processing the JSON output to aggregate by lint name and by
`(lint, file)` pair. The raw JSON is not committed to the repo.

## Top-level totals

- Distinct lints triggered: 67
- Total raw findings: 3094

## Lint-count table (descending)

| Lint | Count | Disposition |
|---|--:|---|
| `clippy::unwrap_used` | 644 | Deny in prod; tests carved out via two-invocation CI pattern. |
| `clippy::doc_markdown` | 625 | Mechanical fix (D13). |
| `clippy::expect_used` | 175 | Deny in prod; tests carved out. |
| `clippy::use_self` | 142 | Mechanical fix (D12). |
| `clippy::redundant_clone` | 133 | Mechanical fix (D9). |
| `clippy::print_stderr` | 97 | Allow; CLI surface in `main.rs`. |
| `clippy::manual_let_else` | 94 | Mechanical fix (D10); every site converted to `let ... else`. |
| `clippy::too_many_lines` | 90 | Allow; file-size budget is the real enforcement. |
| `clippy::map_unwrap_or` | 78 | Mechanical fix (D7). |
| `clippy::missing_const_for_fn` | 73 | Mechanical fix (D11); two test-only fns use `#[cfg_attr(test, expect(...))]`. |
| `clippy::redundant_closure_for_method_calls` | 72 | Mechanical fix (D8). |
| `clippy::exit` | 66 | Allow; CLI surface in `main.rs`. |
| `clippy::cast_possible_truncation` | 61 | Allow; TUI u16 width/height math. |
| `clippy::print_stdout` | 56 | Allow; CLI surface in `main.rs`. |
| `clippy::single_match_else` | 54 | Mechanical fix (D5). |
| `clippy::uninlined_format_args` | 51 | Mechanical fix (D1). |
| `clippy::panic` | 48 | Deny in prod; tests carved out. |
| `clippy::option_if_let_else` | 45 | Mechanical fix (D6). |
| `clippy::needless_pass_by_value` | 45 | Allow (Phase 4 will flip). |
| `clippy::items_after_statements` | 36 | Mechanical fix (D2). |
| `clippy::cast_possible_wrap` | 32 | Allow; TUI cast math. |
| `clippy::allow_attributes_without_reason` | 28 | Fixed in Phase C (source allows removed entirely). |
| `clippy::allow_attributes` | 28 | Fixed in Phase C (source allows removed entirely). |
| `clippy::match_same_arms` | 26 | Mechanical fix (D3). |
| `clippy::used_underscore_binding` | 23 | Mechanical fix (D15 tail). |
| `clippy::significant_drop_tightening` | 21 | Allow (Phase 4 will flip). |
| `clippy::redundant_pub_crate` | 18 | Mechanical fix (D15 tail). |
| `clippy::cast_lossless` | 18 | Allow; TUI cast math. |
| `clippy::trivially_copy_pass_by_ref` | 16 | Mechanical fix (D15 tail). |
| `clippy::must_use_candidate` | 16 | Mechanical fix (D15 tail). |
| `clippy::similar_names` | 13 | Allow; false-positive-heavy. |
| `clippy::or_fun_call` | 12 | Mechanical fix (D15 tail). |
| `clippy::if_not_else` | 10 | Mechanical fix (D15). |
| `clippy::explicit_iter_loop` | 10 | Mechanical fix (D15). |
| `clippy::cast_sign_loss` | 10 | Allow; TUI cast math. |
| `clippy::unused_self` | 8 | Allow (Phase 4 will flip). |
| `clippy::bool_to_int_with_if` | 8 | Mechanical fix (D4). |
| `clippy::unnested_or_patterns` | 6 | Mechanical fix (D15). |
| `clippy::unnecessary_trailing_comma` | 6 | Mechanical fix (D15). |
| `clippy::unnecessary_semicolon` | 6 | Mechanical fix (D15). |
| `clippy::needless_pass_by_ref_mut` | 6 | Mechanical fix (D15 tail). |
| `clippy::needless_continue` | 6 | Mechanical fix (D15). |
| `clippy::match_wildcard_for_single_variants` | 6 | Mechanical fix (D15). |
| `clippy::derive_partial_eq_without_eq` | 6 | Mechanical fix (D15). |
| `clippy::too_long_first_doc_paragraph` | 5 | Mechanical fix (D15). |
| `clippy::format_push_string` | 5 | Mechanical fix (D15). |
| `clippy::case_sensitive_file_extension_comparisons` | 5 | Mechanical fix (D15). |
| `clippy::unreadable_literal` | 4 | Mechanical fix (D15). |
| `clippy::unnecessary_wraps` | 4 | Mechanical fix (D15). |
| `clippy::semicolon_if_nothing_returned` | 4 | Mechanical fix (D15). |
| `clippy::ptr_as_ptr` | 4 | Mechanical fix (D15). |
| `clippy::option_as_ref_cloned` | 4 | Mechanical fix (D15). |
| `clippy::borrow_as_ptr` | 4 | Mechanical fix (D15). |
| `clippy::assigning_clones` | 4 | Mechanical fix (D15). |
| `clippy::single_char_pattern` | 3 | Mechanical fix (D15). |
| `clippy::needless_collect` | 3 | Mechanical fix (D15). |
| `clippy::manual_string_new` | 3 | Mechanical fix (D15). |
| `clippy::struct_excessive_bools` | 2 | Allow (Phase 4 will flip). |
| `clippy::ref_option` | 2 | Mechanical fix (D15 tail). |
| `clippy::redundant_else` | 2 | Mechanical fix (D15). |
| `clippy::needless_raw_string_hashes` | 2 | Mechanical fix (D15). |
| `clippy::implicit_clone` | 2 | Mechanical fix (D15). |
| `clippy::default_trait_access` | 2 | Mechanical fix (D15). |
| `clippy::comparison_chain` | 2 | Mechanical fix (D15). |
| `clippy::cast_precision_loss` | 2 | Allow; TUI cast math. |
| `clippy::should_panic_without_expect` | 1 | Mechanical fix (D15 tail). |
| `clippy::missing_panics_doc` | 1 | Allow (noise per design doc). |

## Mechanical vs structural classification

Phase 3 cleans up the mechanical findings (items the compiler can
rewrite under a local transformation) and punts structural findings
(items whose cleanup requires a design change) to Phase 4.

### Mechanical (fixed in this PR)

All of the "Mechanical fix" rows above. Concretely: format-string
inlining, `map_or` / `map_or_else` combinator rewrites, `let ... else`
conversions, backticking of rustdoc identifiers, `Self` substitutions,
`const fn` additions where applicable, etc. Each category is a local
rewrite; none requires restructuring a module's public API.

### Structural (deferred to Phase 4)

- `clippy::too_many_lines` - the only file at fault is `src/app.rs`
  (~26 kloc). The fix is the Phase 4 decomposition that splits `app.rs`
  into subsystem modules. Allow crate-wide; the file-size budget
  enforces the real ceiling.
- `clippy::needless_pass_by_value` - most sites are App methods that
  take a `WorktreeRepo` (or similar larger struct) by value because
  the TUI event loop holds `&mut App` at the call site. Fixing the
  signatures requires threading borrows through the event-dispatch
  layer; a Phase 4 subgoal.
- `clippy::significant_drop_tightening` - all sites are in `app.rs`
  where a `MutexGuard` outlives a statement. The cleanup is part of
  the same `app.rs` decomposition.
- `clippy::struct_excessive_bools` - the `App` struct itself and one
  view-state struct fail this. Decomposition will split `App` into
  smaller structs; the lint will pass naturally.
- `clippy::unused_self` - 8 sites in `app.rs` where a helper takes
  `&self` for uniformity with related methods. Decomposition moves
  those helpers into free functions or different impl blocks; waiting
  avoids churn.

### Design-doc noise (allowed crate-wide per the hygiene design doc)

- `clippy::module_name_repetitions` - the crate names follow
  `app::AppEvent`, `session::Session`, etc. by design. Allow.
- `clippy::missing_errors_doc` / `clippy::missing_panics_doc` - the
  crate has no published rustdoc contract; these would be pure noise.
  Allow.
- `clippy::similar_names` - fires heavily on short TUI variable
  names (`x` / `y`, `row` / `col`, `i` / `j`). Allow.
- `clippy::cast_possible_truncation` / `cast_possible_wrap` /
  `cast_sign_loss` / `cast_lossless` / `cast_precision_loss` - the TUI
  math is almost entirely `u16` width / height arithmetic coming from
  ratatui's `Rect`. Allow.

### CLI-surface allows

`clippy::print_stdout` / `clippy::print_stderr` / `clippy::exit` are
allowed crate-wide because `src/main.rs` is the CLI surface. Non-CLI
code routes user-visible output through the toast / status-bar
subsystem; that rule is enforced by review, not by the lint.

### Restriction lints with test carve-outs

`clippy::unwrap_used` / `clippy::expect_used` / `clippy::panic` are
`deny` for production code and allowed for tests. The carve-out is
implemented via CI's two-invocation pattern:

```yaml
- run: cargo clippy --bins --all-features -- -D warnings
- run: cargo clippy --tests --all-features -- -D warnings \
        -A clippy::unwrap_used -A clippy::expect_used -A clippy::panic
```

No source-level `#[allow(...)]` attribute is introduced to implement
this carve-out. The `clippy::allow_attributes` / `allow_attributes_without_reason`
pair is itself denied at the crate level to prevent regressions.

## Post-cleanup inventory

After Phase D finished, the final `[lints]` matrix landed in
`Cargo.toml` and both clippy invocations exit 0:

```sh
cargo clippy --bins --all-features -- -D warnings
cargo clippy --tests --all-features -- -D warnings \
    -A clippy::unwrap_used -A clippy::expect_used -A clippy::panic
```

The final lint-matrix disposition:

### Deny (P1 hygiene, zero findings)

- `clippy::dbg_macro`
- `clippy::todo`
- `clippy::unimplemented`
- `clippy::allow_attributes`
- `clippy::allow_attributes_without_reason`
- `rustdoc::broken_intra_doc_links`

### Deny (production restriction lints, zero findings)

- `clippy::unwrap_used`
- `clippy::expect_used`
- `clippy::panic`

Tests are permitted to use these via CI's two-invocation pattern
(`-A clippy::unwrap_used -A clippy::expect_used -A clippy::panic` on
the `--tests` invocation). No source-level `#[allow]` attribute is
used to implement the carve-out; the `clippy::allow_attributes`
lint itself denies them.

### Warn / deny (via -D warnings)

- `clippy::pedantic` (group, priority -1)
- `clippy::nursery` (group, priority -1)
- `rust_2018_idioms` (group, priority -1)

### Allow (with rationale in Cargo.toml)

- **CLI surface**: `print_stdout`, `print_stderr`, `exit`.
- **Design-doc noise**: `module_name_repetitions`,
  `missing_errors_doc`, `missing_panics_doc`, `too_many_lines`,
  `similar_names`.
- **TUI cast math**: `cast_possible_truncation`, `cast_possible_wrap`,
  `cast_sign_loss`, `cast_lossless`, `cast_precision_loss`.
- **Phase-4 structural**: `needless_pass_by_value`,
  `significant_drop_tightening`, `struct_excessive_bools`,
  `unused_self`.

### `unsafe_code` at warn, opted out per-site with `#[expect]`

`unsafe_code` is `warn` (promoted to merge-blocker by CI's
`-D warnings`). The two legitimate unsafe surfaces opt out locally
via `#[expect(unsafe_code, reason = "...")]`, not `#[allow]`:

- `src/session.rs`: one file-level
  `#![expect(unsafe_code, reason = "...")]` at the top of the module
  covers the PTY FFI boundary. Every unsafe block inside the file
  still carries a SAFETY comment.
- `src/app.rs`: `#[expect(unsafe_code, reason = "...")]` on each of
  the two enclosing functions (`impl Drop for
  RebaseGateState::drop` and `run_cancellable`) that contain a
  `libc::killpg` call.

`#[expect]` (not `#[allow]`) is mandatory because
`clippy::allow_attributes` is denied crate-wide. The expect
attribute doubles as a regression signal: if a future refactor
removes the unsafe block, the attribute fires
`unfulfilled_lint_expectations` and the refactor also removes the
attribute.

### Other per-site `#[expect(...)]` uses

A small number of pedantic/nursery lints are suppressed at the
specific site rather than crate-wide, because the finding is true
only at that site and a crate-wide allow would over-suppress:

- `src/salsa.rs`: `app_render` and `app_event` carry
  `#[expect(clippy::unnecessary_wraps, reason = "rat-salsa run_tui
  callback contract requires Result<..>")]`. The `Result` signature
  is dictated by the rat-salsa `run_tui` callback contract.
- `src/config.rs::home_dir` and `src/side_effects/clipboard.rs::copy`
  carry `#[cfg_attr(test, expect(clippy::missing_const_for_fn,
  reason = "..."))]`. Under `cfg(test)` these fns reduce to a
  const-able body; under `cfg(not(test))` they call non-const
  helpers and cannot be `const fn`.

The `#[expect]` usage here is narrow-scoped, always has a `reason`
string, and is not a regression of the "no source-level suppression"
goal (the goal was specifically to eliminate `#[allow]`, not to
forbid the new `#[expect]` attribute which Clippy actively
recommends via `allow_attributes`).

### Source-level `#[allow]` attributes removed

Phase C deleted the seven pre-existing source-level `#[allow]`
attributes:

- `src/app.rs:16442` (`clippy::too_many_arguments` x1 -> struct-bundled
  as `ReviewItemState`)
- `src/app.rs:17552` (`clippy::too_many_arguments` x1 -> struct-bundled
  as `LivePrPrecheckSpec`)
- `src/config.rs:13`, `:39`, `:45` (`dead_code` x3 ->
  `InMemoryConfigProvider` moved to `#[cfg(test)] mod test_support`;
  `ConfigProvider::load` allow dropped because the trait method is
  used in production by `main.rs::config_set`)
- `src/salsa.rs:28`, `:37`, `:43` (`dead_code` x3 ->
  `AppEvent::Message(AppMessage)` and the `AppMessage` enum deleted
  along with the dispatcher arm)
- `src/work_item.rs:302`, `:320`, `:331`, `:338` (`dead_code` x4 ->
  `IssueInfo::state` field, `IssueState` enum,
  `WorkItemError::DetachedHead` / `CorruptBackendRecord` /
  `WorktreeGone` variants deleted; renderer and fixture sites
  updated)
- `src/work_item_backend.rs:20`, `:336` (`dead_code` x2 ->
  `BackendError::Parse` variant and `WorkItemBackend::backend_type()`
  trait method deleted, along with the 19 downstream trait impls)
- `src/worktree_service.rs:354` (`dead_code` x1 -> attribute simply
  removed; `find_branch_for_worktree` is actually used by
  `remove_worktree`, clippy misjudged)

After Phase C, `grep -rn '#\[allow\|#!\[allow' src/` returns zero
source-level allow attributes.

### Rework follow-up (2026-04-21)

During initial review, three deviations from Plan step A2 were
flagged and addressed in a follow-up commit:

1. `unsafe_code` was set to `allow`; reverted to `warn` as the plan
   required. Per-site opt-out is via
   `#[expect(unsafe_code, reason = "...")]` (file-level in
   `src/session.rs`; function-level on the two functions in
   `src/app.rs` that contain `libc::killpg`). The original rationale
   for the flip - "no local suppression path because
   `clippy::allow_attributes` is denied" - was wrong: `#[expect]`
   IS the local suppression path, and the `allow_attributes` lint
   actively prefers it.
2. Twelve extra crate-wide `allow` entries that were not in the
   Plan A2 matrix (`missing_const_for_fn`, `unnecessary_wraps`,
   `comparison_chain`, `ref_option`, `used_underscore_binding`,
   `unreadable_literal`, `manual_let_else`, `option_if_let_else`,
   `items_after_statements`, `match_same_arms`, `or_fun_call`,
   `trivially_copy_pass_by_ref`) were removed. The final matrix now
   matches Plan A2 exactly.
3. Phases D2 / D3 / D6 / D10 / D11 were re-run against actual
   findings (with the extra allows removed) and every site fixed
   at source:
   - D2 `items_after_statements`: items hoisted to the top of
     their enclosing block.
   - D3 `match_same_arms`: identical arms merged with `|`.
   - D6 `option_if_let_else`: rewrote to `.map_or` /
     `.map_or_else`.
   - D10 `manual_let_else`: converted every `let x = match y { ...
     diverging ... }` to `let Pattern = y else { ... }`.
   - D11 `missing_const_for_fn`: fixed via
     `#[cfg_attr(test, expect(clippy::missing_const_for_fn,
     reason = "..."))]` at the two test-only sites where
     `cfg(test)` reduces the fn body to a const-able form but
     `cfg(not(test))` does not.

The remaining mechanical-tail lints flagged by the 12-allow removal
(`unnecessary_wraps`, `comparison_chain`, `ref_option`,
`used_underscore_binding`, `unreadable_literal`, `or_fun_call`,
`trivially_copy_pass_by_ref`) were also fixed at source. For the
two rat-salsa callbacks in `src/salsa.rs` whose `Result` signatures
are fixed by the external callback contract,
`#[expect(clippy::unnecessary_wraps, reason = "...")]` is attached
at the site.

After the rework, `grep -rn '#\[allow\|#!\[allow' src/` still
returns zero hits. All local lint suppressions use `#[expect(...,
reason = "...")]`, which is the form `clippy::allow_attributes`
recommends.

## Calibration artifact provenance

The raw lint counts in the top of this file were produced on
2026-04-21 against master `c5d56b2`. Future hygiene campaigns can
regenerate a comparable baseline by re-running the command quoted in
"Raw source" above and diffing the JSON output against
`/tmp/workbridge-p3-clippy.json` (not committed; see CONTRIBUTING.md
for how to avoid committing raw artifacts).
