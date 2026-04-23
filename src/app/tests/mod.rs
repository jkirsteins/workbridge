//! Unit tests for `src/app/mod.rs`.
//!
//! Extracted from the previously-inline `mod tests { ... }` block so
//! each individual test file stays at or below the 700-line ceiling
//! enforced by `hooks/budget-check.sh`. Shared imports and helpers
//! live in this file; each submodule pulls in the specific names it
//! needs via explicit `use super::{...}` lines.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{
    ActivityId, App, DeleteCleanupInfo, DisplayEntry, FirstRunGlobalHarnessModal, GroupHeaderKind,
    MergePreCheckMessage, MergeReadiness, OrphanWorktree, PrMergeOutcome, PrMergePollResult,
    PrMergePollState, PrMergeResult, PrMergeWatch, RebaseGateMessage, RebaseGateState,
    RebaseTarget, ReviewGateMessage, ReviewGateOrigin, ReviewGateResult, ReviewGateSpawn,
    ReviewGateState, SessionOpenPending, SessionOpenPlanResult, SettingsListFocus,
    StaleWorktreePrompt, StubBackend, StubWorktreeService, UserActionKey, UserActionPayload,
    WorktreeCreateResult,
};
use crate::agent_backend::AgentBackendKind;
use crate::config::{Config, RepoEntry, RepoSource};
use crate::github_client::GithubError;
use crate::mcp::McpEvent;
use crate::work_item::{
    BackendType, FetchMessage, RepoFetchResult, SessionEntry, UnlinkedPr, WorkItem, WorkItemId,
    WorkItemKind, WorkItemStatus,
};
use crate::work_item_backend::{
    ActivityEntry, BackendError, CreateWorkItem, PrIdentityRecord, RepoAssociationRecord,
    WorkItemBackend,
};
use crate::worktree_service::WorktreeService;

/// Poll-wait for a path to be removed from disk, bounded by
/// `timeout`. Used in tests that drive teardown paths whose file
/// removal runs on a detached background thread via
/// `App::spawn_agent_file_cleanup` (blocking I/O on the UI thread
/// is forbidden by `docs/UI.md` "Blocking I/O Prohibition", so
/// the removal cannot run synchronously inside the helper the
/// test calls). The thread spins up exactly one `remove_file`
/// call, so in practice the file disappears within a few
/// milliseconds; the 5-second default timeout is for stressed
/// CI hosts, not for correctness.
fn wait_until_file_removed(path: &std::path::Path, timeout: std::time::Duration) {
    let start = crate::side_effects::clock::instant_now();
    while path.exists() {
        if crate::side_effects::clock::elapsed_since(start) >= timeout {
            return;
        }
        crate::side_effects::clock::sleep(std::time::Duration::from_millis(10));
    }
}

mod shared;

mod part_01;
mod part_02;
mod part_03;
mod part_04;
mod part_05;
mod part_06;
mod part_07;
mod part_08;
mod part_09;
mod part_10;
mod part_11;
mod part_12;
mod part_13;
mod part_14;
mod part_15;
mod part_16;
mod part_17;
mod part_18;
mod part_19;
mod part_20;
mod part_21;
mod part_22;
mod part_23;
mod part_24;

pub use part_05::*;
pub use part_06::*;
pub use part_08::*;
pub use part_12::*;
pub use part_14::*;
pub use part_15::*;
pub use part_16::*;
pub use part_17::*;
pub use part_22::*;
