//! Shared helpers used by more than one adapter in this module tree.
//!
//! The TOML quoting helpers here are crossed by both `CodexBackend`
//! (which emits `-c key=value` overrides) and the `codex_tests` module
//! (which exercises the key-quoting edge cases). Claude's adapter does
//! not use them today, but the helpers live here rather than in
//! `codex.rs` so a second TOML-override-style adapter (e.g. a future
//! `OpenCode` implementation) can reach them without circular sibling
//! imports.
//!
//! Items are `pub(super)` so they cross the `mod.rs` sibling boundary
//! inside the `agent_backend` module tree but do not leak outside of
//! `crate::agent_backend::` (they are implementation detail, not part
//! of the public harness contract).

/// Render a string value as a TOML basic string literal (double-quoted,
/// with `\`, `"`, and control characters escaped). Used to build the
/// `value` half of Codex's `-c key=value` overrides so prompts and
/// paths with special characters (quotes, newlines, backslashes,
/// equals signs) survive Codex's TOML parser as a literal string
/// rather than being interpreted as structured TOML. JSON strings
/// are a subset of TOML basic strings for the characters we care
/// about, so `serde_json::to_string` produces a valid TOML literal.
pub(super) fn toml_quote_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// Render a TOML key fragment so that names containing characters
/// outside the bare-key alphabet do not break Codex's TOML parser.
///
/// Codex's `-c key=value` flag interprets the LHS as a sequence of
/// dot-separated TOML key fragments (e.g. `mcp_servers.workbridge.command`).
/// TOML bare keys are restricted to `A-Z a-z 0-9 _ -`; any other
/// character (including `.`, space, quote, bracket, non-ASCII)
/// either re-splits the key under a different path (the dot case)
/// or aborts the parse outright. The `mcp import` path
/// (`workbridge mcp import`, see `src/main.rs`) takes server names
/// from JSON object keys verbatim with no validation, so an
/// arbitrary string can reach this code.
///
/// If `name` is non-empty and consists entirely of bare-key
/// characters, return it as-is so the rendered argv stays
/// human-readable. Otherwise emit a TOML quoted key (double-quoted,
/// with `"`, `\`, and control characters escaped per TOML's basic
/// string rules). Empty names always quote (TOML rejects empty
/// bare keys).
pub(super) fn toml_quote_key(name: &str) -> String {
    let bare_safe = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare_safe {
        return name.to_string();
    }
    // Quoted keys share TOML basic-string escape rules with quoted
    // values, so the existing `toml_quote_string` helper produces a
    // valid quoted key (it returns a JSON-encoded string, which is a
    // subset of TOML basic strings for the characters we care about
    // here: `"` -> `\"`, `\` -> `\\`, control characters as `\uXXXX`).
    toml_quote_string(name)
}

/// Render a slice of strings as a TOML inline array of quoted strings
/// (e.g. `["--mcp-bridge","--socket","/tmp/s"]`). Used for the
/// `args` field of Codex's `mcp_servers.<name>` overrides.
pub(super) fn toml_quote_string_array(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&toml_quote_string(item));
    }
    out.push(']');
    out
}
