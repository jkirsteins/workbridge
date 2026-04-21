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
#
# IMPORTANT: every `git grep` below is invoked via `git --no-pager grep`.
# `git grep` is in git's built-in pager list (per git-config(1)
# `pager.<cmd>`) and auto-paginates through `$GIT_PAGER` (typically
# `less`) when stdout is a TTY. When this hook runs from inside
# `git commit` in an interactive terminal, pre-commit inherits the
# TTY and the pager fires - even when there are zero matches, `less`
# parks at `(END)` and blocks the commit until the user presses `q`.
# `--no-pager` forces unbuffered stdout regardless of TTY state.
set -euo pipefail

findings=0

# Pattern 1: custom impl of ratatui's Widget trait. Legitimate uses
# exist (they're rare) so this only produces a warning for now.
#
# The regex tolerates generic parameters and lifetimes between `impl`
# and `Widget` (e.g. `impl<T> Widget for Foo<T>`,
# `impl<'a> Widget for Foo<'a>`). The leading `^` is dropped so an
# indented impl block is still caught - custom Widget impls are
# virtually always at file scope but a nested/indented occurrence
# inside a module would otherwise escape.
if git --no-pager grep -n -E '\bimpl(\s*<[^>]*>)?\s+(Widget|StatefulWidget)\s+for\s+' -- 'src/*.rs' 2>/dev/null; then
    echo ""
    echo "note: found custom Widget / StatefulWidget impl(s) above."
    echo "      Review policy prefers built-in ratatui widgets where"
    echo "      possible. See CLAUDE.md \"Severity overrides\"."
    findings=$((findings + 1))
fi

# Pattern 2: manual buffer cell writes. Sometimes unavoidable;
# usually a sign the code is reimplementing layout / rendering.
#
# Covers two modern ratatui access patterns:
#   - buf.cell_mut(Position::new(x, y))       (current API)
#   - buf.get_mut(x, y)                        (deprecated API, kept
#                                               for older ratatui)
#   - buf[(x, y)]  /  &mut buf[(x, y)]         (IndexMut access)
# The IndexMut alternative uses a boundary-style prefix
# `(^|[^[:alnum:]_])` rather than an explicit `(&\s*mut\s+)?` optional
# group. The boundary form naturally matches both bare `buf[(...)]`
# (preceded by whitespace, `=`, `(`, etc.) and `&mut buf[(...)]` (the
# space before `buf` is a non-alnum byte). An earlier revision used
# an explicit `&?\s*mut?\s*buf\s*\[\s*\(` alternative, but
# `mut?` quantifies the final `t` (matching `m`, `mu`, or `mut`)
# rather than requiring the full keyword - which is both loose and
# redundant with the boundary-prefix alternative below. Dropping the
# redundant alternative keeps the regex correct and simpler.
if git --no-pager grep -n -E 'buf\.(cell_mut|get_mut)\(|(^|[^[:alnum:]_])buf\s*\[\s*\(' -- 'src/*.rs' 2>/dev/null; then
    echo ""
    echo "note: found direct buffer cell writes above."
    echo "      Prefer composing ratatui widgets (Paragraph, Block,"
    echo "      Table, List, etc.) over manual cell writes."
    findings=$((findings + 1))
fi

if [ "$findings" -eq 0 ]; then
    echo "ratatui built-in preference check: no findings."
fi

# Warn-only in Phase 1.
exit 0
