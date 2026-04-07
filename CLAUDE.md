# Workbridge Development Rules

Read and follow CONTRIBUTING.md before making changes.

## Review Policy

### Severity overrides

- Using custom widgets when a ratatui built-in widget (from `ratatui-widgets` or `ratatui-core`) could accomplish the same thing: always P0
- Reimplementing layout, rendering, or styling logic already provided by ratatui: always P0

### Review guidelines

- Prefer ratatui's built-in widgets (e.g., `Paragraph`, `Block`, `Table`, `List`, `Tabs`, `Gauge`, `Sparkline`, `BarChart`, `Canvas`, `Scrollbar`) over custom widget implementations whenever possible
- When a ratatui widget almost fits the use case, prefer composing or wrapping the built-in widget rather than building from scratch
- Custom widgets are acceptable only when no ratatui widget (or reasonable composition of widgets) can achieve the required behavior
- Also reference CONTRIBUTING.md and docs/invariants.md as authoritative project files
