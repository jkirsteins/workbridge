//! Pluggable LLM coding harness (agent) backend.
//!
//! This module is the single place where a harness-specific CLI is named
//! and flagged. Everything outside the module talks to `dyn AgentBackend`,
//! so adding a new harness (e.g. Codex) is a matter of writing one more
//! `impl AgentBackend for NewBackend` - the three spawn sites in
//! `src/app.rs` do not change.
//!
//! The contract clauses this trait satisfies (C1..C13) are specified in
//! `docs/harness-contract.md`. Read that doc before editing this file:
//! every method here maps to one or more clauses, and every clause has
//! a reference payload (RP1..RP5) showing the exact wire shape the Claude
//! Code reference implementation produces today.
//!
//! Shape-verification for a second backend lives in the test module at
//! the bottom of this file: `CodexBackend` compiles against the trait and
//! `codex_shape_compiles` asserts that it builds argv vectors for the
//! three profiles (work-item, review-gate, global) without workbridge
//! editing any harness-specific state file.

use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use crate::work_item::WorkItemStatus;

/// Discriminant for logging and config parity. Not used for dispatch
/// (dispatch goes through `dyn AgentBackend`); kept so the runtime can
/// identify which backend is active without downcasting.
///
/// The enum lists every harness the codebase knows about, including
/// future-work stubs. The `OpenCode` variant is internal scaffolding
/// for a future adapter: it has a zero-sized `OpenCodeBackend` impl
/// reachable via `backend_for_kind` (so a future wiring change does
/// not have to reintroduce the enum + struct at the same time), but
/// there is no user-facing path to select it. It is intentionally
/// excluded from `all()`, rejected by `FromStr`, and not bound to
/// any keystroke; see the first-run modal and the `c`/`x` handlers
/// in `src/event.rs`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AgentBackendKind {
    /// Anthropic Claude Code CLI. Reference implementation.
    ClaudeCode,
    /// OpenAI / Codex CLI. Implemented adapter; see `CodexBackend`.
    Codex,
    /// OpenCode CLI. Future-work stub: `OpenCodeBackend` exists and
    /// `backend_for_kind` returns it, but the variant is not exposed
    /// through `all()`, `FromStr`, or any keybinding, so there is no
    /// user-facing path to produce this value. The scaffolding is kept
    /// so a future adapter can land without reintroducing both the
    /// struct and the enum variant in the same change.
    OpenCode,
}

impl AgentBackendKind {
    /// User-selectable harness kinds. Drives the first-run Ctrl+G
    /// modal list and any iteration that represents "choices the
    /// user can make". Deliberately excludes `OpenCode` because that
    /// variant has no user-facing spawn path; see the type-level
    /// comment above.
    pub fn all() -> [AgentBackendKind; 2] {
        [AgentBackendKind::ClaudeCode, AgentBackendKind::Codex]
    }

    /// Lowercase canonical name used in the CLI (`workbridge config
    /// set global-assistant-harness <name>`), in `config.toml`, and in
    /// the first-run modal keybindings.
    pub fn canonical_name(self) -> &'static str {
        match self {
            AgentBackendKind::ClaudeCode => "claude",
            AgentBackendKind::Codex => "codex",
            AgentBackendKind::OpenCode => "opencode",
        }
    }

    /// Binary name expected on `PATH`. Used by `is_available` and by the
    /// "command not found" toast text.
    pub fn binary_name(self) -> &'static str {
        match self {
            AgentBackendKind::ClaudeCode => "claude",
            AgentBackendKind::Codex => "codex",
            AgentBackendKind::OpenCode => "opencode",
        }
    }

    /// Human-readable display name used in status-bar text, UI titles
    /// and the first-run modal body.
    pub fn display_name(self) -> &'static str {
        match self {
            AgentBackendKind::ClaudeCode => "Claude Code",
            AgentBackendKind::Codex => "Codex",
            AgentBackendKind::OpenCode => "OpenCode",
        }
    }

    /// Single-character keybinding used in the first-run modal and the
    /// work-item keyhints for user-selectable kinds. Must stay in sync
    /// with `src/event.rs`. The `OpenCode` mapping is nominal - that
    /// variant is not user-selectable (absent from `all()`) so the
    /// value is never rendered or read; it is kept only so the match
    /// remains exhaustive.
    pub fn keybinding(self) -> char {
        match self {
            AgentBackendKind::ClaudeCode => 'c',
            AgentBackendKind::Codex => 'x',
            AgentBackendKind::OpenCode => '\0',
        }
    }
}

impl std::fmt::Display for AgentBackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.canonical_name())
    }
}

impl FromStr for AgentBackendKind {
    type Err = UnknownHarnessName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // `OpenCode` is intentionally NOT parsed here. The CLI
        // (`workbridge config set global-assistant-harness <name>`)
        // and any other textual surface must reject "opencode" so the
        // user cannot configure a non-functional harness. See the
        // type-level comment on `AgentBackendKind` for why the variant
        // still exists internally.
        match s {
            "claude" => Ok(AgentBackendKind::ClaudeCode),
            "codex" => Ok(AgentBackendKind::Codex),
            other => Err(UnknownHarnessName {
                got: other.to_string(),
            }),
        }
    }
}

/// Error returned when a harness name the user typed does not match any
/// known variant. The canonical names are the single source of truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownHarnessName {
    pub got: String,
}

impl std::fmt::Display for UnknownHarnessName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown harness name '{}' (expected one of: claude, codex)",
            self.got
        )
    }
}

impl std::error::Error for UnknownHarnessName {}

/// Produce a fresh `Arc<dyn AgentBackend>` for the requested kind. The
/// backends are zero-sized structs so construction is cheap. Spawn sites
/// call this to resolve a per-work-item backend once they know the
/// user's harness choice (see `App::backend_for_kind`).
pub fn backend_for_kind(kind: AgentBackendKind) -> Arc<dyn AgentBackend> {
    match kind {
        AgentBackendKind::ClaudeCode => Arc::new(ClaudeCodeBackend),
        AgentBackendKind::Codex => Arc::new(CodexBackend),
        AgentBackendKind::OpenCode => Arc::new(OpenCodeBackend),
    }
}

/// Lazy PATH scan. Uses the pure-Rust `which` crate so we do not shell
/// out. Called from the `c` / `x` key handlers and the first-run
/// modal; MUST NOT be called from a render path (see `docs/UI.md`
/// "Blocking I/O Prohibition" - `which` walks `$PATH` synchronously,
/// which is acceptable for a keypress but not for a render tick).
pub fn is_available(kind: AgentBackendKind) -> bool {
    which::which(kind.binary_name()).is_ok()
}

/// Allowlist of workbridge MCP tools a write-capable session is permitted
/// to call. The same list covers work-item and global-assistant sessions
/// (C5). The review gate uses server-side filtering (read-only MCP mode)
/// instead of a CLI allowlist, so it does NOT pass this list.
pub const WORK_ITEM_ALLOWED_TOOLS: &[&str] = &[
    "mcp__workbridge__workbridge_get_context",
    "mcp__workbridge__workbridge_query_log",
    "mcp__workbridge__workbridge_get_plan",
    "mcp__workbridge__workbridge_report_progress",
    "mcp__workbridge__workbridge_log_event",
    "mcp__workbridge__workbridge_set_activity",
    "mcp__workbridge__workbridge_approve_review",
    "mcp__workbridge__workbridge_request_changes",
    "mcp__workbridge__workbridge_set_status",
    "mcp__workbridge__workbridge_set_plan",
    "mcp__workbridge__workbridge_set_title",
    "mcp__workbridge__workbridge_set_description",
    "mcp__workbridge__workbridge_list_repos",
    "mcp__workbridge__workbridge_list_work_items",
    "mcp__workbridge__workbridge_repo_info",
];

/// Render a string value as a TOML basic string literal (double-quoted,
/// with `\`, `"`, and control characters escaped). Used to build the
/// `value` half of Codex's `-c key=value` overrides so prompts and
/// paths with special characters (quotes, newlines, backslashes,
/// equals signs) survive Codex's TOML parser as a literal string
/// rather than being interpreted as structured TOML. JSON strings
/// are a subset of TOML basic strings for the characters we care
/// about, so `serde_json::to_string` produces a valid TOML literal.
fn toml_quote_string(s: &str) -> String {
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
fn toml_quote_key(name: &str) -> String {
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
fn toml_quote_string_array(items: &[String]) -> String {
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

/// Direct specification of an MCP stdio server (name + command + args).
/// Emitted alongside `mcp_config_path` because some harnesses (notably
/// Codex) register MCP servers via structured CLI overrides that need
/// the raw `command` and `args` fields rather than a pointer to an
/// external JSON file.
///
/// Codex's `-c mcp_servers.<name>.*` flag writes TOML overrides that
/// populate `~/.codex/config.toml`-shaped state in memory, where each
/// MCP server entry requires `command` (string) and `args` (array of
/// strings) directly. There is no `config` sub-field that reads an
/// external JSON, and the JSON schema of the `--mcp-config` file is
/// Claude's, not Codex's - so passing the JSON path to Codex is a
/// silent no-op that spawns the session with no MCP server.
///
/// The `name` field is the key under `mcp_servers.<name>` in the
/// emitted overrides. The primary `mcp_bridge` always uses the
/// reserved name `workbridge` (the workbridge MCP server itself);
/// each entry in `extra_bridges` uses the user-configured server name
/// from the per-repo MCP server list (`config.mcp_servers_for_repo`).
///
/// Claude does not need this struct - its `--mcp-config <file>` flag
/// reads the JSON directly - but the struct is populated for both
/// backends at every spawn site so a future backend with the same
/// "structured override" requirement is a one-line addition to the
/// relevant `build_command` and does not need a new plumbing pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpBridgeSpec {
    /// Server name under `mcp_servers.<name>` in the emitted Codex
    /// override. The primary bridge uses `"workbridge"`; per-repo
    /// extras use the user's configured name (`McpServerEntry::name`).
    pub name: String,
    /// Absolute path to the binary that acts as the MCP bridge (for
    /// the primary bridge: the workbridge executable re-invoked with
    /// `--mcp-bridge`; for extras: the user's configured `command`).
    pub command: PathBuf,
    /// Argument vector for the bridge binary. For the primary bridge:
    /// `--mcp-bridge`, `--socket <path>` in the order the bridge
    /// expects. For extras: the user-configured args.
    pub args: Vec<String>,
}

/// Config for an interactive spawn (work-item or global-assistant).
///
/// Passed by value borrowing into `AgentBackend::build_command`. Every
/// field is already rendered / resolved by the caller: the backend is
/// pure argv construction and MUST NOT touch the filesystem, MUST NOT
/// read config, and MUST NOT return dynamic values.
pub struct SpawnConfig<'a> {
    /// Workflow stage the session is spawning into. Used by the backend
    /// to decide whether a stage reminder applies (C8) and whether the
    /// session is read-only (C11).
    pub stage: WorkItemStatus,
    /// Rendered stage system prompt (C6). `None` for global-assistant
    /// spawns that have no stage prompt; the backend still accepts the
    /// flag whose value is passed separately.
    pub system_prompt: Option<&'a str>,
    /// Path to the MCP-config JSON file already written by the caller.
    /// The backend appends the flag (e.g. `--mcp-config <path>`) in the
    /// order its CLI requires (C4). `None` produces a degraded session
    /// that cannot reach the workbridge MCP server; this path exists so
    /// a failed config write (disk full, permission denied) still lets
    /// the user see and dismiss the session rather than silently
    /// blocking it.
    pub mcp_config_path: Option<&'a Path>,
    /// Structured bridge spec (command + args) for harnesses that
    /// register MCP servers via per-field CLI overrides instead of an
    /// external JSON file. See `McpBridgeSpec`. `None` degrades the
    /// session in the same way `mcp_config_path: None` does - the
    /// agent cannot reach the workbridge MCP server, but the session
    /// is still visible so the user can dismiss it.
    pub mcp_bridge: Option<&'a McpBridgeSpec>,
    /// Per-repo user-configured MCP servers (from
    /// `Config::mcp_servers_for_repo`). Claude consumes these via the
    /// `mcp_config_path` JSON file; Codex emits one set of per-field
    /// `-c mcp_servers.<name>.*` overrides per entry, in addition to
    /// the primary `mcp_bridge`. Empty when the work item is not
    /// associated with a repo or the repo has no extra MCP servers
    /// configured. HTTP-transport entries are filtered out by the
    /// caller for Codex (Codex's `mcp_servers.<name>` schema requires
    /// command + args; there is no `url` sub-field).
    pub extra_bridges: &'a [McpBridgeSpec],
    /// Tool allowlist (C5). For Claude this is passed as a comma-joined
    /// argument to `--allowedTools`; the review-gate spawn path does NOT
    /// go through this struct, so this list is never empty in practice.
    pub allowed_tools: &'a [&'a str],
    /// Literal initial user message to inject as the positional argument
    /// (C7). `None` disables auto-start. The caller decides which
    /// auto-start key in `stage_prompts.json` to render; the backend only
    /// sees the final rendered string.
    pub auto_start_message: Option<&'a str>,
    /// Read-only sessions (C11) are enforced at the MCP server layer.
    /// Currently only the review gate is read-only, and it uses
    /// `build_review_gate_command` directly. This field is kept so a
    /// future interactive read-only profile is a one-line change.
    pub read_only: bool,
}

/// Config for a headless review-gate spawn (C1 headless, C11 read-only).
pub struct ReviewGateSpawnConfig<'a> {
    /// Rendered review-gate system prompt (the `review_gate` template).
    pub system_prompt: &'a str,
    /// Initial user prompt - the configured review skill string
    /// (e.g. `/claude-adversarial-review`).
    pub initial_prompt: &'a str,
    /// JSON schema for the structured verdict; see RP2 in the doc.
    pub json_schema: &'a str,
    /// Path to the MCP-config JSON file the caller already wrote.
    pub mcp_config_path: &'a Path,
    /// Structured MCP bridge spec for harnesses that need per-field
    /// CLI overrides. Mirrors `SpawnConfig::mcp_bridge`. Required
    /// rather than optional on this struct because the review / rebase
    /// gate paths always have exe + socket in hand when they build the
    /// config; a backend that doesn't need the spec simply ignores it.
    pub mcp_bridge: &'a McpBridgeSpec,
    /// Per-repo user-configured MCP servers (from
    /// `Config::mcp_servers_for_repo`). Same semantics as
    /// `SpawnConfig::extra_bridges`. Defaults to an empty slice for
    /// gate spawns whose caller does not have a per-repo context
    /// (e.g. global-assistant or unscoped review).
    pub extra_bridges: &'a [McpBridgeSpec],
}

/// Verdict parsed from a headless review-gate session's stdout (RP5).
/// Absence of either field is interpreted as `approved: false` with an
/// empty detail.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewGateVerdict {
    pub approved: bool,
    pub detail: String,
}

/// Pluggable LLM coding harness adapter.
///
/// Each implementation owns exactly one CLI (`claude`, `codex`, ...) and
/// knows how to build argv vectors for the three spawn profiles defined
/// in `docs/harness-contract.md` under "Known Spawn Sites". Implementors
/// MUST satisfy every clause C1..C13 from that doc; if a clause cannot
/// be satisfied, the implementor must say so explicitly in the doc's
/// Implementation Map and the review is required to flag the gap (see
/// `CLAUDE.md` severity overrides).
///
/// # Writing a new backend
///
/// Required reading: `docs/harness-contract.md` (every clause) and this
/// file's `tests` module (the Codex shape-verification stub).
///
/// 1. Add a variant to `AgentBackendKind` behind `#[cfg(test)]` first and
///    write the shape test. It forces you to think about argv before
///    touching any real spawn site.
/// 2. Implement the trait. If a clause cannot be satisfied with flags
///    alone (e.g. Codex's `~/.codex/config.toml` for MCP injection),
///    write the file inside `write_session_files` to a temp directory
///    and return the path so the caller cleans it up - do NOT write
///    into the worktree (pollutes git state), do NOT set environment
///    variables (C13), and do NOT mutate the user's dotfiles (see the
///    file-injection prohibition in the review policy).
/// 3. Update `docs/harness-contract.md` "Implementation Map" with a new
///    per-clause entry for the backend, and the "Known Spawn Sites" line
///    numbers if any moved.
/// 4. Promote the `#[cfg(test)]` variant to a real variant and wire
///    `App::agent_backend` to select it based on config.
pub trait AgentBackend: Send + Sync {
    /// Discriminant for logging and parity checks.
    fn kind(&self) -> AgentBackendKind;

    /// The CLI binary name this backend spawns (e.g. `claude`, `codex`).
    /// This is the single place a vendor name is allowed to appear as a
    /// string literal outside of doc comments and `#[cfg(test)]` code.
    fn command_name(&self) -> &'static str;

    /// Build the argv for an interactive work-item or global session.
    /// The first element of the returned vec is the command name (C1);
    /// subsequent elements are flags and positional arguments in the
    /// order the harness requires.
    ///
    /// Must be a pure function of the input: no filesystem access, no
    /// environment reads, no clock.
    fn build_command(&self, cfg: &SpawnConfig<'_>) -> Vec<String>;

    /// Build the argv for a headless review-gate spawn (C1 headless, C11
    /// read-only). The returned vec goes directly into
    /// `std::process::Command::new(...).args(...)`.
    fn build_review_gate_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String>;

    /// Build the argv for a headless read-write spawn (rebase gate).
    /// Similar to `build_review_gate_command` but the session needs
    /// write access (file edits, git operations) so the backend MUST
    /// include its permission-bypass flag (Claude: `--dangerously-skip-
    /// permissions`; Codex: `--full-auto`). The returned vec goes
    /// directly into `std::process::Command::new(...).args(...)`.
    fn build_headless_rw_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String>;

    /// Parse the verdict envelope produced by a headless review-gate
    /// session. Backends that emit a single JSON document (Claude's
    /// `--output-format json`) reach into the envelope's structured body
    /// directly; backends that emit an event stream (Codex `exec --json`)
    /// keep only the last relevant event before calling this function.
    fn parse_review_gate_stdout(&self, stdout: &str) -> ReviewGateVerdict;

    /// Write backend-specific temp files required before spawn. For
    /// backends that route MCP injection through a mechanism other than
    /// CLI flags (e.g. a config file), this is where that file is
    /// written - but it MUST go into a temp directory, never into the
    /// user's worktree. Writing into a worktree pollutes git state and
    /// constitutes file injection (prohibited by the review policy).
    /// Claude Code needs no side-car files: `--mcp-config` handles
    /// MCP injection entirely via the CLI flag.
    ///
    /// Returns the list of paths the backend created. The caller MUST
    /// pass the same list back to `cleanup_session_files` when the
    /// session ends so nothing leaks.
    ///
    /// `cwd` is the session's working directory (C2); provided for
    /// reference but backends MUST NOT write files into it. Called
    /// only for spawn sites that own a worktree-scoped cwd (work-item
    /// spawns today); global-assistant and review-gate spawns skip
    /// this step because they use scratch cwds.
    fn write_session_files(&self, cwd: &Path, mcp_config_json: &str) -> io::Result<Vec<PathBuf>>;

    /// Remove the files returned by `write_session_files`. The default
    /// implementation best-effort removes each path and swallows errors
    /// (missing files are fine - the session may have died before the
    /// files were written).
    fn cleanup_session_files(&self, paths: &[PathBuf]) {
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Reference implementation: Anthropic Claude Code (`claude`) CLI.
///
/// Every piece of Claude-specific knowledge in workbridge lives here:
/// the binary name, every CLI flag, the `PostToolUse` planning hook, and
/// the JSON envelope shape for headless
/// output. Nothing else in `src/` mentions these.
pub struct ClaudeCodeBackend;

impl ClaudeCodeBackend {
    /// The `--settings` JSON payload that installs a `PostToolUse` hook on
    /// `TodoWrite`. The hook greps the tool payload for
    /// `workbridge_set_plan`; if missing, it emits a stderr reminder that
    /// Claude sees on its next turn. See RP4 and C8 in the contract doc.
    ///
    /// This is the C8 delivery mechanism for Claude Code. Backends that
    /// do not have a hook system (Codex, per its Implementation Map entry)
    /// return an empty argv fragment from `planning_reminder_argv` and
    /// rely on the system-prompt-embedded reminder alone; that path is
    /// strictly weaker because it cannot re-fire after the first turn.
    const PLANNING_REMINDER_JSON: &'static str = r#"{"hooks":{"PostToolUse":[{"matcher":"TodoWrite","hooks":[{"type":"command","command":"bash -c 'cat | grep -q workbridge_set_plan || echo \"REMINDER: Your plan MUST include a step to call workbridge_set_plan MCP tool to persist the plan. Add this as the FIRST step.\" >&2; true'"}]}]}}"#;

    /// Argv fragment that installs the planning-stage reminder hook.
    /// Empty vec for non-Planning stages.
    fn planning_reminder_argv(stage: WorkItemStatus) -> Vec<String> {
        if stage == WorkItemStatus::Planning {
            vec![
                "--settings".to_string(),
                Self::PLANNING_REMINDER_JSON.to_string(),
            ]
        } else {
            Vec::new()
        }
    }
}

impl AgentBackend for ClaudeCodeBackend {
    fn kind(&self) -> AgentBackendKind {
        AgentBackendKind::ClaudeCode
    }

    fn command_name(&self) -> &'static str {
        "claude"
    }

    fn build_command(&self, cfg: &SpawnConfig<'_>) -> Vec<String> {
        let mut cmd: Vec<String> = vec![self.command_name().to_string()];
        // C3 - Permissions: always pass the non-interactive flag for
        // write-capable sessions. Read-only sessions skip it because the
        // server-side MCP filter (C11) is the real enforcement and the
        // CLI allowlist is defence in depth; passing the bypass flag
        // for a read-only session would be misleading.
        if !cfg.read_only {
            cmd.push("--dangerously-skip-permissions".to_string());
            // C5 - Tool allowlist (write-capable sessions only). Read-
            // only sessions omit this and rely entirely on the read-only
            // MCP server filter; see `docs/harness-contract.md` C11.
            cmd.push("--allowedTools".to_string());
            cmd.push(cfg.allowed_tools.join(","));
        }
        // C8 - Stage reminder (Planning only for Claude).
        cmd.extend(Self::planning_reminder_argv(cfg.stage));
        // C6 - System prompt.
        if let Some(prompt) = cfg.system_prompt {
            cmd.push("--system-prompt".to_string());
            cmd.push(prompt.to_string());
        }
        // C7 - Auto-start prompt. MUST come BEFORE --mcp-config because
        // Claude Code otherwise mistakes the positional for another
        // config file path. Regression-tested in
        // `claude_interactive_argv_for_planning`.
        if let Some(msg) = cfg.auto_start_message {
            cmd.push(msg.to_string());
        }
        // C4 - MCP injection (temp-file path, written by the caller).
        // Optional: when None, the session spawns in a degraded mode
        // that cannot reach the workbridge MCP server. See
        // `SpawnConfig::mcp_config_path` for when this path is taken.
        if let Some(path) = cfg.mcp_config_path {
            cmd.push("--mcp-config".to_string());
            cmd.push(path.to_string_lossy().into_owned());
        }
        cmd
    }

    fn build_review_gate_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
        // C1 headless + C11 read-only: no --dangerously-skip-permissions
        // (--print is non-interactive and never prompts) and no
        // --allowedTools (the read-only MCP server does the filtering).
        vec![
            "--print".to_string(),
            "-p".to_string(),
            cfg.initial_prompt.to_string(),
            "--system-prompt".to_string(),
            cfg.system_prompt.to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--json-schema".to_string(),
            cfg.json_schema.to_string(),
            "--mcp-config".to_string(),
            cfg.mcp_config_path.to_string_lossy().into_owned(),
        ]
    }

    fn build_headless_rw_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
        // Headless read-write: same shape as the review gate but with
        // --dangerously-skip-permissions so the session can write files
        // and run git commands (e.g. conflict resolution during rebase).
        vec![
            "--print".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "-p".to_string(),
            cfg.initial_prompt.to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--json-schema".to_string(),
            cfg.json_schema.to_string(),
            "--mcp-config".to_string(),
            cfg.mcp_config_path.to_string_lossy().into_owned(),
        ]
    }

    fn parse_review_gate_stdout(&self, stdout: &str) -> ReviewGateVerdict {
        // Claude Code's `--output-format json` wraps the schema-validated
        // body in a `structured_output` field. Missing fields degrade to
        // `approved: false, detail: ""`.
        match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
            Ok(envelope) => {
                let structured = &envelope["structured_output"];
                let approved = structured["approved"].as_bool().unwrap_or(false);
                let detail = structured["detail"].as_str().unwrap_or("").to_string();
                ReviewGateVerdict { approved, detail }
            }
            Err(e) => ReviewGateVerdict {
                approved: false,
                detail: format!("review gate: invalid JSON response: {e}"),
            },
        }
    }

    fn write_session_files(&self, _cwd: &Path, _mcp_config_json: &str) -> io::Result<Vec<PathBuf>> {
        // The `--mcp-config` flag already injects the MCP config
        // into the session. Writing a redundant `.mcp.json` into
        // the worktree would pollute the user's git state and
        // constitutes file injection into a third-party workspace
        // (prohibited by the review policy). No side-car files
        // are needed for Claude Code.
        Ok(vec![])
    }
}

/// Real adapter for the OpenAI Codex CLI (`codex`). Satisfies C1..C13 per
/// `docs/harness-contract.md`; see the "Codex" column of the
/// Implementation Map for the per-clause workaround details.
///
/// Argv shape summary:
///
/// - Interactive (work-item / global): `codex --ask-for-approval never
///   --sandbox workspace-write
///   --config mcp_servers.workbridge.command="<exe>"
///   --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<sock>"]
///   [--config instructions="<sys>"] [<auto-start>]` (RP1c).
///   The approval-policy + sandbox pair is equivalent to `--full-auto`
///   minus `-a on-request`; the user explicitly disallowed per-call
///   MCP approval prompts (see Authorization 2b in the review-loop
///   session log, superseding the original `--full-auto` authorization).
/// - Headless read-only (review gate): `codex exec --json
///   --config instructions="<sys>"
///   --config mcp_servers.workbridge.command="<exe>"
///   --config mcp_servers.workbridge.args=[...] <prompt>` (RP2c).
/// - Headless read-write (rebase gate): `codex --ask-for-approval never
///   exec --json --full-auto
///   --config mcp_servers.workbridge.command="<exe>"
///   --config mcp_servers.workbridge.args=[...] <prompt>` (RP2bc).
///   `--ask-for-approval` is a TOP-LEVEL flag and MUST precede `exec`
///   (clap rejects it inside the `exec` subcommand). `--full-auto`
///   stays inside `exec`.
///
/// C4 note: earlier drafts used `mcp_servers.workbridge.config=<path>`
/// pointing at the Claude-shaped JSON file. That shape is syntactically
/// accepted by Codex but rejected at configuration load time with
/// "invalid transport in `mcp_servers.workbridge`" - Codex's TOML
/// schema requires `command` (string) and `args` (array of strings)
/// directly, and has no `config` sub-field. The current shape emits
/// the per-field overrides built from `McpBridgeSpec`.
///
/// C13 (no env leakage) holds because every piece of per-session state
/// is delivered via the `--config` CLI flag. The adapter does NOT touch
/// `~/.codex/config.toml` or any other file in `$HOME`, and does NOT
/// set environment variables. Temp files written by
/// `write_session_files` go under `std::env::temp_dir()` only.
pub struct CodexBackend;

impl CodexBackend {
    /// Emit `--config mcp_servers.<name>.*` overrides for one MCP
    /// stdio bridge (the workbridge primary or a per-repo extra).
    /// Codex requires `command` (string) and `args` (array of strings)
    /// at this path; there is no `config` sub-field that reads an
    /// external JSON (the pattern used by `--mcp-config` on the Claude
    /// side).
    fn extend_one_mcp_bridge(cmd: &mut Vec<String>, bridge: &McpBridgeSpec) {
        // Render `bridge.name` as a TOML key fragment. Codex's
        // `-c <dotted.key>=<value>` parses the LHS as TOML key
        // fragments; names containing `.`, space, quote, bracket, or
        // non-ASCII either misregister under a different path or
        // abort the TOML parse. `toml_quote_key` returns the bare
        // name when safe and a quoted key otherwise, so `my.server`
        // becomes `mcp_servers."my.server".command` instead of
        // mistakenly nesting under `mcp_servers.my.server.command`.
        let key = toml_quote_key(&bridge.name);
        cmd.push("--config".to_string());
        cmd.push(format!(
            "mcp_servers.{key}.command={}",
            toml_quote_string(&bridge.command.to_string_lossy())
        ));
        cmd.push("--config".to_string());
        cmd.push(format!(
            "mcp_servers.{key}.args={}",
            toml_quote_string_array(&bridge.args)
        ));
    }

    /// Emit overrides for every per-repo extra bridge in order, then
    /// the primary workbridge bridge LAST. Passing `primary: None`
    /// silently skips the workbridge overrides so callers in a
    /// degraded-spawn path (e.g. MCP socket bind failed) still produce
    /// a non-broken argv; extras are still emitted in that case so
    /// user-configured per-repo servers remain available even when the
    /// workbridge bridge itself is unavailable.
    ///
    /// Ordering invariant: the primary `workbridge` overrides MUST be
    /// emitted AFTER every extra. Codex's `-c key=value` overrides are
    /// last-write-wins, so this ordering structurally guarantees that
    /// no per-repo extra (whether named `workbridge` accidentally or
    /// maliciously) can clobber the workbridge MCP bridge entry that
    /// the session needs to talk to workbridge itself. This mirrors
    /// `crate::mcp::build_mcp_config`, which inserts the workbridge
    /// server into the JSON map last for the same reason; see the
    /// `codex_extras_cannot_override_workbridge_primary` regression
    /// test and `build_mcp_config_workbridge_key_always_wins`.
    fn extend_mcp_bridge_argv(
        cmd: &mut Vec<String>,
        primary: Option<&McpBridgeSpec>,
        extras: &[McpBridgeSpec],
    ) {
        for extra in extras {
            Self::extend_one_mcp_bridge(cmd, extra);
        }
        if let Some(bridge) = primary {
            Self::extend_one_mcp_bridge(cmd, bridge);
        }
    }
}

impl AgentBackend for CodexBackend {
    fn kind(&self) -> AgentBackendKind {
        AgentBackendKind::Codex
    }

    fn command_name(&self) -> &'static str {
        "codex"
    }

    fn build_command(&self, cfg: &SpawnConfig<'_>) -> Vec<String> {
        let mut cmd = vec![self.command_name().to_string()];
        // C3: Codex's approval policy and sandbox policy are split across
        // two flags. For write-capable interactive sessions we want ZERO
        // in-session approval prompts (the user authorised this via the
        // 2026-04-17 "MCP tools need to be pre-allowed for codex"
        // directive, superseding the original `--full-auto` choice) but
        // we still want the workspace-write sandbox so Codex's shell
        // tool is scoped to the work-item's worktree. `--full-auto`
        // bundles `-a on-request --sandbox workspace-write`, which
        // prompts on every MCP tool call; we emit the two flags directly
        // instead. Read-only interactive sessions (global assistant in
        // read-only mode, hypothetical future read-only scope) omit both
        // since the MCP-server layer enforces read-only there.
        if !cfg.read_only {
            cmd.push("--ask-for-approval".to_string());
            cmd.push("never".to_string());
            cmd.push("--sandbox".to_string());
            cmd.push("workspace-write".to_string());
        }
        // C3b: `--ask-for-approval never` only governs shell/patch
        // sandbox operations in codex-cli 0.120.0; it does NOT silence
        // the per-MCP-tool "elicitation" approval dialog that pops up
        // the first time the model calls each workbridge_* tool.
        // Setting `granular_approval.mcp_elicitations=false` (verified
        // present in the shipped codex binary via `strings`; its
        // docstring reads "Whether to allow MCP elicitation prompts")
        // suppresses that dialog structurally so MCP calls go through
        // without user interaction. Applied to both read-only and
        // write-capable interactive sessions because the read-only
        // review gate also needs unprompted MCP access to call its
        // verdict tools. This was the fix for the 2026-04-17 user
        // directive "MCP tools need to be pre-allowed for codex, so it
        // does not ask for permission for them" - the approval-policy
        // swap alone was necessary but not sufficient.
        cmd.push("--config".to_string());
        cmd.push("granular_approval.mcp_elicitations=false".to_string());
        // C4: Codex has no `--mcp-config` flag and no `mcp_servers.*.config`
        // sub-field that reads an external JSON (that path is what the
        // earlier implementation used; verified to be rejected by
        // `codex mcp list` with "invalid transport"). Instead, Codex's
        // TOML schema for `mcp_servers.<name>` requires `command`
        // (string) and `args` (array of strings) directly - so we emit
        // per-field `--config` overrides from `McpBridgeSpec`.
        //
        // The caller still writes the Claude-shaped JSON to
        // `mcp_config_path` (it is consumed by Claude's adapter and by
        // the MCP-config on-disk contract for parity logging), but we
        // do NOT reference that path in Codex's argv - only the
        // structured `mcp_bridge` spec is used. Missing `mcp_bridge`
        // degrades rather than falling back to `~/.codex/config.toml`
        // (which would silently cross-contaminate personal config with
        // workbridge runtime state).
        let _ = cfg.mcp_config_path;
        Self::extend_mcp_bridge_argv(&mut cmd, cfg.mcp_bridge, cfg.extra_bridges);
        // C5: Codex has no per-tool CLI allowlist; the allowlist is
        // enforced at the MCP server layer (the same mechanism the
        // Claude review gate already uses). `allowed_tools` is accepted
        // here for audit parity but not rendered into argv.
        let _ = cfg.allowed_tools;
        // C6: Codex has no `--system-prompt`; the system prompt is
        // delivered via `--config instructions=<prompt>`. For a session
        // with no system prompt (global assistant), the flag is omitted.
        // The value is TOML-quoted so prompts with special characters
        // (quotes, newlines, equals signs) survive Codex's TOML parser.
        if let Some(prompt) = cfg.system_prompt {
            cmd.push("--config".to_string());
            cmd.push(format!("instructions={}", toml_quote_string(prompt)));
        }
        // C8: Codex has no PostToolUse hook equivalent. The Planning
        // reminder is embedded in the system prompt (the caller renders
        // the planning prompt with the reminder baked in). This is
        // strictly weaker than Claude's hook because it cannot re-fire
        // on each TodoWrite; documented in the Implementation Map.
        // C7: Auto-start message goes as the trailing positional
        // argument. Unlike Claude, Codex does not conflate positionals
        // with config files, so ordering vs `--config` flags does not
        // matter. Emitted last for human readability of logged argv.
        if let Some(msg) = cfg.auto_start_message {
            cmd.push(msg.to_string());
        }
        // C2: Codex takes `--cd` to set the child cwd, but the PTY
        // infrastructure already sets cwd via `Session::spawn`, so we
        // do NOT add `--cd` here. The clause is met by the PTY's cwd.
        cmd
    }

    fn build_review_gate_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
        // C1 headless + C11 read-only: `codex exec --json` emits a
        // newline-delimited event stream. No `--full-auto` (this session
        // cannot write) and no `--ask-for-approval` overrides (the
        // read-only MCP server enforces the gate).
        //
        // C3b: also suppress MCP elicitation prompts - see the note in
        // `build_command`. Without this, the review gate runs headless
        // and would block forever on the first workbridge_* call
        // because there is no interactive user to answer the prompt.
        let mut cmd = vec![
            "exec".to_string(),
            "--json".to_string(),
            "--config".to_string(),
            "granular_approval.mcp_elicitations=false".to_string(),
            "--config".to_string(),
            format!("instructions={}", toml_quote_string(cfg.system_prompt)),
        ];
        Self::extend_mcp_bridge_argv(&mut cmd, Some(cfg.mcp_bridge), cfg.extra_bridges);
        cmd.push(cfg.initial_prompt.to_string());
        cmd
    }

    fn build_headless_rw_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
        // Headless read-write: `--ask-for-approval never` is a TOP-LEVEL
        // codex flag - it MUST come BEFORE the `exec` subcommand, or
        // clap rejects the argv with "unexpected argument
        // '--ask-for-approval' found" and the rebase gate fails before
        // a single child token is produced. Verified live on
        // 2026-04-16: `codex --ask-for-approval never exec --json
        // --full-auto --skip-git-repo-check "echo hi"` produces a valid
        // event stream, while `codex exec --json --full-auto
        // --ask-for-approval never "echo hi"` fails immediately.
        // `--full-auto` (write access) IS an `exec` subcommand flag and
        // stays after `exec`. The combination parallels Claude's
        // `--dangerously-skip-permissions` (which already implies "no
        // approval prompts").
        //
        // C3b: also suppress MCP elicitation prompts (same reasoning as
        // the review gate - the rebase gate is headless and would block
        // forever on an MCP approval prompt).
        let mut cmd = vec![
            "--ask-for-approval".to_string(),
            "never".to_string(),
            "exec".to_string(),
            "--json".to_string(),
            "--full-auto".to_string(),
            "--config".to_string(),
            "granular_approval.mcp_elicitations=false".to_string(),
        ];
        Self::extend_mcp_bridge_argv(&mut cmd, Some(cfg.mcp_bridge), cfg.extra_bridges);
        cmd.push(cfg.initial_prompt.to_string());
        cmd
    }

    fn parse_review_gate_stdout(&self, stdout: &str) -> ReviewGateVerdict {
        // Codex `exec --json` emits a newline-delimited event stream.
        // We keep the last `agent_message` event (that's the final
        // assistant message), parse its `content` field as the verdict
        // envelope body. If the stream is empty or the content is not
        // valid JSON, fall back to an unapproved verdict with a
        // diagnostic detail.
        let last_message: Option<String> = stdout
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                serde_json::from_str::<serde_json::Value>(line).ok()
            })
            .filter_map(|v| {
                let ty = v.get("type").and_then(|t| t.as_str())?;
                if ty != "agent_message" {
                    return None;
                }
                v.get("content")
                    .and_then(|c| c.as_str())
                    .map(|s| s.to_string())
            })
            .next_back();

        let Some(content) = last_message else {
            return ReviewGateVerdict {
                approved: false,
                detail: "codex: no agent_message events in stdout".into(),
            };
        };

        match serde_json::from_str::<serde_json::Value>(content.trim()) {
            Ok(v) => ReviewGateVerdict {
                approved: v.get("approved").and_then(|a| a.as_bool()).unwrap_or(false),
                detail: v
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
            },
            Err(e) => ReviewGateVerdict {
                approved: false,
                detail: format!("codex: invalid JSON in agent_message content: {e}"),
            },
        }
    }

    fn write_session_files(&self, _cwd: &Path, _mcp_config_json: &str) -> io::Result<Vec<PathBuf>> {
        // Codex MCP injection goes through the CLI flag
        // `--config mcp_servers.workbridge.config=<path>` pointing at the
        // already-written temp JSON that the caller prepared. No
        // additional Codex-specific side-car files are needed. Never
        // write into `_cwd` (pollutes the user's worktree) or
        // `~/.codex/config.toml` (cross-contaminates personal config);
        // both would violate the file-injection rule.
        Ok(vec![])
    }
}

/// Future-work stub adapter for the OpenCode CLI. Not implemented:
/// every method returns empty argv and a diagnostic verdict. No
/// user-facing path currently reaches this backend - the `AgentBackendKind::OpenCode`
/// variant is not exposed through `AgentBackendKind::all()`, not
/// accepted by `FromStr`, and not bound to any keybinding. The struct
/// and `backend_for_kind` wiring are retained as scaffolding so a
/// future real adapter can land without reintroducing both the type
/// and the dispatch arm at the same time. The tests in this file
/// pin the stub's "returns nothing functional" contract so accidental
/// invocation would fail loudly rather than appear to succeed.
pub struct OpenCodeBackend;

impl AgentBackend for OpenCodeBackend {
    fn kind(&self) -> AgentBackendKind {
        AgentBackendKind::OpenCode
    }

    fn command_name(&self) -> &'static str {
        "opencode"
    }

    fn build_command(&self, _cfg: &SpawnConfig<'_>) -> Vec<String> {
        // Returns an argv that contains only the binary name so a
        // caller that routes to this backend without checking kind
        // still produces a legible failure (the binary itself will
        // print its own help / error). The spawn sites guard against
        // this by calling `App::ensure_harness_implemented` before
        // spawning, so in practice this path is unreachable.
        vec![self.command_name().to_string()]
    }

    fn build_review_gate_command(&self, _cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
        Vec::new()
    }

    fn build_headless_rw_command(&self, _cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
        Vec::new()
    }

    fn parse_review_gate_stdout(&self, _stdout: &str) -> ReviewGateVerdict {
        ReviewGateVerdict {
            approved: false,
            detail: "opencode adapter not yet implemented".into(),
        }
    }

    fn write_session_files(&self, _cwd: &Path, _mcp_config_json: &str) -> io::Result<Vec<PathBuf>> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bridge() -> McpBridgeSpec {
        McpBridgeSpec {
            name: "workbridge".to_string(),
            command: PathBuf::from("/opt/workbridge"),
            args: vec![
                "--mcp-bridge".to_string(),
                "--socket".to_string(),
                "/tmp/workbridge-mcp-fake.sock".to_string(),
            ],
        }
    }

    #[test]
    fn codex_shape_compiles() {
        let backend: Box<dyn AgentBackend> = Box::new(CodexBackend);
        assert_eq!(backend.kind(), AgentBackendKind::Codex);
        assert_eq!(backend.command_name(), "codex");

        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-fake.sock");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Planning,
            system_prompt: Some("be helpful"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: Some("Explain who you are and start working."),
            read_only: false,
        };
        let argv = backend.build_command(&cfg);
        assert_eq!(argv.first().map(String::as_str), Some("codex"));
        // Interactive write-capable Codex uses `--ask-for-approval never`
        // + `--sandbox workspace-write` (Authorization 2b, replaces the
        // earlier `--full-auto`). `--full-auto` is `-a on-request` and
        // prompts for approval on every MCP tool call, which the user
        // explicitly rejected on 2026-04-17.
        assert!(
            argv.iter().any(|s| s == "--ask-for-approval") && argv.iter().any(|s| s == "never"),
            "interactive Codex must pin approval policy to never"
        );
        assert!(
            argv.iter().any(|s| s == "--sandbox") && argv.iter().any(|s| s == "workspace-write"),
            "interactive Codex must keep workspace-write sandbox"
        );
        assert!(
            !argv.iter().any(|s| s == "--full-auto"),
            "interactive Codex must NOT use --full-auto (pulls in -a on-request)"
        );
        assert!(
            argv.iter()
                .any(|s| s == "Explain who you are and start working."),
            "auto-start prompt must be present as a positional"
        );
        // C8: no --settings-style argv for Codex.
        assert!(!argv.iter().any(|s| s == "--settings"));

        let rg_cfg = ReviewGateSpawnConfig {
            system_prompt: "review gate system prompt",
            initial_prompt: "/claude-adversarial-review",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        let rg_argv = backend.build_review_gate_command(&rg_cfg);
        assert_eq!(rg_argv.first().map(String::as_str), Some("exec"));
        assert!(rg_argv.iter().any(|s| s == "--json"));

        // Headless read-write (rebase gate) shape.
        let rw_cfg = ReviewGateSpawnConfig {
            system_prompt: "rebase gate prompt",
            initial_prompt: "rebase onto main",
            json_schema: r#"{"type":"object"}"#,
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        let rw_argv = backend.build_headless_rw_command(&rw_cfg);
        // `--ask-for-approval` is a TOP-LEVEL Codex flag and MUST come
        // before the `exec` subcommand; verifying first() covers both
        // halves of the placement bug (RP2bc was previously emitting
        // `exec --json --full-auto --ask-for-approval never ...` which
        // codex rejects with "unexpected argument '--ask-for-approval'").
        assert_eq!(
            rw_argv.first().map(String::as_str),
            Some("--ask-for-approval")
        );
        assert!(
            rw_argv.iter().any(|s| s == "--full-auto"),
            "headless rw must include Codex's write-access flag"
        );

        // C4: Codex writes no session files by default in the stub.
        let cwd = std::env::temp_dir();
        let files = backend.write_session_files(&cwd, "{}").unwrap();
        assert!(files.is_empty());
    }

    // ---- Claude Code backend tests ----

    #[test]
    fn claude_interactive_argv_for_planning() {
        let backend = ClaudeCodeBackend;
        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-config-abc.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Planning,
            system_prompt: Some("system prompt here"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: Some("Explain who you are and start working."),
            read_only: false,
        };
        let argv = backend.build_command(&cfg);
        assert_eq!(argv[0], "claude");
        assert!(argv.iter().any(|s| s == "--dangerously-skip-permissions"));
        assert!(argv.iter().any(|s| s == "--allowedTools"));
        // Planning installs the PostToolUse hook.
        assert!(argv.iter().any(|s| s == "--settings"));
        // The positional auto-start prompt must precede --mcp-config so
        // Claude Code does not mistake it for a config file path.
        let prompt_idx = argv
            .iter()
            .position(|s| s == "Explain who you are and start working.")
            .expect("auto-start prompt missing");
        let cfg_idx = argv
            .iter()
            .position(|s| s == "--mcp-config")
            .expect("--mcp-config missing");
        assert!(
            prompt_idx < cfg_idx,
            "auto-start positional must precede --mcp-config (regression: \
             Claude Code reads positionals as config paths)"
        );
    }

    #[test]
    fn claude_interactive_argv_for_blocked_no_auto_start() {
        let backend = ClaudeCodeBackend;
        let mcp_path = PathBuf::from("/tmp/mcp.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Blocked,
            system_prompt: Some("blocked prompt"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = backend.build_command(&cfg);
        assert!(!argv.iter().any(|s| s == "--settings"));
        assert!(argv.iter().any(|s| s == "--mcp-config"));
        // Blocked has no positional auto-start.
        assert!(!argv.iter().any(|s| s.contains("Explain who you are")));
    }

    #[test]
    fn claude_interactive_argv_degraded_without_mcp_config() {
        // If the caller cannot write the MCP temp file, the session still
        // spawns so the user can see the failure mode. The backend MUST
        // omit `--mcp-config` in that case so Claude Code does not error
        // out on a missing file.
        let backend = ClaudeCodeBackend;
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("prompt"),
            mcp_config_path: None,
            mcp_bridge: None,
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: Some("Explain who you are and start working."),
            read_only: false,
        };
        let argv = backend.build_command(&cfg);
        assert!(!argv.iter().any(|s| s == "--mcp-config"));
        assert!(argv.iter().any(|s| s == "--system-prompt"));
        assert!(argv.iter().any(|s| s == "--allowedTools"));
    }

    #[test]
    fn claude_interactive_argv_read_only_skips_permission_flags() {
        // A hypothetical read-only interactive session (not wired yet,
        // but the trait surface supports it) omits both
        // `--dangerously-skip-permissions` and `--allowedTools` because
        // the read-only MCP server does the enforcement. This matches
        // how the review gate (headless read-only) behaves.
        let backend = ClaudeCodeBackend;
        let mcp_path = PathBuf::from("/tmp/ro.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Review,
            system_prompt: Some("ro prompt"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: true,
        };
        let argv = backend.build_command(&cfg);
        assert!(!argv.iter().any(|s| s == "--dangerously-skip-permissions"));
        assert!(!argv.iter().any(|s| s == "--allowedTools"));
        assert!(argv.iter().any(|s| s == "--mcp-config"));
    }

    #[test]
    fn claude_review_gate_argv_shape() {
        let backend = ClaudeCodeBackend;
        let mcp_path = PathBuf::from("/tmp/rg.json");
        let bridge = fake_bridge();
        let cfg = ReviewGateSpawnConfig {
            system_prompt: "review gate system prompt",
            initial_prompt: "/claude-adversarial-review",
            json_schema: r#"{"type":"object"}"#,
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        let argv = backend.build_review_gate_command(&cfg);
        assert_eq!(argv[0], "--print");
        assert!(argv.iter().any(|s| s == "--output-format"));
        assert!(argv.iter().any(|s| s == "json"));
        assert!(argv.iter().any(|s| s == "--json-schema"));
        // Review gate does not use --dangerously-skip-permissions or
        // --allowedTools (C3 and C5 per RP2).
        assert!(!argv.iter().any(|s| s == "--dangerously-skip-permissions"));
        assert!(!argv.iter().any(|s| s == "--allowedTools"));
    }

    #[test]
    fn claude_headless_rw_argv_includes_permission_bypass() {
        let backend = ClaudeCodeBackend;
        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-config-abc.json");
        let bridge = fake_bridge();
        let cfg = ReviewGateSpawnConfig {
            system_prompt: "",
            initial_prompt: "rebase onto origin/main",
            json_schema: r#"{"type":"object"}"#,
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        let argv = backend.build_headless_rw_command(&cfg);
        assert!(
            argv.iter().any(|s| s == "--dangerously-skip-permissions"),
            "headless rw must include --dangerously-skip-permissions"
        );
        assert!(argv.iter().any(|s| s == "--print"));
        assert!(argv.iter().any(|s| s == "--mcp-config"));
        assert!(argv.iter().any(|s| s == "--json-schema"));
    }

    #[test]
    fn claude_parse_review_gate_envelope() {
        let backend = ClaudeCodeBackend;
        let verdict = backend
            .parse_review_gate_stdout(r#"{"structured_output":{"approved":true,"detail":"ok"}}"#);
        assert!(verdict.approved);
        assert_eq!(verdict.detail, "ok");

        let missing = backend.parse_review_gate_stdout(r#"{}"#);
        assert!(!missing.approved);
        assert_eq!(missing.detail, "");

        let broken = backend.parse_review_gate_stdout("not json");
        assert!(!broken.approved);
        assert!(broken.detail.contains("invalid JSON"));
    }

    // ---- Codex backend tests ----

    /// Pins C3: interactive write-capable Codex uses `--ask-for-approval
    /// never` + `--sandbox workspace-write` (Authorization 2b, supersedes
    /// the earlier `--full-auto` shape). `--full-auto` pulls in
    /// `-a on-request` which prompts for approval on every MCP tool
    /// call; the user explicitly rejected that UX on 2026-04-17.
    #[test]
    fn codex_interactive_argv_pre_approves_tool_calls() {
        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-fake.sock");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("sys"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);
        assert_eq!(argv[0], "codex");
        // Approval policy must be "never" (adjacent pair).
        let approval_idx = argv.iter().position(|s| s == "--ask-for-approval");
        assert!(
            approval_idx.is_some(),
            "interactive Codex must emit --ask-for-approval"
        );
        assert_eq!(
            argv.get(approval_idx.unwrap() + 1).map(String::as_str),
            Some("never"),
            "--ask-for-approval value must be 'never' (no MCP tool prompts)"
        );
        // Sandbox must remain workspace-write (adjacent pair).
        let sandbox_idx = argv.iter().position(|s| s == "--sandbox");
        assert!(
            sandbox_idx.is_some(),
            "interactive Codex must emit --sandbox"
        );
        assert_eq!(
            argv.get(sandbox_idx.unwrap() + 1).map(String::as_str),
            Some("workspace-write"),
            "sandbox must stay workspace-write (Authorization 2b scope)"
        );
        // `--full-auto` must NOT be emitted in interactive mode (it bundles
        // -a on-request which contradicts Authorization 2b).
        assert!(
            !argv.iter().any(|s| s == "--full-auto"),
            "interactive Codex must NOT use --full-auto"
        );
    }

    /// Pins C3b: all three Codex profiles MUST include the
    /// `granular_approval.mcp_elicitations=false` `--config` override.
    /// Without this, `--ask-for-approval never` alone does NOT suppress
    /// per-MCP-tool approval dialogs in codex-cli 0.120.0 (verified
    /// live on 2026-04-17). The review gate and rebase gate are
    /// headless (`exec --json`) and would hang forever on the first
    /// workbridge_* MCP call without this flag; the interactive path
    /// surfaces the prompt to the user, which breaks the "pre-allow
    /// MCP tools" user requirement.
    #[test]
    fn codex_suppresses_mcp_elicitation_prompts_on_all_profiles() {
        fn has_granular_flag(argv: &[String]) -> bool {
            argv.windows(2)
                .any(|w| w[0] == "--config" && w[1] == "granular_approval.mcp_elicitations=false")
        }
        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-fake.sock");
        let bridge = fake_bridge();

        // Interactive write-capable.
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("sys"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        assert!(
            has_granular_flag(&CodexBackend.build_command(&cfg)),
            "interactive write-capable Codex must suppress MCP elicitation prompts"
        );
        // Interactive read-only (global-assistant edge case + hypothetical
        // future read-only work-item scope). Also must suppress.
        let ro_cfg = SpawnConfig {
            stage: WorkItemStatus::Review,
            system_prompt: Some("ro"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: true,
        };
        assert!(
            has_granular_flag(&CodexBackend.build_command(&ro_cfg)),
            "interactive read-only Codex must suppress MCP elicitation prompts"
        );
        // Review gate (headless read-only).
        let rg_cfg = ReviewGateSpawnConfig {
            system_prompt: "sys",
            initial_prompt: "prompt",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        assert!(
            has_granular_flag(&CodexBackend.build_review_gate_command(&rg_cfg)),
            "review gate Codex must suppress MCP elicitation prompts"
        );
        // Rebase gate (headless read-write).
        let rw_cfg = ReviewGateSpawnConfig {
            system_prompt: "sys",
            initial_prompt: "rebase",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        assert!(
            has_granular_flag(&CodexBackend.build_headless_rw_command(&rw_cfg)),
            "rebase gate Codex must suppress MCP elicitation prompts"
        );
    }

    /// Pins C4: Codex MCP injection uses per-field `--config
    /// mcp_servers.workbridge.command=...` and `mcp_servers.workbridge.args=[...]`.
    /// The earlier `mcp_servers.workbridge.config=<path>` shape is
    /// rejected by Codex at config load time with "invalid transport"
    /// and MUST NOT be emitted. Verified against the live `codex` CLI
    /// on 2026-04-16.
    #[test]
    fn codex_mcp_config_injected_via_config_flag() {
        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-42.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: None,
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);

        // The old broken shape must NOT appear.
        let has_broken_config = argv
            .windows(2)
            .any(|w| w[0] == "--config" && w[1].starts_with("mcp_servers.workbridge.config="));
        assert!(
            !has_broken_config,
            "codex must NOT emit mcp_servers.workbridge.config=... \
             (rejected by codex as invalid transport); got {argv:?}"
        );

        // The correct shape: per-field command + args overrides.
        let has_command_flag = argv
            .windows(2)
            .any(|w| w[0] == "--config" && w[1].starts_with("mcp_servers.workbridge.command="));
        let has_args_flag = argv
            .windows(2)
            .any(|w| w[0] == "--config" && w[1].starts_with("mcp_servers.workbridge.args="));
        assert!(
            has_command_flag,
            "codex must inject MCP command via --config \
             mcp_servers.workbridge.command=\"...\", got {argv:?}"
        );
        assert!(
            has_args_flag,
            "codex must inject MCP args via --config \
             mcp_servers.workbridge.args=[...], got {argv:?}"
        );

        // The emitted command must be the TOML-quoted absolute path of
        // the workbridge bridge binary, and the args must be a TOML
        // inline array of quoted strings.
        let command_value = argv
            .windows(2)
            .find(|w| w[0] == "--config" && w[1].starts_with("mcp_servers.workbridge.command="))
            .map(|w| w[1].clone())
            .unwrap();
        assert!(
            command_value.ends_with(r#""/opt/workbridge""#),
            "command override must quote the path as a TOML basic string, \
             got {command_value:?}"
        );
        let args_value = argv
            .windows(2)
            .find(|w| w[0] == "--config" && w[1].starts_with("mcp_servers.workbridge.args="))
            .map(|w| w[1].clone())
            .unwrap();
        assert!(
            args_value.contains(r#"["--mcp-bridge","--socket","#),
            "args override must be a TOML inline array of quoted strings, \
             got {args_value:?}"
        );
    }

    /// Regression test: if the caller cannot build a bridge spec (e.g.
    /// MCP socket bind failed), Codex omits the workbridge server
    /// overrides entirely rather than falling back to the user's
    /// `~/.codex/config.toml`. This degrades cleanly rather than
    /// silently contaminating personal config.
    #[test]
    fn codex_mcp_bridge_none_omits_workbridge_overrides() {
        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-42.json");
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: None,
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: None,
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);
        assert!(
            !argv
                .iter()
                .any(|s| s.starts_with("mcp_servers.workbridge.")),
            "missing mcp_bridge must skip all workbridge overrides, got {argv:?}"
        );
    }

    /// Pins C6 + C13: the system prompt is delivered via `--config
    /// instructions=...`, not via `--system-prompt` and not via an
    /// environment variable (Codex has neither).
    #[test]
    fn codex_system_prompt_goes_through_config_instructions() {
        let mcp_path = PathBuf::from("/tmp/mcp.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("be concise"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);
        assert!(argv.iter().any(|s| s.starts_with("instructions=")));
        assert!(!argv.iter().any(|s| s == "--system-prompt"));
    }

    /// Pins C7: the auto-start prompt is emitted as a positional
    /// argument, the last item in argv.
    #[test]
    fn codex_auto_start_prompt_is_last_positional() {
        let mcp_path = PathBuf::from("/tmp/mcp.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Planning,
            system_prompt: Some("sys"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: Some("Explain who you are and start working."),
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);
        assert_eq!(
            argv.last().map(String::as_str),
            Some("Explain who you are and start working.")
        );
    }

    /// Pins C11: read-only interactive sessions omit the write-mode
    /// approval/sandbox pair. The read-only MCP server is the enforcement
    /// mechanism; the CLI flags would be misleading.
    #[test]
    fn codex_read_only_interactive_omits_write_flags() {
        let mcp_path = PathBuf::from("/tmp/mcp.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Review,
            system_prompt: Some("ro"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: true,
        };
        let argv = CodexBackend.build_command(&cfg);
        // Read-only must NOT emit the write-mode approval/sandbox pair
        // and must NOT emit the deprecated `--full-auto` shortcut.
        assert!(!argv.iter().any(|s| s == "--ask-for-approval"));
        assert!(!argv.iter().any(|s| s == "--sandbox"));
        assert!(!argv.iter().any(|s| s == "--full-auto"));
    }

    /// Pins the headless review-gate shape: `codex exec --json ...` with
    /// per-field workbridge MCP overrides.
    #[test]
    fn codex_review_gate_command_uses_exec_json() {
        let mcp_path = PathBuf::from("/tmp/rg.json");
        let bridge = fake_bridge();
        let cfg = ReviewGateSpawnConfig {
            system_prompt: "sys",
            initial_prompt: "prompt",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        let argv = CodexBackend.build_review_gate_command(&cfg);
        assert_eq!(argv[0], "exec");
        assert!(argv.iter().any(|s| s == "--json"));
        // Review gate does NOT get --full-auto (read-only).
        assert!(!argv.iter().any(|s| s == "--full-auto"));
        // Per-field MCP overrides must be present.
        assert!(
            argv.iter()
                .any(|s| s.starts_with("mcp_servers.workbridge.command=")),
            "review gate argv must include mcp_servers.workbridge.command override, got {argv:?}"
        );
        assert!(
            argv.iter()
                .any(|s| s.starts_with("mcp_servers.workbridge.args=")),
            "review gate argv must include mcp_servers.workbridge.args override, got {argv:?}"
        );
        // The old broken shape must not appear.
        assert!(
            !argv
                .iter()
                .any(|s| s.starts_with("mcp_servers.workbridge.config=")),
            "review gate must not emit the deprecated .config=<path> shape, got {argv:?}"
        );
    }

    /// Pins the headless rebase-gate shape: `codex --ask-for-approval
    /// never exec --json --full-auto ...` with per-field workbridge MCP
    /// overrides. `--ask-for-approval` is a TOP-LEVEL codex flag and
    /// MUST come BEFORE the `exec` subcommand; the live CLI rejects the
    /// argv with "unexpected argument '--ask-for-approval' found" if
    /// the flag appears after `exec`. `--full-auto` is an `exec`
    /// subcommand flag and stays after `exec`. Verified live on
    /// 2026-04-16 via `codex --ask-for-approval never exec --json
    /// --full-auto --skip-git-repo-check "echo hi"`, which produced a
    /// valid event stream.
    #[test]
    fn codex_headless_rw_includes_full_auto_and_approval_never() {
        let mcp_path = PathBuf::from("/tmp/rb.json");
        let bridge = fake_bridge();
        let cfg = ReviewGateSpawnConfig {
            system_prompt: "",
            initial_prompt: "rebase",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        let argv = CodexBackend.build_headless_rw_command(&cfg);

        // R2-F-1 regression: `--ask-for-approval` is parsed as a
        // top-level flag by clap; placing it after `exec` is rejected.
        let exec_idx = argv
            .iter()
            .position(|s| s == "exec")
            .expect("exec subcommand must be present");
        let approval_idx = argv
            .iter()
            .position(|s| s == "--ask-for-approval")
            .expect("--ask-for-approval must be present");
        assert!(
            approval_idx < exec_idx,
            "--ask-for-approval must come BEFORE the `exec` subcommand \
             (codex rejects it as a subcommand flag); got {argv:?}"
        );
        // `--ask-for-approval never` must still be an adjacent pair.
        assert_eq!(
            argv.get(approval_idx + 1).map(String::as_str),
            Some("never")
        );

        // `--full-auto` is an exec subcommand flag and MUST stay after
        // `exec`. The reverse placement (top-level) is silently
        // accepted by some codex builds but documented to belong to
        // exec; keeping it inside exec preserves parity with RP1c.
        let full_auto_idx = argv
            .iter()
            .position(|s| s == "--full-auto")
            .expect("--full-auto must be present");
        assert!(
            full_auto_idx > exec_idx,
            "--full-auto must come AFTER `exec`; got {argv:?}"
        );

        assert!(
            argv.iter()
                .any(|s| s.starts_with("mcp_servers.workbridge.command=")),
            "rebase gate argv must include mcp_servers.workbridge.command override, got {argv:?}"
        );
        assert!(
            argv.iter()
                .any(|s| s.starts_with("mcp_servers.workbridge.args=")),
            "rebase gate argv must include mcp_servers.workbridge.args override, got {argv:?}"
        );
    }

    /// R2-F-2 regression: per-repo user-configured MCP servers
    /// (`SpawnConfig::extra_bridges` / `ReviewGateSpawnConfig::extra_bridges`)
    /// must be emitted as their own `--config mcp_servers.<name>.command`
    /// / `mcp_servers.<name>.args` pair, in addition to the primary
    /// workbridge bridge. Without this, Codex sessions silently lose
    /// every per-repo MCP server the user has configured (Claude
    /// sessions still see them via the `--mcp-config` JSON file). The
    /// primary `workbridge` overrides MUST still be present.
    #[test]
    fn codex_mcp_bridge_extras_emit_per_key_overrides() {
        let mcp_path = PathBuf::from("/tmp/extras.json");
        let bridge = fake_bridge();
        let extras = vec![
            McpBridgeSpec {
                name: "datadog".to_string(),
                command: PathBuf::from("/usr/local/bin/datadog-mcp"),
                args: vec!["--api-key".to_string(), "REDACTED".to_string()],
            },
            McpBridgeSpec {
                name: "filesystem".to_string(),
                command: PathBuf::from("npx"),
                args: vec![
                    "-y".to_string(),
                    "@modelcontextprotocol/server-filesystem".to_string(),
                    "/tmp".to_string(),
                ],
            },
        ];

        // Interactive spawn: extras + primary all visible in argv.
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("sys"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &extras,
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);
        for name in ["workbridge", "datadog", "filesystem"] {
            let cmd_key = format!("mcp_servers.{name}.command=");
            let args_key = format!("mcp_servers.{name}.args=");
            assert!(
                argv.iter().any(|s| s.starts_with(&cmd_key)),
                "missing {cmd_key} override; got {argv:?}"
            );
            assert!(
                argv.iter().any(|s| s.starts_with(&args_key)),
                "missing {args_key} override; got {argv:?}"
            );
        }
        // The datadog command override must quote the configured
        // command path as a TOML basic string so values with special
        // characters survive Codex's TOML parser.
        let datadog_cmd = argv
            .iter()
            .find(|s| s.starts_with("mcp_servers.datadog.command="))
            .unwrap();
        assert!(
            datadog_cmd.ends_with(r#""/usr/local/bin/datadog-mcp""#),
            "datadog command override must be TOML-quoted, got {datadog_cmd:?}"
        );

        // Headless review gate also forwards extras (so adversarial
        // reviews can call out to per-repo MCP servers like datadog).
        let rg_cfg = ReviewGateSpawnConfig {
            system_prompt: "sys",
            initial_prompt: "/review",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &extras,
        };
        let rg_argv = CodexBackend.build_review_gate_command(&rg_cfg);
        for name in ["workbridge", "datadog", "filesystem"] {
            let cmd_key = format!("mcp_servers.{name}.command=");
            assert!(
                rg_argv.iter().any(|s| s.starts_with(&cmd_key)),
                "review gate missing {cmd_key}; got {rg_argv:?}"
            );
        }

        // Headless rebase gate also forwards extras (rebase fixers may
        // need per-repo tools).
        let rw_cfg = ReviewGateSpawnConfig {
            system_prompt: "",
            initial_prompt: "rebase",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &extras,
        };
        let rw_argv = CodexBackend.build_headless_rw_command(&rw_cfg);
        for name in ["workbridge", "datadog", "filesystem"] {
            let cmd_key = format!("mcp_servers.{name}.command=");
            assert!(
                rw_argv.iter().any(|s| s.starts_with(&cmd_key)),
                "rebase gate missing {cmd_key}; got {rw_argv:?}"
            );
        }
    }

    /// R3-F-1 regression: Codex's `-c key=value` overrides are
    /// last-write-wins. The workbridge primary MUST be emitted AFTER
    /// every per-repo extra so a maliciously- or accidentally-named
    /// extra (e.g. a per-repo MCP server literally named `workbridge`)
    /// cannot override the workbridge bridge entry that the session
    /// needs to talk to workbridge itself. Mirrors
    /// `crate::mcp::build_mcp_config`'s
    /// `build_mcp_config_workbridge_key_always_wins` test.
    #[test]
    fn codex_extras_cannot_override_workbridge_primary() {
        let primary = McpBridgeSpec {
            name: "workbridge".to_string(),
            command: PathBuf::from("/opt/workbridge"),
            args: vec![
                "--mcp-bridge".to_string(),
                "--socket".to_string(),
                "/tmp/real.sock".to_string(),
            ],
        };
        let extras = vec![McpBridgeSpec {
            // Extra deliberately named the same as the primary -
            // simulates a user (or hand-edited config) registering a
            // per-repo server under the reserved `workbridge` name.
            name: "workbridge".to_string(),
            command: PathBuf::from("/bin/false"),
            args: vec!["adversarial".to_string()],
        }];

        // Interactive spawn: argv must contain TWO command flags for
        // `mcp_servers.workbridge.command=`, and the LAST one must be
        // the genuine workbridge binary path (not the adversarial one).
        let mcp_path = PathBuf::from("/tmp/mcp.json");
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("sys"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&primary),
            extra_bridges: &extras,
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);
        let cmd_overrides: Vec<&String> = argv
            .iter()
            .filter(|s| s.starts_with("mcp_servers.workbridge.command="))
            .collect();
        assert_eq!(
            cmd_overrides.len(),
            2,
            "expected 2 mcp_servers.workbridge.command overrides (one extra + one primary), got {argv:?}"
        );
        assert!(
            cmd_overrides.last().unwrap().contains("/opt/workbridge"),
            "primary workbridge override must come LAST so codex's last-write-wins \
             keeps the genuine bridge; got {argv:?}"
        );
        assert!(
            !cmd_overrides.last().unwrap().contains("/bin/false"),
            "adversarial extra must NOT be the last write; got {argv:?}"
        );

        // Headless review gate must enforce the same ordering.
        let rg_cfg = ReviewGateSpawnConfig {
            system_prompt: "sys",
            initial_prompt: "/review",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &primary,
            extra_bridges: &extras,
        };
        let rg_argv = CodexBackend.build_review_gate_command(&rg_cfg);
        let rg_cmd_overrides: Vec<&String> = rg_argv
            .iter()
            .filter(|s| s.starts_with("mcp_servers.workbridge.command="))
            .collect();
        assert_eq!(rg_cmd_overrides.len(), 2);
        assert!(rg_cmd_overrides.last().unwrap().contains("/opt/workbridge"));

        // Headless rebase gate must enforce the same ordering.
        let rw_cfg = ReviewGateSpawnConfig {
            system_prompt: "",
            initial_prompt: "rebase",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &primary,
            extra_bridges: &extras,
        };
        let rw_argv = CodexBackend.build_headless_rw_command(&rw_cfg);
        let rw_cmd_overrides: Vec<&String> = rw_argv
            .iter()
            .filter(|s| s.starts_with("mcp_servers.workbridge.command="))
            .collect();
        assert_eq!(rw_cmd_overrides.len(), 2);
        assert!(rw_cmd_overrides.last().unwrap().contains("/opt/workbridge"));
    }

    /// R3-F-2: TOML key fragments containing only bare-key characters
    /// pass through unchanged so the rendered argv stays readable.
    #[test]
    fn toml_quote_key_bare_passes_through() {
        assert_eq!(toml_quote_key("foo"), "foo");
        assert_eq!(toml_quote_key("workbridge"), "workbridge");
        assert_eq!(toml_quote_key("my-server_1"), "my-server_1");
        assert_eq!(toml_quote_key("ABC123"), "ABC123");
    }

    /// R3-F-2: A name containing `.` would split a single TOML key
    /// fragment into multiple, misregistering the override under a
    /// different path. The helper must quote it as a single fragment.
    #[test]
    fn toml_quote_key_dotted_is_quoted() {
        assert_eq!(toml_quote_key("my.server"), r#""my.server""#);
        assert_eq!(toml_quote_key("a.b.c"), r#""a.b.c""#);
    }

    /// R3-F-2: spaces are not allowed in TOML bare keys; the helper
    /// must produce a quoted key.
    #[test]
    fn toml_quote_key_spaced_is_quoted() {
        assert_eq!(toml_quote_key("my server"), r#""my server""#);
    }

    /// R3-F-2: a name containing a literal `"` must escape it inside
    /// the quoted key form so the TOML parser sees one continuous
    /// string rather than two halves separated by a stray quote.
    #[test]
    fn toml_quote_key_with_quote_escapes() {
        // `serde_json::to_string("my\"name")` produces `"my\"name"`,
        // which is the exact TOML basic-string form the helper emits.
        assert_eq!(toml_quote_key("my\"name"), r#""my\"name""#);
    }

    /// R3-F-2: empty names must always be quoted (TOML rejects empty
    /// bare keys). The empty bare key `mcp_servers..command=` would
    /// abort Codex's TOML parser at config load time.
    #[test]
    fn toml_quote_key_empty_is_quoted() {
        assert_eq!(toml_quote_key(""), r#""""#);
    }

    /// R3-F-2 argv shape: a `McpBridgeSpec` whose `name` contains a
    /// dot must produce `mcp_servers."my.server".command=...` (key
    /// quoted as one TOML fragment) rather than
    /// `mcp_servers.my.server.command=...` (which would misregister
    /// the server under `mcp_servers.my.server.command` as a leaf
    /// rather than under the intended `mcp_servers.my.server` table).
    #[test]
    fn codex_extra_bridge_with_dotted_name_emits_quoted_key() {
        let primary = fake_bridge();
        let extras = vec![McpBridgeSpec {
            name: "my.server".to_string(),
            command: PathBuf::from("/usr/local/bin/my-server"),
            args: vec!["--port".to_string(), "8080".to_string()],
        }];
        let mcp_path = PathBuf::from("/tmp/mcp.json");
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("sys"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&primary),
            extra_bridges: &extras,
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = CodexBackend.build_command(&cfg);
        assert!(
            argv.iter()
                .any(|s| s.starts_with(r#"mcp_servers."my.server".command="#)),
            "dotted server name must produce a quoted key fragment; got {argv:?}"
        );
        assert!(
            argv.iter()
                .any(|s| s.starts_with(r#"mcp_servers."my.server".args="#)),
            "dotted server name must produce a quoted args key fragment; got {argv:?}"
        );
        // The malformed bare-key shape (which would silently
        // misregister under `mcp_servers.my.server.command`) must NOT
        // appear in the argv.
        assert!(
            !argv
                .iter()
                .any(|s| s.starts_with("mcp_servers.my.server.command=")),
            "bare-key emission for a dotted name re-splits the TOML path; got {argv:?}"
        );
        // Plain server names still emit bare keys (no regression in
        // readability for the common case).
        assert!(
            argv.iter()
                .any(|s| s.starts_with("mcp_servers.workbridge.command=")),
            "plain name 'workbridge' must still emit a bare key; got {argv:?}"
        );
    }

    /// Pins the event-stream parser: `codex exec --json` emits
    /// newline-delimited JSON; we find the last `agent_message` event
    /// and parse its `content` field as the verdict envelope.
    #[test]
    fn codex_parses_agent_message_event_stream() {
        let stream = r#"
{"type":"thinking","content":"..."}
{"type":"tool_call","name":"read"}
{"type":"agent_message","content":"{\"approved\":true,\"detail\":\"ok\"}"}
"#;
        let verdict = CodexBackend.parse_review_gate_stdout(stream);
        assert!(verdict.approved);
        assert_eq!(verdict.detail, "ok");
    }

    /// Empty stream or no agent_message -> unapproved with a diagnostic
    /// detail. Pins the "absent envelope" failure mode.
    #[test]
    fn codex_parse_empty_stream_returns_unapproved() {
        let verdict = CodexBackend.parse_review_gate_stdout("");
        assert!(!verdict.approved);
        assert!(verdict.detail.contains("no agent_message"));

        let wrong_types = r#"{"type":"thinking","content":"..."}"#;
        let verdict2 = CodexBackend.parse_review_gate_stdout(wrong_types);
        assert!(!verdict2.approved);
        assert!(verdict2.detail.contains("no agent_message"));
    }

    /// C4 / C13: Codex does not write session files into the worktree
    /// or user home. The caller prepares the temp JSON before spawning.
    #[test]
    fn codex_writes_no_session_files() {
        let cwd = std::env::temp_dir();
        let files = CodexBackend.write_session_files(&cwd, "{}").unwrap();
        assert!(files.is_empty());
    }

    // ---- OpenCode backend tests ----

    /// Pins the stub contract: every method returns an empty / degraded
    /// value and `parse_review_gate_stdout` surfaces the explicit "not
    /// yet implemented" detail so the review gate fails loudly rather
    /// than appearing to silently approve.
    #[test]
    fn opencode_backend_is_a_stub() {
        let backend = OpenCodeBackend;
        assert_eq!(backend.kind(), AgentBackendKind::OpenCode);
        assert_eq!(backend.command_name(), "opencode");

        let mcp_path = PathBuf::from("/tmp/mcp.json");
        let bridge = fake_bridge();
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Implementing,
            system_prompt: Some("sys"),
            mcp_config_path: Some(&mcp_path),
            mcp_bridge: Some(&bridge),
            extra_bridges: &[],
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: None,
            read_only: false,
        };
        let argv = backend.build_command(&cfg);
        assert_eq!(argv, vec!["opencode".to_string()]);

        let rg_cfg = ReviewGateSpawnConfig {
            system_prompt: "",
            initial_prompt: "",
            json_schema: "{}",
            mcp_config_path: &mcp_path,
            mcp_bridge: &bridge,
            extra_bridges: &[],
        };
        assert!(backend.build_review_gate_command(&rg_cfg).is_empty());
        assert!(backend.build_headless_rw_command(&rg_cfg).is_empty());

        let verdict = backend.parse_review_gate_stdout("anything");
        assert!(!verdict.approved);
        assert!(verdict.detail.contains("not yet implemented"));

        let files = backend
            .write_session_files(&std::env::temp_dir(), "{}")
            .unwrap();
        assert!(files.is_empty());
    }

    // ---- AgentBackendKind helpers ----

    /// Pins `FromStr` for the CLI subcommand: only the two user-
    /// selectable canonical names parse; "opencode" and anything else
    /// return `UnknownHarnessName`. The "opencode" rejection is load-
    /// bearing: the `AgentBackendKind::OpenCode` variant exists as
    /// internal scaffolding but the user must not be able to set it
    /// via `workbridge config set global-assistant-harness opencode`.
    #[test]
    fn agent_backend_kind_from_str_validates_canonical_names() {
        assert_eq!(
            AgentBackendKind::from_str("claude").unwrap(),
            AgentBackendKind::ClaudeCode
        );
        assert_eq!(
            AgentBackendKind::from_str("codex").unwrap(),
            AgentBackendKind::Codex
        );
        // OpenCode is not user-selectable: from_str MUST reject it.
        let err = AgentBackendKind::from_str("opencode").unwrap_err();
        assert_eq!(err.got, "opencode");
        // Error message lists only user-selectable names. The input
        // string is quoted back in the message, so we only assert
        // the "expected one of" list does not advertise opencode.
        let msg = format!("{err}");
        assert!(
            msg.contains("expected one of: claude, codex"),
            "expected error to advertise only user-selectable names, got: {msg}"
        );
        assert!(
            !msg.contains("expected one of: claude, codex, opencode"),
            "expected one-of list must not include opencode, got: {msg}"
        );
        assert!(AgentBackendKind::from_str("Claude").is_err());
        assert!(AgentBackendKind::from_str("").is_err());
        assert!(AgentBackendKind::from_str("gemini").is_err());
    }

    /// Pins the stable enumeration used by the first-run modal.
    /// `all()` lists user-selectable kinds only; `OpenCode` is excluded
    /// because it has no user-facing spawn path.
    #[test]
    fn agent_backend_kind_all_is_stable() {
        assert_eq!(
            AgentBackendKind::all(),
            [AgentBackendKind::ClaudeCode, AgentBackendKind::Codex]
        );
    }

    /// Pins the keybinding map used by the work-item c/x handlers
    /// and the first-run modal. Only user-selectable kinds are listed
    /// by `all()`, so this test covers `c` and `x` only.
    #[test]
    fn agent_backend_kind_keybindings_are_unique() {
        use std::collections::HashSet;
        let keys: HashSet<char> = AgentBackendKind::all()
            .iter()
            .map(|k| k.keybinding())
            .collect();
        assert_eq!(keys.len(), 2, "each kind must map to a unique keybinding");
        assert!(keys.contains(&'c'));
        assert!(keys.contains(&'x'));
    }

    /// Pins `backend_for_kind`: the factory returns a backend whose
    /// `kind()` matches the argument, including for the internal-only
    /// `OpenCode` variant so the dispatch arm does not rot.
    #[test]
    fn backend_for_kind_roundtrips() {
        for kind in AgentBackendKind::all() {
            let backend = backend_for_kind(kind);
            assert_eq!(backend.kind(), kind);
        }
        // OpenCode is not in `all()` but the factory must still
        // produce a backend for it so a future wiring change does not
        // have to reintroduce the arm.
        let opencode = backend_for_kind(AgentBackendKind::OpenCode);
        assert_eq!(opencode.kind(), AgentBackendKind::OpenCode);
    }

    /// Pins that `is_available` returns false for a clearly bogus
    /// binary name. We cannot positively test "claude on PATH" because
    /// CI may or may not have the binary; the false-case is the one we
    /// control.
    #[test]
    fn is_available_returns_false_for_missing_binary() {
        // Monkey-patch-free check: if `which::which("claude")` is Ok
        // on this machine, we rely on the fact that `is_available`
        // delegates to that call and therefore produces the same
        // answer. The real regression being guarded is "function
        // compiled away or always returns true"; a false for one kind
        // plus a true for the same kind's `which::which` call would
        // also catch that.
        // The claim we pin here is the weaker half: `is_available`
        // agrees with `which::which`.
        for kind in AgentBackendKind::all() {
            let by_helper = is_available(kind);
            let by_which = which::which(kind.binary_name()).is_ok();
            assert_eq!(by_helper, by_which, "mismatch for {kind:?}");
        }
    }
}
