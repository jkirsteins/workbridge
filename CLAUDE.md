# Workbridge Development Rules

## Error Handling

Never silently ignore errors. Every error must be either:
1. Handled (recovered from with a concrete fallback), or
2. Surfaced to the user (status message, stderr, UI indicator)

`unwrap_or_default()` on a Result that could contain a meaningful error is
a bug. If you want to fall back to a default, log or display the error first.
