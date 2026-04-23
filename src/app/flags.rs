//! Grouping structs for the many boolean flags that used to live
//! directly on `App`. Each substate groups together bools that are
//! conceptually part of the same flow, so `struct_excessive_bools`
//! sees <= 3 bools per struct.
//!
//! The grouping is by concern (delete flow, merge flow, cleanup
//! flow, etc.), not by alphabetical accident. Each subset struct
//! also stays well under the clippy threshold of "more than 3 bools
//! per struct" so the refactor doesn't just push the complaint
//! down one level.

/// Boolean flags for the delete-work-item flow.
#[derive(Default)]
pub struct DeleteFlowFlags {
    /// True when the delete confirmation modal is visible.
    pub prompt_visible: bool,
    /// True while the async delete cleanup thread is running on behalf
    /// of the user-initiated (modal) delete path. The dialog stays
    /// visible with a spinner and the event loop swallows all keys
    /// except Q/Ctrl+Q.
    pub in_progress: bool,
}

/// Boolean flags for the merge-work-item flow.
#[derive(Default)]
pub struct MergeFlowFlags {
    /// True when the merge strategy prompt is visible (Review -> Done).
    pub confirm: bool,
    /// True while the merge background thread is running.
    /// The dialog stays open with a spinner in this state.
    pub in_progress: bool,
}

/// Boolean flags for the unlinked-PR cleanup flow.
#[derive(Default)]
pub struct CleanupFlowFlags {
    /// True when the unlinked-item cleanup confirmation prompt is visible.
    pub prompt_visible: bool,
    /// True when the cleanup reason text input is active (user pressed
    /// Enter from the confirmation prompt to type an optional close
    /// reason).
    pub reason_input_active: bool,
}

/// Boolean flags for various prompt/recovery dialogs that each only
/// need a single bool of visibility state.
#[derive(Default)]
pub struct PromptFlags {
    /// True when the rework reason text input is visible (Review ->
    /// Implementing).
    pub rework_visible: bool,
    /// True when the no-plan prompt is visible (offered when the agent
    /// blocks because no implementation plan exists).
    pub no_plan_visible: bool,
    /// True while the background recovery thread is running
    /// (force-remove, prune, recreate) for a stale worktree. The
    /// dialog switches to a spinner with no key options so the user
    /// cannot interact until recovery completes.
    pub stale_recovery_in_progress: bool,
}

/// GitHub CLI availability / one-shot status tracking.
#[derive(Default)]
pub struct GhStatusFlags {
    /// True once a "gh CLI not found" message has been shown. Prevents
    /// spamming the status bar on every fetch cycle.
    pub cli_not_found_shown: bool,
    /// True once a "gh auth required" message has been shown. Prevents
    /// spamming the status bar on every fetch cycle.
    pub auth_required_shown: bool,
    /// True if the `gh` CLI is available at startup.
    pub available: bool,
}

/// Background fetcher bookkeeping flags.
#[derive(Default)]
pub struct FetcherFlags {
    /// Set when manage/unmanage changes active repos. The main loop
    /// checks this flag and restarts the background fetcher with the
    /// updated repo list so newly managed repos get fetched and removed
    /// repos stop.
    pub repos_changed: bool,
    /// True when the fetcher channel has disconnected unexpectedly
    /// (all sender threads exited). Surfaced in the status bar so the
    /// user knows background updates have stopped.
    pub disconnected: bool,
}
