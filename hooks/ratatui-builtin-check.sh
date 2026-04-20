#!/usr/bin/env bash
# hooks/ratatui-builtin-check.sh
#
# Heuristic grep for patterns that typically indicate a hand-rolled
# widget implementation where a ratatui-widgets or ratatui-core widget
# already exists. The CLAUDE.md review policy calls this a P0 issue;
# this script is a defense-in-depth assist, not a perfect oracle.
#
# In Phase 1 this runs warn-only (exit 0 even on findings). Promotion
# to a blocking check happens after we've observed the false-positive
# rate and tuned the patterns.
set -euo pipefail

findings=0

# Pattern 1: custom impl of ratatui's Widget trait. Legitimate uses
# exist (they're rare) so this only produces a warning for now.
if git grep -n -E '^impl\s+(Widget|StatefulWidget)\s+for\s+' -- 'src/*.rs' 2>/dev/null; then
    echo ""
    echo "note: found custom Widget / StatefulWidget impl(s) above."
    echo "      Review policy prefers built-in ratatui widgets where"
    echo "      possible. See CLAUDE.md \"Severity overrides\"."
    findings=$((findings + 1))
fi

# Pattern 2: manual buffer cell writes in loops. Sometimes unavoidable;
# usually a sign the code is reimplementing layout / rendering.
if git grep -n -E 'buf\.get_mut\(' -- 'src/*.rs' 2>/dev/null; then
    echo ""
    echo "note: found direct buf.get_mut(...) writes above."
    echo "      Prefer composing ratatui widgets (Paragraph, Block,"
    echo "      Table, List, etc.) over manual cell writes."
    findings=$((findings + 1))
fi

if [ "$findings" -eq 0 ]; then
    echo "ratatui built-in preference check: no findings."
fi

# Warn-only in Phase 1.
exit 0
