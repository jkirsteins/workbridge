#!/usr/bin/env bash
# hooks/clippy-check.sh
#
# Runs the project's two-invocation clippy pattern:
#   1. Production code (`--bins`) is linted with every lint in
#      `Cargo.toml` `[lints]` including the deny'd restriction lints
#      (`unwrap_used`, `expect_used`, `panic`).
#   2. Tests (`--tests`) are linted with the same `[lints]` matrix,
#      but the three restriction lints above are allowed at the CLI
#      (via `-A`) because tests use those constructs idiomatically.
#
# Both invocations promote warnings to errors via `-D warnings`.
#
# Called from BOTH `hooks/pre-commit` and `.github/workflows/ci.yml`
# so the `-A` flag set lives in exactly one place. Adding a new
# test-only carve-out, or flipping a structural lint back to warn,
# is a one-line edit here instead of coordinated edits across the
# hook, CI, and CONTRIBUTING.md.
#
# `--bins` (no `--lib`) because workbridge has no library target
# today. If a library target is ever added, change to `--lib --bins`
# here and this is the only place the change needs to land.

set -euo pipefail

cargo clippy --bins --all-features -- -D warnings
cargo clippy --tests --all-features -- \
    -D warnings \
    -A clippy::unwrap_used \
    -A clippy::expect_used \
    -A clippy::panic
