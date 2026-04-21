#!/usr/bin/env bash
# hooks/budget-check.sh
#
# Enforces a uniform 700-line ceiling on every tracked `src/**/*.rs`
# file (including nested paths). Used by both hooks/pre-commit and the
# CI budget job.
#
# Exit 0 if every tracked `src/**/*.rs` file is at or below 700 lines.
# Exit 1 (with a human-readable error) if any file is over the ceiling.
#
# This hook is intentionally simple: no TOML parsing, no per-file
# exception mechanism, no escape hatch. The only legitimate response
# to an over-budget file is to decompose it into logical submodules.
#
# Line-count source: the hook reads staged index content via
# `git show ":$path"` when a blob exists, and falls back to the
# working tree otherwise. Reading the index (instead of the working
# tree) matches the contract documented in the header of
# hooks/pre-commit: a contributor can `git add` a large file and then
# revert the working tree before `git commit`, so the commit bakes
# in the index content. Validating the working tree would let a
# stage-then-edit-away bypass slip through. The CI `budget` job runs
# against the checked-out tree, where the index-blob read degrades
# to the working-tree read (both are identical on a fresh checkout),
# so CI behavior is unchanged.
set -euo pipefail

CEILING=700

# Read the line count of a path, preferring the staged index blob
# over the working tree. Prints the count on stdout. Prints nothing
# and returns nonzero if neither the index nor the working tree has
# the file.
#
# We check for blob existence with `git cat-file -e :$path` before
# piping `git show` into `wc -l`. An earlier implementation captured
# the blob via `blob=$(git show ...)` and then piped `printf '%s'
# "$blob" | wc -l`, but bash command substitution strips trailing
# newlines, so a 701-line file was reported as 700 and slipped past
# the ceiling. Using `cat-file -e` as the existence probe and piping
# `git show` straight into `wc -l` preserves the true line count.
line_count_for() {
    local path="$1"
    if git cat-file -e ":$path" 2>/dev/null; then
        git show ":$path" | wc -l | tr -d ' '
        return 0
    fi
    if [ -f "$path" ]; then
        wc -l < "$path" | tr -d ' '
        return 0
    fi
    return 1
}

fail=0
# `git ls-files 'src/**/*.rs' 'src/*.rs'` enumerates every tracked
# Rust source file under src/, at any nesting depth. We check the
# uniform 700-line ceiling against each one; no path is exempt.
while IFS= read -r tracked; do
    [ -z "$tracked" ] && continue
    if ! actual=$(line_count_for "$tracked"); then
        continue
    fi
    if [ "$actual" -gt "$CEILING" ]; then
        echo "OVER BUDGET ($CEILING lines): $tracked has $actual lines."
        fail=1
    fi
done < <(git ls-files 'src/**/*.rs' 'src/*.rs' 2>/dev/null | sort -u)

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "One or more files exceed the uniform $CEILING-line ceiling."
    echo "Decompose the file into logical submodules. The ceiling has"
    echo "no per-file exception mechanism by design - see the"
    echo "[ABSOLUTE] rule in CLAUDE.md about size-exception mechanisms."
    exit 1
fi

echo "file-size budget OK."
