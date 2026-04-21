//! `OpenAI` Codex (`codex`) CLI adapter.

use std::io;
use std::path::{Path, PathBuf};

use super::common::{toml_quote_key, toml_quote_string, toml_quote_string_array};
use super::{
    AgentBackend, AgentBackendKind, McpBridgeSpec, ReviewGateSpawnConfig, ReviewGateVerdict,
    SpawnConfig,
};

/// Real adapter for the `OpenAI` Codex CLI (`codex`). Satisfies C1..C13 per
/// `docs/harness-contract.md`; see the "Codex" column of the
/// Implementation Map for the per-clause workaround details.
///
/// Argv shape summary:
///
/// - Interactive (work-item / global): `codex
///   --dangerously-bypass-approvals-and-sandbox
///   --config mcp_servers.workbridge.command="<exe>"
///   --config mcp_servers.workbridge.args=["--mcp-bridge","--socket","<sock>"]
///   [--config instructions="<sys>"] [<auto-start>]` (`RP1c`).
///   No sandbox; linked-worktree layout is incompatible with Codex's
///   `workspace-write` sandbox - see README "Per-harness permission
///   model" for the full rationale.
/// - Headless read-only (review gate): `codex
///   --dangerously-bypass-approvals-and-sandbox exec --json
///   --config instructions="<sys>"
///   --config mcp_servers.workbridge.command="<exe>"
///   --config mcp_servers.workbridge.args=[...] <prompt>` (`RP2c`).
///   The dangerous flag is emitted even for this conceptually read-only
///   path, for symmetry across the three Codex spawn sites and so
///   review skills that shell out (e.g. `cargo check`) are not silently
///   denied.
/// - Headless read-write (rebase gate): `codex
///   --dangerously-bypass-approvals-and-sandbox exec --json
///   --config mcp_servers.workbridge.command="<exe>"
///   --config mcp_servers.workbridge.args=[...] <prompt>` (`RP2bc`).
///   The dangerous flag is a TOP-LEVEL codex flag and MUST precede
///   `exec` (clap rejects top-level flags inside the `exec` subcommand).
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
/// `write_session_files` go under the process temp dir only (reached
/// through `crate::side_effects::paths::temp_dir`).
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
        // C3b: set `default_tools_approval_mode = "approve"` for every
        // bridge so Codex auto-approves all tool calls from this MCP
        // server without prompting the user. This is the ONLY path that
        // actually suppresses the "Allow the <server> to run tool ..."
        // dialog in codex-cli 0.120.0 - verified by reading codex's
        // source (`codex-rs/core/src/mcp_tool_call.rs`,
        // `custom_mcp_tool_approval_mode` resolves to
        // `mcp_servers.<name>.default_tools_approval_mode`; a value of
        // `Approve` makes `auto_approved_by_policy` true and skips the
        // prompt entirely). The global `approval_policy` flag (`never`,
        // `granular`, etc.) covers shell/patch approvals but NOT MCP
        // tool approvals - those are controlled per-server. The user's
        // 2026-04-17 directive ("MCP tools need to be pre-allowed for
        // codex, so it does not ask for permission for them") maps
        // onto this setting. Emitted for both the workbridge primary
        // and any user-configured extras - user-configured MCP servers
        // in a workbridge session are equally trusted (the user chose
        // them).
        cmd.push("--config".to_string());
        cmd.push(format!(
            "mcp_servers.{key}.default_tools_approval_mode=\"approve\""
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
        // C3: for write-capable interactive sessions, emit
        // `--dangerously-bypass-approvals-and-sandbox`. This flag
        // bypasses both the approval prompts AND the filesystem
        // sandbox - no sandbox; linked-worktree layout incompatible
        // with `workspace-write`. See README "Per-harness permission
        // model" for the full rationale (workbridge runs each work
        // item in a linked git worktree whose index lives outside the
        // worktree cwd, at `<repo>/.git/worktrees/<slug>/`, so
        // `workspace-write` denies `git commit` with "Operation not
        // permitted"). MCP tool approvals remain covered by per-server
        // `mcp_servers.<name>.default_tools_approval_mode = "approve"`
        // emitted by `extend_one_mcp_bridge` below (defence in depth
        // - the dangerous flag covers MCP approvals today but the
        // per-server overrides ensure the behaviour survives a Codex
        // change to that flag's scope).
        //
        // Read-only interactive sessions (global assistant in read-only
        // mode, hypothetical future read-only scope; no caller today)
        // omit the flag since the MCP-server layer enforces read-only
        // there.
        if !cfg.read_only {
            cmd.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        }
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
        // newline-delimited event stream. The read-only MCP server
        // enforces the gate semantics.
        //
        // No sandbox - see README "Per-harness permission model" for
        // why. The dangerous flag is included here explicitly even
        // though the review gate is conceptually read-only, for
        // symmetry across the three Codex spawn sites AND so review
        // skills that invoke shell commands (e.g. `cargo check`) are
        // not silently denied. `--dangerously-bypass-approvals-and-sandbox`
        // is a top-level flag and MUST precede `exec` per Codex's
        // clap layout (same constraint that forces the old
        // `--ask-for-approval never` to precede `exec` in the rebase
        // gate).
        //
        // C3b: MCP tool approvals are still suppressed per-server via
        // `mcp_servers.<name>.default_tools_approval_mode = "approve"`
        // (emitted by `extend_one_mcp_bridge`). Without that, the
        // headless review gate would block on the first `workbridge_*`
        // call (no user to answer the approval dialog).
        let mut cmd = vec![
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "exec".to_string(),
            "--json".to_string(),
            "--config".to_string(),
            format!("instructions={}", toml_quote_string(cfg.system_prompt)),
        ];
        Self::extend_mcp_bridge_argv(&mut cmd, Some(cfg.mcp_bridge), cfg.extra_bridges);
        cmd.push(cfg.initial_prompt.to_string());
        cmd
    }

    fn build_headless_rw_command(&self, cfg: &ReviewGateSpawnConfig<'_>) -> Vec<String> {
        // Headless read-write: no sandbox - see README "Per-harness
        // permission model" for the full rationale.
        // `--dangerously-bypass-approvals-and-sandbox` is a TOP-LEVEL
        // codex flag and MUST precede `exec`; clap rejects top-level
        // flags inside the `exec` subcommand.
        //
        // C3b: MCP tool approvals are also suppressed per-server via
        // `mcp_servers.<name>.default_tools_approval_mode = "approve"`
        // (emitted by `extend_one_mcp_bridge`) as defence in depth.
        // The headless rebase gate would otherwise block on the first
        // `workbridge_*` MCP call with no interactive user to answer.
        let mut cmd = vec![
            "--dangerously-bypass-approvals-and-sandbox".to_string(),
            "exec".to_string(),
            "--json".to_string(),
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
                    .map(std::string::ToString::to_string)
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
                approved: v
                    .get("approved")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
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

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codex_extras_tests.rs"]
mod extras_tests;
