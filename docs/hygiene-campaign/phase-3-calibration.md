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
| `clippy::manual_let_else` | 94 | Mechanical fix (D10). |
| `clippy::too_many_lines` | 90 | Allow; file-size budget is the real enforcement. |
| `clippy::map_unwrap_or` | 78 | Mechanical fix (D7). |
| `clippy::missing_const_for_fn` | 73 | Mechanical fix (D11). |
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
- run: cargo clippy --lib --bins --all-features -- -D warnings
- run: cargo clippy --tests --all-features -- -D warnings \
        -A clippy::unwrap_used -A clippy::expect_used -A clippy::panic
```

No source-level `#[allow(...)]` attribute is introduced to implement
this carve-out. The `clippy::allow_attributes` / `allow_attributes_without_reason`
pair is itself denied at the crate level to prevent regressions.

## Post-cleanup inventory

See "Post-cleanup inventory" section at the end of this document
(added in the final Phase E commit once Phase D was complete).
