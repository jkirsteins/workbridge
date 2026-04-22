//! `OpenCode` CLI adapter - future-work stub.

use std::io;
use std::path::{Path, PathBuf};

use super::{
    AgentBackend, AgentBackendKind, ReviewGateSpawnConfig, ReviewGateVerdict, SpawnConfig,
};

/// Future-work stub adapter for the `OpenCode` CLI. Not implemented:
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
    use std::path::PathBuf;

    use super::super::{
        AgentBackend, AgentBackendKind, McpBridgeSpec, ReviewGateSpawnConfig, SpawnConfig,
        WORK_ITEM_ALLOWED_TOOLS,
    };
    use super::OpenCodeBackend;
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

        let tmp = tempfile::tempdir().expect("tempdir");
        let files = backend.write_session_files(tmp.path(), "{}").unwrap();
        assert!(files.is_empty());
    }
}
