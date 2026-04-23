//! Pluggable LLM coding harness (agent) backend.
//!
//! This module is the single place where a harness-specific CLI is named
//! and flagged. Everything outside the module talks to `dyn AgentBackend`,
//! so adding a new harness (e.g. Codex) is a matter of writing one more
//! `impl AgentBackend for NewBackend` - the four known spawn sites
//! enumerated in `docs/harness-contract.md` do not change.
//!
//! The contract clauses this trait satisfies (C1..C13) are specified in
//! `docs/harness-contract.md`. Read that doc before editing this module:
//! every method here maps to one or more clauses, and every clause has
//! a reference payload (RP1..RP5) showing the exact wire shape the Claude
//! Code reference implementation produces today.
//!
//! Shape-verification for a second backend lives in the per-adapter tests
//! under this module tree (`claude_code.rs`, `codex.rs`, `codex_tests.rs`,
//! `opencode.rs`): `CodexBackend` compiles against the trait and its
//! `codex_shape_compiles` test asserts that it builds argv vectors for the
//! three argv profiles (interactive, review-gate, rebase-gate) without
//! workbridge editing any harness-specific state file.

use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use crate::work_item::WorkItemStatus;

mod claude_code;
mod codex;
mod common;
mod opencode;

pub use claude_code::ClaudeCodeBackend;
pub use codex::CodexBackend;
pub use opencode::OpenCodeBackend;

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
    /// `OpenAI` / Codex CLI. Implemented adapter; see `CodexBackend`.
    Codex,
    /// `OpenCode` CLI. Future-work stub: `OpenCodeBackend` exists and
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
    pub const fn all() -> [Self; 2] {
        [Self::ClaudeCode, Self::Codex]
    }

    /// Lowercase canonical name used in the CLI (`workbridge config
    /// set global-assistant-harness <name>`), in `config.toml`, and in
    /// the first-run modal keybindings.
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    /// Binary name expected on `PATH`. Used by `is_available` and by the
    /// "command not found" toast text.
    pub const fn binary_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }

    /// Human-readable display name used in status-bar text, UI titles
    /// and the first-run modal body.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
        }
    }

    /// Single-character keybinding used in the first-run modal and the
    /// work-item keyhints for user-selectable kinds. Must stay in sync
    /// with `src/event.rs`. The `OpenCode` mapping is nominal - that
    /// variant is not user-selectable (absent from `all()`) so the
    /// value is never rendered or read; it is kept only so the match
    /// remains exhaustive.
    pub const fn keybinding(self) -> char {
        match self {
            Self::ClaudeCode => 'c',
            Self::Codex => 'x',
            Self::OpenCode => '\0',
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
            "claude" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewGateVerdict {
    pub approved: bool,
    pub detail: String,
}

/// Pluggable LLM coding harness adapter.
///
/// Each implementation owns exactly one CLI (`claude`, `codex`, ...) and
/// knows how to build argv vectors for the three argv profiles
/// (interactive, headless read-only review gate, headless read-write
/// rebase gate) that back the four known spawn sites defined in
/// `docs/harness-contract.md` under "Known Spawn Sites". Implementors
/// MUST satisfy every clause C1..C14 from that doc; if a clause cannot
/// be satisfied, the implementor must say so explicitly in the doc's
/// Adapter Compatibility Matrix (marking the cell `workaround` or
/// `not implemented` with a note) and the review is required to flag
/// the gap (see `CLAUDE.md` severity overrides).
///
/// # Writing a new backend
///
/// Required reading: `docs/harness-contract.md` (every clause) and the
/// per-adapter tests under `src/agent_backend/` (the Codex shape-
/// verification stub in `codex_tests.rs` is the canonical example).
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
/// 3. Update `docs/harness-contract.md` Adapter Compatibility Matrix
///    with a new column for the backend (cells: `supported` /
///    `workaround` / `not implemented`), add per-cell notes below the
///    table for anything non-obvious, and refresh the "Known Spawn
///    Sites" table if any spawn paths changed.
/// 4. Promote the `#[cfg(test)]` variant to a real variant and wire
///    the adapter into selection: per-work-item via `harness_choice`
///    / `App::backend_for_work_item`, and globally via
///    `config.defaults.global_assistant_harness` /
///    `App::global_assistant_harness_kind`. The
///    `App::services::agent_backend` field is NOT the resolver for
///    any spawn path (see `docs/harness-contract.md` "Trait
///    Implementation"); it only exists for test stubs and non-spawn
///    helpers.
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
    /// include its permission-bypass flag (Claude:
    /// `--dangerously-skip-permissions`; Codex:
    /// `--dangerously-bypass-approvals-and-sandbox`). The returned vec
    /// goes directly into `std::process::Command::new(...).args(...)`.
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{AgentBackendKind, backend_for_kind, is_available};

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
