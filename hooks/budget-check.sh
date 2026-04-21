#!/usr/bin/env bash
# hooks/budget-check.sh
#
# Enforces per-file line-count budgets declared in
# ci/file-size-budgets.toml. Used by both hooks/pre-commit and the CI
# budget job.
#
# Exit 0 if every file is at or below its budget. Tracked top-level
# `src/*.rs` files WITHOUT an explicit entry are subject to an implicit
# 700-line ceiling: under the ceiling is a silent pass, over the ceiling
# fails with a clear message asking the contributor to either shrink the
# file or declare an explicit entry in ci/file-size-budgets.toml.
# Nested files (src/<dir>/...) are intentionally out of scope.
# Exit 1 (with a human-readable error) if any file is over budget.
#
# The TOML parsing is intentionally minimal so the hook has no runtime
# dependencies beyond bash + awk + coreutils + git.
#
# Line-count source: this hook reads staged index content via
# `git show ":$path"` when a blob exists, and falls back to the
# working tree otherwise. Reading the index (instead of the working
# tree) matches the contract documented in the header of
# hooks/pre-commit: a contributor can `git add` a large file and
# then revert the working tree before `git commit`, so the commit
# bakes in the index content. Validating the working tree would let
# a stage-then-edit-away bypass slip through. The CI `budget` job
# runs against the checked-out tree, where the index-blob read
# degrades to the working-tree read (both are identical on a fresh
# checkout), so CI behavior is unchanged.
set -euo pipefail

budget_file="ci/file-size-budgets.toml"

if [ ! -f "$budget_file" ]; then
    echo "ERROR: budget file not found: $budget_file"
    exit 1
fi

# Read the line count of a path, preferring the staged index blob
# over the working tree. Prints the count on stdout. Prints nothing
# and returns nonzero if neither the index nor the working tree has
# the file (i.e. it has been deleted or never existed).
#
# Implementation note: we must distinguish "git show failed" (no blob
# at :$path, e.g. the file is untracked and unstaged) from "git show
# succeeded and returned a 0-line blob" (the staged content is empty,
# which is legitimate). Piping `git show` directly into `wc -l` loses
# git's exit status - the pipeline's status is `wc`'s, which is
# always 0. Capture the blob first (preserving git's exit code), then
# count lines only on success.
line_count_for() {
    local path="$1"
    local blob
    if blob=$(git show ":$path" 2>/dev/null); then
        printf '%s' "$blob" | wc -l | tr -d ' '
        return 0
    fi
    if [ -f "$path" ]; then
        wc -l < "$path" | tr -d ' '
        return 0
    fi
    return 1
}

fail=0
# Track declared paths so we can cross-check against the list of
# tracked top-level src/*.rs files below.
declared_paths=""
while IFS= read -r line; do
    # Match lines like: "src/app.rs" = 26206
    if [[ "$line" =~ ^\"([^\"]+)\"[[:space:]]*=[[:space:]]*([0-9]+) ]]; then
        path="${BASH_REMATCH[1]}"
        budget="${BASH_REMATCH[2]}"
        declared_paths="$declared_paths
$path"
        if ! actual=$(line_count_for "$path"); then
            # File removed / renamed; budget entry is stale. Not fatal
            # on pre-commit because deletions are legal; a separate
            # rule could later require the entry to be removed too.
            continue
        fi
        if [ "$actual" -gt "$budget" ]; then
            echo "OVER BUDGET: $path has $actual lines, budget is $budget"
            fail=1
        fi
    fi
done < "$budget_file"

# Implicit-ceiling check: every tracked top-level src/*.rs file
# without an explicit budget entry is held to an implicit 700-line
# ceiling. Files at or below the ceiling pass silently; files over
# must either be shrunk or declare an explicit entry with a larger
# budget and rationale in the commit message.
#
# The implicit default exists so newly-extracted modules cannot grow
# silently past the point where they warrant explicit review.
# Declaring an explicit entry is a legitimate escape hatch - the
# budget file is the record of every such exception, and each entry
# is expected to have been justified in its introducing commit.
#
# Nested files (e.g. src/side_effects/*.rs) are intentionally out of
# scope for this hook - see the header comment of
# ci/file-size-budgets.toml.
#
# `git ls-files 'src/*.rs'` matches recursively (gitignore-style
# globbing), so we filter to strictly `src/<name>.rs` (no nested
# slash) before comparing.
IMPLICIT_BUDGET=700
while IFS= read -r tracked; do
    [ -z "$tracked" ] && continue
    case "$tracked" in
        src/*/*) continue ;;  # nested path (e.g. src/side_effects/mod.rs)
    esac
    case "
$declared_paths
" in
        *"
$tracked
"*)
            # File has an explicit entry; already checked above.
            continue
            ;;
    esac
    if ! actual=$(line_count_for "$tracked"); then
        continue
    fi
    if [ "$actual" -gt "$IMPLICIT_BUDGET" ]; then
        echo "OVER IMPLICIT BUDGET ($IMPLICIT_BUDGET lines): $tracked has $actual lines."
        echo "Either shrink the file, or add an explicit entry to"
        echo "ci/file-size-budgets.toml with a larger budget and"
        echo "rationale in the commit message."
        fail=1
    fi
done < <(git ls-files 'src/*.rs' 2>/dev/null)

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "One or more files exceed their line-count budget. Either"
    echo "shrink the file, or - if growth is intentional - update"
    echo "ci/file-size-budgets.toml with rationale in the commit"
    echo "message."
    exit 1
fi

echo "file-size budget OK."
