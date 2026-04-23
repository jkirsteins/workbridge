//! User-action guard subsystem.
//!
//! Single source of truth for "is this user-initiated remote-I/O action
//! currently in flight" plus the last-attempt timestamps used for debounce.
//! Every user-initiated remote-I/O spawn (`GithubRefresh`, `PrCreate`,
//! `PrMerge`, `ReviewSubmit`, `WorktreeCreate`, `UnlinkedCleanup`,
//! `DeleteCleanup`, `RebaseOnMain`) goes through this guard via the
//! `App::try_begin_user_action` / `App::end_user_action` helpers defined
//! in the parent module.
//!
//! See `docs/UI.md` "User action guard" for the admission contract and
//! `CLAUDE.md` severity overrides for the review-time policy.

use std::collections::HashMap;

use super::{
    ActivityId, CleanupResult, MergePreCheckMessage, PrCreateResult, PrMergeResult,
    ReviewSubmitResult, WorktreeCreateResult,
};
use crate::work_item::WorkItemId;

/// Keys identifying every user-initiated remote I/O action that runs
/// through `App::try_begin_user_action`. One variant per admission slot:
/// rejecting a second concurrent entry is the whole point of routing
/// through the helper.
///
/// See `docs/UI.md` "User action guard" for the admission contract and
/// `CLAUDE.md` severity overrides for the review-time policy.
#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub enum UserActionKey {
    /// User-triggered GitHub refresh (Ctrl+R). Debounced to 500ms; the
    /// background fetcher's structural restart path does NOT go through
    /// this key - only the explicit user press does.
    GithubRefresh,
    /// Asynchronous PR creation initiated from the Review stage.
    PrCreate,
    /// Asynchronous PR merge (`gh pr merge`) initiated from the merge
    /// modal.
    PrMerge,
    /// Asynchronous PR review submission (approve / request-changes).
    ReviewSubmit,
    /// Shared single slot for `spawn_session`'s auto-worktree creation
    /// and `spawn_import_worktree`'s import flow. Per-repo concurrency
    /// is intentionally out of scope here: both callers compete for the
    /// same global slot. If concurrent worktree creation for different
    /// repos is later wanted, key this on `(RepoPath, Branch)` instead.
    WorktreeCreate,
    /// Asynchronous unlinked-PR cleanup initiated from the cleanup
    /// modal. Single source of truth for the "cleanup in progress" read
    /// state that event.rs / ui.rs / salsa.rs query via
    /// `App::is_user_action_in_flight(&UnlinkedCleanup)`.
    UnlinkedCleanup,
    /// Asynchronous delete-cleanup initiated from the delete modal or
    /// the MCP delete handler.
    DeleteCleanup,
    /// Asynchronous rebase-onto-main initiated by the `m` keybinding.
    /// Single-flight: while a rebase is running for any work item, a
    /// second `m` press is silently coalesced. The matching payload
    /// carries the `WorkItemId` so the gate state can be looked up by
    /// owner. Per-item concurrency is intentionally out of scope: a
    /// future change wanting parallel rebases across different
    /// repos can re-key on `(RepoPath, Branch)` the same way the doc
    /// for `WorktreeCreate` describes.
    RebaseOnMain,
}

/// Payload stored inside `UserActionState` for each in-flight entry.
/// Carries the background-thread receiver and any per-action metadata
/// (such as the `WorkItemId` the action was spawned for) directly inside
/// the helper map, so state ownership is structural: dropping the map
/// entry drops the receiver, and there is no way to leave behind a
/// stray `Option<Receiver>` or orphaned `Option<ActivityId>`.
pub enum UserActionPayload {
    /// No receiver attached. Used by `GithubRefresh` (the fetcher has
    /// its own channel) and as the transient state between
    /// `try_begin_user_action` returning and the caller attaching the
    /// payload via `attach_user_action_payload`.
    Empty,
    PrCreate {
        rx: crossbeam_channel::Receiver<PrCreateResult>,
        wi_id: WorkItemId,
    },
    /// Precheck phase: the live `WorktreeService::list_worktrees`
    /// background thread is running and `poll_merge_precheck` will
    /// drain `rx` and either swap this payload for `PrMerge` (Ready)
    /// or release the slot entirely (Blocked / disconnected). The
    /// receiver is owned structurally by this variant so any
    /// `end_user_action(&UserActionKey::PrMerge)` automatically
    /// drops it - cancel paths (`retreat_stage`,
    /// `delete_work_item_by_id`) do not need any sibling cleanup
    /// line, because there is no sibling `Option<Receiver>` field
    /// on `App` that could outlive the slot.
    PrMergePrecheck {
        rx: crossbeam_channel::Receiver<MergePreCheckMessage>,
    },
    /// Merge phase: the `gh pr merge` background thread is running.
    /// Reached via `perform_merge_after_precheck` after a
    /// `PrMergePrecheck` -> `Ready` handoff, which replaces the
    /// payload via `attach_user_action_payload`. The slot was
    /// reserved in `execute_merge` before the precheck spawned, so
    /// this transition does NOT re-admit the helper key.
    PrMerge {
        rx: crossbeam_channel::Receiver<PrMergeResult>,
    },
    ReviewSubmit {
        rx: crossbeam_channel::Receiver<ReviewSubmitResult>,
        wi_id: WorkItemId,
    },
    WorktreeCreate {
        rx: crossbeam_channel::Receiver<WorktreeCreateResult>,
        wi_id: WorkItemId,
    },
    UnlinkedCleanup {
        rx: crossbeam_channel::Receiver<CleanupResult>,
    },
    DeleteCleanup {
        rx: crossbeam_channel::Receiver<CleanupResult>,
    },
    RebaseOnMain {
        wi_id: WorkItemId,
    },
}

/// State for one in-flight user action. Owned by the `UserActionGuard`
/// map keyed on `UserActionKey`. The `activity_id` is the status-bar
/// spinner started when the action was admitted; the `payload` carries
/// the per-action receiver so there is exactly one structural drop
/// site for both.
pub struct UserActionState {
    pub activity_id: ActivityId,
    pub payload: UserActionPayload,
}

/// Single source of truth for "is this user action in flight" plus the
/// last-attempt timestamps used for debounce. See `App::try_begin_user_action`.
#[derive(Default)]
pub struct UserActionGuard {
    pub in_flight: HashMap<UserActionKey, UserActionState>,
    pub last_attempted: HashMap<UserActionKey, std::time::Instant>,
}
