#!/usr/bin/env bash
# hooks/budget-check.sh
#
# Enforces per-file line-count budgets declared in
# ci/file-size-budgets.toml. Used by both hooks/pre-commit and the CI
# budget job.
#
# Exit 0 if every file is at or below its budget.
# Exit 1 (with a human-readable error) if any file is over budget.
#
# The TOML parsing is intentionally minimal so the hook has no runtime
# dependencies beyond bash + awk + coreutils.
set -euo pipefail

budget_file="ci/file-size-budgets.toml"

if [ ! -f "$budget_file" ]; then
    echo "ERROR: budget file not found: $budget_file"
    exit 1
fi

fail=0
while IFS= read -r line; do
    # Match lines like: "src/app.rs" = 26206
    if [[ "$line" =~ ^\"([^\"]+)\"[[:space:]]*=[[:space:]]*([0-9]+) ]]; then
        path="${BASH_REMATCH[1]}"
        budget="${BASH_REMATCH[2]}"
        if [ ! -f "$path" ]; then
            # File removed / renamed; budget entry is stale. Not fatal
            # on pre-commit because deletions are legal; a separate
            # rule could later require the entry to be removed too.
            continue
        fi
        actual=$(wc -l < "$path" | tr -d ' ')
        if [ "$actual" -gt "$budget" ]; then
            echo "OVER BUDGET: $path has $actual lines, budget is $budget"
            fail=1
        fi
    fi
done < "$budget_file"

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "One or more files exceed their line-count budget."
    echo "Either shrink the file, or - if the growth is intentional -"
    echo "update ci/file-size-budgets.toml with rationale in the commit"
    echo "message."
    exit 1
fi

echo "file-size budget OK."
