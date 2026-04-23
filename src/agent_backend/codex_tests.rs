//! Core argv-shape tests for `CodexBackend`. See `codex_extras_tests.rs` for the rest.
use std::path::PathBuf;

use super::super::{
    AgentBackend, AgentBackendKind, McpBridgeSpec, ReviewGateSpawnConfig, SpawnConfig,
    WORK_ITEM_ALLOWED_TOOLS,
};
use super::CodexBackend;
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

/// Assert that none of the legacy sandboxed-mode flags appear anywhere
/// in a Codex argv list. Used by both the interactive and the gate
/// (review / rebase) argv shape tests so the expectation stays in one
/// place.
fn assert_no_sandbox_flags(argv: &[String], context: &str) {
    assert!(
        !argv.iter().any(|s| s == "--sandbox"),
        "{context} must NOT emit --sandbox"
    );
    assert!(
        !argv.iter().any(|s| s == "workspace-write"),
        "{context} must NOT emit workspace-write"
    );
    assert!(
        !argv.iter().any(|s| s == "--ask-for-approval"),
        "{context} must NOT emit --ask-for-approval"
    );
    assert!(
        !argv.iter().any(|s| s == "--full-auto"),
        "{context} must NOT use --full-auto"
    );
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
    // Interactive write-capable Codex uses `--dangerously-bypass-approvals-and-sandbox`.
    // The linked-worktree layout is incompatible with Codex's built-in `workspace-write`
    // sandbox; see README "Per-harness permission model" for the full rationale.
    assert!(
        argv.iter()
            .any(|s| s == "--dangerously-bypass-approvals-and-sandbox"),
        "interactive Codex must emit --dangerously-bypass-approvals-and-sandbox, got {argv:?}"
    );
    assert_no_sandbox_flags(&argv, "interactive Codex");
    // Per-server approval pre-approve must be emitted for the
    // workbridge primary.
    assert!(
        argv.iter()
            .any(|s| s == "mcp_servers.workbridge.default_tools_approval_mode=\"approve\""),
        "workbridge MCP server must be marked default_tools_approval_mode=\"approve\" to suppress tool-call prompts, got {argv:?}"
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
    // Dangerous flag is top-level and must precede `exec`.
    assert_eq!(
        rg_argv.first().map(String::as_str),
        Some("--dangerously-bypass-approvals-and-sandbox"),
        "review gate must start with --dangerously-bypass-approvals-and-sandbox"
    );
    assert!(rg_argv.iter().any(|s| s == "exec"));
    assert!(rg_argv.iter().any(|s| s == "--json"));
    assert_no_sandbox_flags(&rg_argv, "review gate");

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
    // Rebase gate: `--dangerously-bypass-approvals-and-sandbox`
    // (top-level) must precede `exec`. Per-server MCP approval is
    // also emitted as defence in depth.
    assert_eq!(
        rw_argv.first().map(String::as_str),
        Some("--dangerously-bypass-approvals-and-sandbox"),
        "rebase gate must start with --dangerously-bypass-approvals-and-sandbox"
    );
    let exec_idx = rw_argv
        .iter()
        .position(|s| s == "exec")
        .expect("rebase gate must include `exec`");
    let dangerous_idx = rw_argv
        .iter()
        .position(|s| s == "--dangerously-bypass-approvals-and-sandbox")
        .expect("rebase gate must include dangerous flag");
    assert!(
        dangerous_idx < exec_idx,
        "--dangerously-bypass-approvals-and-sandbox must precede `exec`"
    );
    assert_no_sandbox_flags(&rw_argv, "rebase gate");
    assert!(
        rw_argv
            .iter()
            .any(|s| s == "mcp_servers.workbridge.default_tools_approval_mode=\"approve\""),
        "rebase gate must mark workbridge MCP server default_tools_approval_mode=\"approve\""
    );

    // C4: Codex writes no session files by default in the stub.
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let files = backend.write_session_files(&cwd, "{}").unwrap();
    assert!(files.is_empty());
}

/// Pins C3: interactive write-capable Codex uses
/// `--dangerously-bypass-approvals-and-sandbox`. The linked-worktree
/// layout is incompatible with Codex's built-in `workspace-write`
/// sandbox (git stores per-worktree state at
/// `<repo>/.git/worktrees/<slug>/` - outside the worktree cwd - and
/// `workspace-write` denies writes there, breaking `git commit`).
/// See README "Per-harness permission model" for the full rationale.
/// Per-server MCP approval overrides remain as defence in depth.
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
    // `--dangerously-bypass-approvals-and-sandbox` MUST be present.
    assert!(
        argv.iter()
            .any(|s| s == "--dangerously-bypass-approvals-and-sandbox"),
        "interactive Codex must emit --dangerously-bypass-approvals-and-sandbox, got {argv:?}"
    );
    // Per-server MCP pre-approval via
    // `mcp_servers.workbridge.default_tools_approval_mode="approve"`
    // is retained as defence in depth: the dangerous flag covers
    // MCP approvals today but the per-server overrides ensure the
    // behaviour survives a Codex change to that flag's scope.
    assert!(
        argv.iter()
            .any(|s| s == "mcp_servers.workbridge.default_tools_approval_mode=\"approve\""),
        "workbridge MCP server must be marked default_tools_approval_mode=\"approve\", got {argv:?}"
    );
    // Old sandboxed-mode flags MUST NOT be emitted.
    assert!(
        !argv.iter().any(|s| s == "--ask-for-approval"),
        "interactive Codex must NOT emit --ask-for-approval"
    );
    assert!(
        !argv.iter().any(|s| s == "--sandbox"),
        "interactive Codex must NOT emit --sandbox"
    );
    assert!(
        !argv.iter().any(|s| s == "workspace-write"),
        "interactive Codex must NOT emit workspace-write"
    );
    assert!(
        !argv.iter().any(|s| s == "--full-auto"),
        "interactive Codex must NOT use --full-auto"
    );
}

/// Pins C3b: all three Codex profiles MUST include the
/// `granular_approval.mcp_elicitations=false` `--config` override.
/// Without this, `--ask-for-approval never` alone does NOT suppress
/// per-MCP-tool approval dialogs in codex-cli 0.120.0 (verified
/// live on 2026-04-17). Suppression is done per-MCP-server via
/// `mcp_servers.<name>.default_tools_approval_mode = "approve"`.
/// The review gate and rebase gate are headless (`exec --json`)
/// and would hang forever on the first workbridge_* MCP call
/// without this setting; the interactive path surfaces the prompt
/// to the user, which breaks the "pre-allow MCP tools" user
/// requirement (2026-04-17 directive).
#[test]
fn codex_suppresses_mcp_tool_call_prompts_on_all_profiles() {
    let expected = "mcp_servers.workbridge.default_tools_approval_mode=\"approve\"".to_string();
    let has_approve_flag = |argv: &[String]| -> bool { argv.iter().any(|s| s == &expected) };
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
        has_approve_flag(&CodexBackend.build_command(&cfg)),
        "interactive write-capable Codex must mark workbridge MCP server default_tools_approval_mode=approve"
    );
    // Interactive read-only - also must suppress (read-only MCP
    // calls still need to go through without prompting).
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
        has_approve_flag(&CodexBackend.build_command(&ro_cfg)),
        "interactive read-only Codex must mark workbridge MCP server default_tools_approval_mode=approve"
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
        has_approve_flag(&CodexBackend.build_review_gate_command(&rg_cfg)),
        "review gate Codex must mark workbridge MCP server default_tools_approval_mode=approve"
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
        has_approve_flag(&CodexBackend.build_headless_rw_command(&rw_cfg)),
        "rebase gate Codex must mark workbridge MCP server default_tools_approval_mode=approve"
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

/// Pins C11: read-only interactive sessions omit every
/// permission/sandbox flag entirely. The read-only MCP server is
/// the enforcement mechanism; adding the dangerous bypass flag
/// here would grant write capability that the read-only path is
/// supposed to deny, and the legacy sandboxed flags
/// (`--ask-for-approval`, `--sandbox`, `--full-auto`) are also
/// omitted since read-only has no write semantics to gate.
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
    // Read-only must NOT emit the dangerous bypass flag.
    assert!(
        !argv
            .iter()
            .any(|s| s == "--dangerously-bypass-approvals-and-sandbox"),
        "read-only Codex must NOT emit --dangerously-bypass-approvals-and-sandbox; got {argv:?}"
    );
    // Read-only must NOT emit any legacy sandbox/approval flag either.
    assert!(!argv.iter().any(|s| s == "--ask-for-approval"));
    assert!(!argv.iter().any(|s| s == "--sandbox"));
    assert!(!argv.iter().any(|s| s == "workspace-write"));
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
    // Review gate: `--dangerously-bypass-approvals-and-sandbox` is
    // a top-level flag and MUST precede `exec` per Codex's clap
    // layout. Dangerous flag is included even though this path is
    // conceptually read-only, for symmetry across the three Codex
    // spawn sites and so review skills that shell out (e.g.
    // `cargo check`) are not silently denied.
    assert_eq!(
        argv[0], "--dangerously-bypass-approvals-and-sandbox",
        "review gate argv must start with the dangerous flag, got {argv:?}"
    );
    let dangerous_idx = argv
        .iter()
        .position(|s| s == "--dangerously-bypass-approvals-and-sandbox")
        .expect("review gate must include dangerous flag");
    let exec_idx = argv
        .iter()
        .position(|s| s == "exec")
        .expect("review gate must include `exec`");
    assert!(
        dangerous_idx < exec_idx,
        "--dangerously-bypass-approvals-and-sandbox must precede `exec`"
    );
    assert!(argv.iter().any(|s| s == "--json"));
    // Old sandboxed-mode flags MUST NOT be emitted on the review
    // gate path.
    assert!(!argv.iter().any(|s| s == "--ask-for-approval"));
    assert!(!argv.iter().any(|s| s == "--sandbox"));
    assert!(!argv.iter().any(|s| s == "workspace-write"));
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

/// Pins the headless rebase-gate shape: top-level
/// `--dangerously-bypass-approvals-and-sandbox` before `exec`, and
/// the per-server `default_tools_approval_mode="approve"` override
/// on the workbridge MCP server. The dangerous flag is a top-level
/// codex flag and MUST precede `exec` (clap rejects top-level
/// flags inside the `exec` subcommand). MCP tool prompts are also
/// suppressed via the per-server setting as defence in depth.
/// See README "Per-harness permission model" for why the rebase
/// gate does not use Codex's built-in sandbox.
#[test]
fn codex_headless_rw_argv_shape_and_mcp_pre_approval() {
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

    let exec_idx = argv
        .iter()
        .position(|s| s == "exec")
        .expect("exec subcommand must be present");

    // `--dangerously-bypass-approvals-and-sandbox` must precede exec.
    let dangerous_idx = argv
        .iter()
        .position(|s| s == "--dangerously-bypass-approvals-and-sandbox")
        .expect("--dangerously-bypass-approvals-and-sandbox must be present");
    assert!(
        dangerous_idx < exec_idx,
        "--dangerously-bypass-approvals-and-sandbox must come BEFORE `exec`; got {argv:?}"
    );

    // Old sandboxed-mode flags MUST NOT be emitted.
    assert!(
        !argv.iter().any(|s| s == "--ask-for-approval"),
        "rebase gate must NOT emit --ask-for-approval; got {argv:?}"
    );
    assert!(
        !argv.iter().any(|s| s == "--sandbox"),
        "rebase gate must NOT emit --sandbox; got {argv:?}"
    );
    assert!(
        !argv.iter().any(|s| s == "workspace-write"),
        "rebase gate must NOT emit workspace-write; got {argv:?}"
    );
    assert!(
        !argv.iter().any(|s| s == "--full-auto"),
        "rebase gate must NOT emit --full-auto; got {argv:?}"
    );

    // Per-server MCP pre-approval must be present so the headless
    // rebase gate does not hang on MCP tool prompts.
    assert!(
        argv.iter()
            .any(|s| s == "mcp_servers.workbridge.default_tools_approval_mode=\"approve\""),
        "rebase gate must mark workbridge MCP server default_tools_approval_mode=\"approve\", got {argv:?}"
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

/// Symmetry invariant: every Codex spawn path (interactive work-item
/// / global, headless review gate, headless rebase gate) MUST emit
/// `--dangerously-bypass-approvals-and-sandbox`. The review gate is
/// included explicitly even though it is conceptually read-only,
/// both for symmetry across the four spawn sites and so review
/// skills that invoke shell commands (e.g. `cargo check`) are not
/// silently denied. A future PR that re-introduces a sandbox flag
/// in just one builder - or drops the dangerous flag from just one
/// builder - must fail this test loudly. See README "Per-harness
/// permission model" for why the linked-worktree layout is
/// incompatible with Codex's built-in sandbox.
#[test]
fn codex_all_spawn_paths_use_dangerous_flag() {
    let mcp_path = PathBuf::from("/tmp/workbridge-mcp-fake.sock");
    let bridge = fake_bridge();
    let dangerous = "--dangerously-bypass-approvals-and-sandbox";

    // 1. Interactive work-item / global.
    let interactive_cfg = SpawnConfig {
        stage: WorkItemStatus::Implementing,
        system_prompt: Some("sys"),
        mcp_config_path: Some(&mcp_path),
        mcp_bridge: Some(&bridge),
        extra_bridges: &[],
        allowed_tools: WORK_ITEM_ALLOWED_TOOLS,
        auto_start_message: None,
        read_only: false,
    };
    let interactive_argv = CodexBackend.build_command(&interactive_cfg);
    assert!(
        interactive_argv.iter().any(|s| s == dangerous),
        "interactive Codex spawn path must emit {dangerous}; got {interactive_argv:?}"
    );

    // 2. Headless review gate (conceptually read-only - explicitly
    //    included for symmetry and for review skills that shell out).
    let rg_cfg = ReviewGateSpawnConfig {
        system_prompt: "sys",
        initial_prompt: "/claude-adversarial-review",
        json_schema: "{}",
        mcp_config_path: &mcp_path,
        mcp_bridge: &bridge,
        extra_bridges: &[],
    };
    let rg_argv = CodexBackend.build_review_gate_command(&rg_cfg);
    assert!(
        rg_argv.iter().any(|s| s == dangerous),
        "review gate Codex spawn path must emit {dangerous}; got {rg_argv:?}"
    );

    // 3. Headless rebase gate.
    let rw_cfg = ReviewGateSpawnConfig {
        system_prompt: "",
        initial_prompt: "rebase",
        json_schema: "{}",
        mcp_config_path: &mcp_path,
        mcp_bridge: &bridge,
        extra_bridges: &[],
    };
    let rw_argv = CodexBackend.build_headless_rw_command(&rw_cfg);
    assert!(
        rw_argv.iter().any(|s| s == dangerous),
        "rebase gate Codex spawn path must emit {dangerous}; got {rw_argv:?}"
    );

    // Symmetrically, none of the three paths may emit the legacy
    // sandboxed-mode flags (a future PR that re-introduces one in
    // just one spawn site would fail this check).
    for (label, argv) in [
        ("interactive", &interactive_argv),
        ("review gate", &rg_argv),
        ("rebase gate", &rw_argv),
    ] {
        for banned in [
            "--sandbox",
            "workspace-write",
            "--ask-for-approval",
            "--full-auto",
        ] {
            assert!(
                !argv.iter().any(|s| s == banned),
                "{label} Codex spawn path must NOT emit legacy flag {banned}; got {argv:?}"
            );
        }
    }
}
