# Error States

When a work item encounters an inconsistency, it is flagged with an error.
Errors are orthogonal to status - a Todo or InProgress item can have errors.

## Error Presentation

Errors are shown inline on the affected work item, not in a separate error
log or modal. The work item appears in the sidebar with a visual error
indicator, and selecting it shows the error details in the detail panel.

The error message must:
1. State what is wrong
2. Show the conflicting data (what was found)
3. Suggest how to fix it (what the user should do)

## Error Types

### MultiplePrsForBranch (implemented)

**Detection**: GitHub API returns >1 open PR for the branch (after filtering
out fork PRs from different repo owners).

**Presentation**: The work item shows an error badge. The detail panel lists
the conflicting PRs by number and title, and suggests closing one.

**Severity**: Error. The work item is still usable (the worktree works fine)
but the PR piece is ambiguous and not shown.

### IssueNotFound (implemented)

**Detection**: Branch name matches the issue pattern (e.g., `42-fix-bug`),
extracting issue number 42, but GitHub API returns 404 for that issue. Only
fires when the fetcher actually attempted the lookup - before the first
fetch completes, no error is shown.

**Presentation**: Warning badge with the issue number and repo. Suggests
renaming the branch or creating the issue.

**Severity**: Warning. The work item is fully usable; it just has no issue
piece.

### DetachedHead (defined, not currently produced)

**Detection**: Would fire when a worktree has no branch. Currently, detached
worktrees simply do not match any work item by branch, so no error is
produced - the worktree is silently excluded.

The variant exists in the WorkItemError enum for display completeness but
the assembly layer does not produce it.

### CorruptBackendRecord (defined, not currently produced)

**Detection**: Would fire when backend.list() encounters a parseable but
invalid record. In v1, the LocalFileBackend skips corrupt files entirely
rather than producing this error.

### BranchLockedToWorktree (implemented)

**Detection**: `git worktree add` fails with "is already used by worktree at"
error. This happens when a previous worktree was left in a corrupt state
(typically after an interrupted rebase via the "m" key) - the branch is still
registered as checked out in git's bookkeeping but the worktree has a detached
HEAD mid-rebase, so `find_reusable_worktree` does not match it.

**Presentation**: Modal dialog titled "Stale Worktree". Shows the branch name,
the stale worktree path, and the explanation. Offers [r] Force-remove stale
worktree & retry, or [Esc] Dismiss. During recovery, the dialog switches to a
spinner modal ("Recovering Worktree") with no key options.

**Recovery**: Force-removes the stale worktree entry (`git worktree remove
--force`), prunes git's bookkeeping (`git worktree prune`), then retries
`git worktree add`. All three operations run on a background thread. The
branch is preserved (not deleted) since it contains the user's work. On
success, the worktree is created and the session opens normally. On failure,
a generic alert is shown with the error details.

**Severity**: Error with recovery. The work item is unusable until the stale
worktree is cleaned up, but the recovery is automated once the user confirms.

### WorktreeGone (defined, not currently produced)

**Detection**: Would fire when a work item references a worktree path that
no longer exists on disk. Detection is deferred to a future assembly pass.
Currently, the worktree_path is simply set to None when no match is found.

## Planned Error Types

The following error conditions from the original design are not yet
implemented but remain planned:

- **Diverged Branch**: Local and remote branch point to different commits
  and neither is an ancestor of the other. Requires real git state
  derivation (currently hardcoded).
- **Repository Unavailable**: Registered repo path does not exist or is
  not accessible.
- **GitHub API Unreachable**: API calls fail (network error, auth expired,
  rate limited). Per-item stale indicators and a global status bar message.

## Philosophy

Every error state was chosen because the alternative was worse:

- Guessing which PR is correct risks showing wrong data.
- Silently dropping the issue link hides a naming mistake.
- Allowing detached HEAD worktrees creates items that cannot be enriched.
- Auto-rebasing diverged branches can destroy work.

The cost of showing an error is low: the user reads a message and takes
action. The cost of guessing wrong is high: the user trusts incorrect
data and makes decisions based on it.

When in doubt, surface the problem. Never hide it.
