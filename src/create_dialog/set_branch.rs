//! The "Set branch name" recovery modal state.

use rat_widget::text_input::TextInputState;

use crate::work_item::{WorkItemId, WorkItemStatus};

/// Which follow-up action should be re-driven after the user confirms a
/// `SetBranchDialog`. The dialog itself only persists the branch name;
/// the caller who triggered it recorded its intent here so
/// `confirm_set_branch_dialog` can complete the original gesture.
#[derive(Clone, Debug)]
pub enum PendingBranchAction {
    /// The user pressed Enter on a branchless Planning/Implementing item,
    /// which should open a Claude session once the branch is set.
    SpawnSession,
    /// The user tried to advance a Backlog item past Planning without a
    /// branch; re-attempt the stage change once the branch is persisted.
    Advance {
        from: WorkItemStatus,
        to: WorkItemStatus,
    },
}

/// State for the "Set branch name" recovery modal. Shown when a work item
/// has reached a stage where a branch is required but its repo
/// associations all have `branch.is_none()`. The dialog reuses
/// `rat_widget::text_input::TextInputState` and prefills a slug generated
/// from the item's title.
#[derive(Clone, Debug)]
pub struct SetBranchDialog {
    /// The work item that needs a branch.
    pub wi_id: WorkItemId,
    /// The branch-name text input, prefilled with a slug default.
    pub input: TextInputState,
    /// What to do after the branch is persisted.
    pub pending: PendingBranchAction,
}
