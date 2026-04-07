# Workbridge Development Rules

Read and follow CONTRIBUTING.md before making changes.

## Review Policy

### Severity overrides

- Using custom widgets when a ratatui built-in widget (from `ratatui-widgets` or `ratatui-core`) could accomplish the same thing: always P0
- Reimplementing layout, rendering, or styling logic already provided by ratatui: always P0
- Modifying docs/invariants.md: always P0 - invariants must never be changed
- Code changes to a system described in docs/ without corresponding docs updates: always P0
- Relaxing linter rules, suppressing warnings, or skipping git hooks (--no-verify): always P0
- Underscore-prefixing struct fields or variables to hide dead code warnings instead of removing the dead code: always P0

### Review guidelines

- Prefer ratatui's built-in widgets (e.g., `Paragraph`, `Block`, `Table`, `List`, `Tabs`, `Gauge`, `Sparkline`, `BarChart`, `Canvas`, `Scrollbar`) over custom widget implementations whenever possible
- When a ratatui widget almost fits the use case, prefer composing or wrapping the built-in widget rather than building from scratch
- Custom widgets are acceptable only when no ratatui widget (or reasonable composition of widgets) can achieve the required behavior
- docs/invariants.md is immutable - any diff that modifies it must be flagged as a violation regardless of context
- If a change touches a system that has a corresponding doc in docs/ (e.g., aggregation, inbox, worktree management, error states, etc.), the review must verify that the doc is updated to reflect the new behavior. Missing doc updates are P0
- Never relax linter requirements, add #[allow(...)], suppress warnings, or skip git hooks to make code pass CI. Fix the underlying issue instead
- Also reference CONTRIBUTING.md and docs/invariants.md as authoritative project files
