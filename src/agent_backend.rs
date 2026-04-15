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

use crate::work_item::WorkItemStatus;

/// Discriminant for logging and config parity. Not used for dispatch
/// (dispatch goes through `dyn AgentBackend`); kept so the runtime can
/// identify which backend is active without downcasting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentBackendKind {
    /// Anthropic Claude Code CLI. Reference implementation.
    ClaudeCode,
    /// Shape-verification target. Not wired into spawn sites; exists so
    /// `codex_shape_compiles` can prove the trait fits a second harness.
    #[cfg(test)]
    Codex,
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
///    write the file inside `write_session_files` and return the path
///    so the caller cleans it up - do NOT set environment variables
///    (C13) and do NOT mutate the user's dotfiles (see the file-injection
///    prohibition in the doc's C2 section).
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

    /// Parse the verdict envelope produced by a headless review-gate
    /// session. Backends that emit a single JSON document (Claude's
    /// `--output-format json`) reach into the envelope's structured body
    /// directly; backends that emit an event stream (Codex `exec --json`)
    /// keep only the last relevant event before calling this function.
    fn parse_review_gate_stdout(&self, stdout: &str) -> ReviewGateVerdict;

    /// Write backend-specific files required before spawn. For Claude this
    /// writes `.mcp.json` into the work-item worktree so Claude Code's
    /// project discovery picks it up in addition to `--mcp-config`. For
    /// backends that route MCP injection through a different mechanism
    /// (e.g. Codex's `config.toml`), this is where that file is written.
    ///
    /// Returns the list of paths the backend created. The caller MUST
    /// pass the same list back to `cleanup_session_files` when the
    /// session ends so nothing leaks.
    ///
    /// `cwd` is the session's working directory (C2); the backend decides
    /// whether to actually write anything there. Called only for spawn
    /// sites that own a worktree-scoped cwd (work-item spawns today);
    /// global-assistant and review-gate spawns skip this step because
    /// they use scratch cwds that should not be polluted.
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
/// the binary name, every CLI flag, the `PostToolUse` planning hook, the
/// `.mcp.json` convention, and the JSON envelope shape for headless
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

    fn write_session_files(&self, cwd: &Path, mcp_config_json: &str) -> io::Result<Vec<PathBuf>> {
        // Claude Code's project-discovery path reads `.mcp.json` from the
        // worktree root. We write it in addition to `--mcp-config` so the
        // discovery path has a known-good config if the flag is ever
        // dropped. See `docs/harness-contract.md` C4 for the rationale.
        let path = cwd.join(".mcp.json");
        std::fs::write(&path, mcp_config_json)?;
        Ok(vec![path])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape-verification stub for a second harness.
    ///
    /// **This is not a working backend.** It exists exclusively to prove
    /// that the trait shape is harness-neutral enough to host a second
    /// CLI whose feature set differs from Claude Code's on the three
    /// clauses that matter most (C4 MCP injection, C6 system prompt
    /// shape, C8 stage reminder). If a future change to `AgentBackend`
    /// breaks this stub, the trait has been Claude-Code-coupled and the
    /// change needs a rework.
    ///
    /// The argv shapes are derived from the public Codex CLI surface as
    /// of 2026-04-15 (`codex`, `codex exec --json`, `--cd`, `--full-auto`,
    /// `--config`) and from the Codex column of the Implementation Map
    /// in `docs/harness-contract.md`. They are NOT exercised against a
    /// real Codex process.
    struct CodexBackend;

    impl AgentBackend for CodexBackend {
        fn kind(&self) -> AgentBackendKind {
            AgentBackendKind::Codex
        }

        fn command_name(&self) -> &'static str {
            "codex"
        }

        fn build_command(&self, cfg: &SpawnConfig<'_>) -> Vec<String> {
            let mut cmd = vec![self.command_name().to_string()];
            // C3: Codex's `--full-auto` matches Claude's
            // `--dangerously-skip-permissions`.
            cmd.push("--full-auto".to_string());
            // C4: Codex has no `--mcp-config` flag. The real adapter would
            // write a temp `config.toml` inside `write_session_files` and
            // pass it via `--config`. For shape verification we reference
            // the path the caller prepared. When `None`, the adapter
            // would fall back to the user's global config.toml.
            if let Some(path) = cfg.mcp_config_path {
                cmd.push("--config".to_string());
                cmd.push(format!("mcp_servers.workbridge.config={}", path.display()));
            }
            // C5: Codex has no per-tool CLI allowlist; the clause is met
            // by the MCP server filter (the same mechanism the Claude
            // review gate already uses). The allowlist is still attached
            // here for audit parity so a future adapter can refuse to
            // start if a tool it does not know about is requested.
            let _ = cfg.allowed_tools;
            // C6: Codex has no `--system-prompt`; the harness-neutral
            // escape hatch is to prepend the stage prompt as an initial
            // user message. Shown here as `--config instructions=...`
            // for shape; a real adapter may choose stdin instead.
            if let Some(prompt) = cfg.system_prompt {
                cmd.push("--config".to_string());
                cmd.push(format!("instructions={prompt}"));
            }
            // C8: Codex has no PostToolUse hook equivalent. The clause
            // accepts "no extra reminder beyond the system prompt"; this
            // is explicit rather than implicit so a reviewer can see the
            // gap.
            if cfg.stage == WorkItemStatus::Planning {
                // intentionally empty: the planning reminder is embedded
                // in the system prompt for Codex.
            }
            // C7: Auto-start message as positional.
            if let Some(msg) = cfg.auto_start_message {
                cmd.push(msg.to_string());
            }
            // C2: Codex takes `--cd` to set the child cwd. The caller
            // passes the cwd via `Session::spawn` already (same as
            // Claude), so we do NOT add `--cd` here; the clause is met by
            // the PTY's own cwd. The line is included as a comment so
            // the adapter author knows the flag exists.
            cmd
        }

        fn build_review_gate_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
            // Codex's headless mode is `codex exec --json`, which emits a
            // newline-delimited event stream rather than a single JSON
            // document. A real adapter would keep only the last
            // `agent_message` event from the stream and hand it to
            // `parse_review_gate_stdout`.
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--config".to_string(),
                format!("instructions={}", cfg.system_prompt),
                "--config".to_string(),
                format!(
                    "mcp_servers.workbridge.config={}",
                    cfg.mcp_config_path.display()
                ),
                cfg.initial_prompt.to_string(),
            ]
        }

        fn parse_review_gate_stdout(&self, stdout: &str) -> ReviewGateVerdict {
            // A real adapter would parse the event stream. For shape
            // verification we accept a plain JSON object with `approved`
            // and `detail` fields.
            match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                Ok(v) => ReviewGateVerdict {
                    approved: v["approved"].as_bool().unwrap_or(false),
                    detail: v["detail"].as_str().unwrap_or("").to_string(),
                },
                Err(e) => ReviewGateVerdict {
                    approved: false,
                    detail: format!("codex shape stub: {e}"),
                },
            }
        }

        fn write_session_files(
            &self,
            _cwd: &Path,
            _mcp_config_json: &str,
        ) -> io::Result<Vec<PathBuf>> {
            // A real Codex adapter would write a temp `config.toml` here.
            // The shape stub returns an empty vec to keep the test
            // hermetic.
            Ok(Vec::new())
        }
    }

    #[test]
    fn codex_shape_compiles() {
        let backend: Box<dyn AgentBackend> = Box::new(CodexBackend);
        assert_eq!(backend.kind(), AgentBackendKind::Codex);
        assert_eq!(backend.command_name(), "codex");

        let mcp_path = PathBuf::from("/tmp/workbridge-mcp-fake.sock");
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Planning,
            system_prompt: Some("be helpful"),
            mcp_config_path: Some(&mcp_path),
            allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
            auto_start_message: Some("Explain who you are and start working."),
            read_only: false,
        };
        let argv = backend.build_command(&cfg);
        assert_eq!(argv.first().map(String::as_str), Some("codex"));
        assert!(argv.iter().any(|s| s == "--full-auto"));
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
        };
        let rg_argv = backend.build_review_gate_command(&rg_cfg);
        assert_eq!(rg_argv.first().map(String::as_str), Some("exec"));
        assert!(rg_argv.iter().any(|s| s == "--json"));

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
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Planning,
            system_prompt: Some("system prompt here"),
            mcp_config_path: Some(&mcp_path),
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
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Blocked,
            system_prompt: Some("blocked prompt"),
            mcp_config_path: Some(&mcp_path),
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
        let cfg = SpawnConfig {
            stage: WorkItemStatus::Review,
            system_prompt: Some("ro prompt"),
            mcp_config_path: Some(&mcp_path),
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
        let cfg = ReviewGateSpawnConfig {
            system_prompt: "review gate system prompt",
            initial_prompt: "/claude-adversarial-review",
            json_schema: r#"{"type":"object"}"#,
            mcp_config_path: &mcp_path,
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
}
