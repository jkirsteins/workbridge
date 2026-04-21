//! Anthropic Claude Code (`claude`) CLI adapter.
//!
//! Reference implementation: every piece of Claude-specific knowledge
//! in workbridge lives here: the binary name, every CLI flag, the
//! `PostToolUse` planning hook, and the JSON envelope shape for
//! headless output. Nothing else in `src/` mentions these.

use std::io;
use std::path::{Path, PathBuf};

use super::{
    AgentBackend, AgentBackendKind, ReviewGateSpawnConfig, ReviewGateVerdict, SpawnConfig,
};
use crate::work_item::WorkItemStatus;

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::{
        McpBridgeSpec, ReviewGateSpawnConfig, SpawnConfig, WORK_ITEM_ALLOWED_TOOLS,
    };
    use super::*;
    use crate::work_item::WorkItemStatus;

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

        let missing = backend.parse_review_gate_stdout(r"{}");
        assert!(!missing.approved);
        assert_eq!(missing.detail, "");

        let broken = backend.parse_review_gate_stdout("not json");
        assert!(!broken.approved);
        assert!(broken.detail.contains("invalid JSON"));
    }
}
