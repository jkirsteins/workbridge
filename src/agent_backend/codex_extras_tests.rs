//! Second half of `CodexBackend`'s test suite. Split out of
//! `codex_tests.rs` to keep every file under the 700-line ceiling;
//! included via `#[path = "codex_extras_tests.rs"] mod extras_tests;`
//! in `codex.rs`. Covers per-repo extra-bridge emission, the
//! last-write-wins ordering invariant, TOML key quoting, the
//! `exec --json` event-stream parser, and the write-session-files
//! no-op contract.

use std::path::PathBuf;

use super::super::common::toml_quote_key;
use super::super::{
    AgentBackend, McpBridgeSpec, ReviewGateSpawnConfig, SpawnConfig, WORK_ITEM_ALLOWED_TOOLS,
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

/// Empty stream or no `agent_message` -> unapproved with a diagnostic
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
    let tmp = tempfile::tempdir().expect("tempdir");
    let cwd = tmp.path().to_path_buf();
    let files = CodexBackend.write_session_files(&cwd, "{}").unwrap();
    assert!(files.is_empty());
}
