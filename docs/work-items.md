# Work Items

A work item is WorkBridge's central abstraction. It represents one unit of
in-progress software development work.

## Definition

A work item is anchored by a persistent backend record (a local JSON file
in v1) and enriched with derived metadata from git and GitHub. It combines:

- **A backend record** (mandatory, provides identity, title, status, repo associations)
- **A local git worktree** (optional, matched by branch name)
- **A GitHub issue** (optional, derived from branch name)
- **A GitHub pull request** (optional, derived from branch-to-PR lookup)

These components are called "puzzle pieces." The backend record is always
present. The worktree, issue, and PR are discovered automatically and
attached when found.

## Puzzle Pieces

### Backend Record (mandatory)

The backend record anchors the work item's identity. It is a local JSON file
(in v1) that stores:

- Work item ID (file path)
- Title
- Status (Backlog, Planning, Implementing, Blocked, or Review)
- Repo associations (repo path + optional branch name + optional PR identity snapshot)

The UI can render a work item list immediately from backend records alone,
before any git or GitHub data is fetched.

### Worktree (optional)

Matched by branch name from the backend record's repo associations. When a
worktree exists, WorkBridge has:

- A path on disk (where the code lives)
- A branch name (the identity of the work)
- A location to spawn a Claude Code session

Note: git dirty/clean status and ahead/behind counts are defined in the
data model (GitState) but currently hardcoded to false/0/0. Real git state
derivation is planned but not yet implemented.

### GitHub Issue (optional)

Linked when the branch name matches the issue pattern (default: starts with
a number). For example, `42-resize-bug` links to issue #42. The issue gives
WorkBridge:

- Issue title, state (open/closed)
- Labels

If the branch name has no matching number, there is no issue. This is fine -
the work item simply has no issue piece.

### Pull Request (optional)

Discovered by querying GitHub for open PRs whose head branch matches the
work item's branch. The PR gives WorkBridge:

- PR number, title, state
- Draft status
- Review decision (approved, changes requested, etc.)
- CI check status
- URL

If no live PR exists for the branch but the backend record is Done and has
a persisted PR identity snapshot (saved at merge time), assembly synthesizes
a PrInfo with PrState::Merged from the snapshot. This ensures Done items
continue displaying their PR link after the branch/PR is cleaned up.

For Done items that were merged before `pr_identity` persistence existed,
a one-time startup backfill queries GitHub for merged PRs and populates
the snapshot. This migration code (in `salsa.rs` / `app.rs`) can be
removed once no Done items with `pr_identity=None` remain on disk.

If no live PR exists and no persisted PR identity applies, there is no PR
piece.

## Work Item Kind

Each work item has a `WorkItemKind` that determines its workflow:

- **Own** (default) - The user's own work. Follows the full six-stage
  workflow: Backlog -> Planning -> Implementing -> Blocked -> Review -> Done.
  Created when importing an unlinked PR or creating a new work item from
  scratch.

- **ReviewRequest** - A PR where the user was requested as a reviewer.
  Restricted to a two-stage workflow: Review -> Done. These items appear
  in the "REVIEW REQUESTS" group in the sidebar with an "[RR]" badge.

### Review request behavior

Review-requested PRs are discovered via the `review-requested:@me` GitHub
search filter and displayed in a dedicated "REVIEW REQUESTS" section in the
sidebar. Before import, they show an "R" prefix.

Pressing Enter on a review request imports it directly into the Review stage
(not Backlog), since reviewing is the only meaningful action. A worktree is
created for the reviewer to inspect the code.

Stage restrictions for ReviewRequest items:

- **advance_stage**: Only Review -> Done is allowed (via the merge gate).
  Advancing from any other stage is blocked.
- **retreat_stage**: Always blocked. There is no valid previous stage for
  a review request in Review.
- **MCP transitions**: All MCP status transitions are blocked. Claude
  sessions should not drive workflow for someone else's PR.

## Work Item Status

Work items follow a six-stage workflow:

- **Backlog** - Work has been identified but not started. (Stored as "Backlog" in the backend; legacy "Todo" values are accepted via serde alias.)
- **Planning** - A Claude session produces an implementation plan. Advancing to Implementing requires the plan to be set via `workbridge_set_plan`; manual advance is blocked.
- **Implementing** - Active development. A Claude session works on the code.
- **Blocked** - The implementation is stuck and needs user input. Can move back to Implementing when unblocked.
- **Review** - Implementation is complete and under review. Entering Review from Implementing or Blocked triggers a review gate (async plan-vs-implementation check) and auto-creates a PR.
- **Done** - Work is finished. This status is derived, not directly settable (see below).

### Status transitions

Most forward transitions are triggered by the user via TUI keybinds (advance/retreat). Claude sessions can request a limited set of transitions via the `workbridge_set_status` MCP tool:

- Implementing -> Review (routed through the review gate)
- Implementing -> Blocked
- Blocked -> Implementing
- Blocked -> Review (routed through the review gate)
- Planning -> Implementing

All other transitions must go through TUI keybinds.

### Review gate

When a work item transitions from Implementing or Blocked to Review (whether user- or MCP-initiated), a review gate runs asynchronously. It compares the implementation plan against the actual code changes and produces findings. If no plan exists, the gate is skipped.

### Merge gate

Advancing from Review to Done is gated by PR merge. Instead of directly changing status, the user is prompted to choose a merge strategy (squash or merge). The TUI spawns an async `gh pr merge` command. Done is reached only after GitHub confirms the PR was merged.

If any prerequisite is missing - no repo association, no branch, no GitHub remote, or no open PR - the merge is blocked with an error message and the item stays in Review. Done cannot be set directly via MCP either; it always requires the merge gate.

### Derived Done status

During assembly, if any repo association has a merged PR (`PrState::Merged`), the work item's status is set to Done regardless of what the backend record says. This is marked as a derived status (`status_derived = true`). When the status is derived, manual stage transitions (advance/retreat) and MCP transitions are blocked.

This includes synthetic PrInfo produced by the PR identity fallback: when a backend record is Done and has a persisted PR identity snapshot but no live PR, assembly injects a PrInfo with PrState::Merged. The derived-Done logic then fires on this synthetic PR, setting `status_derived = true`. Because the fallback only activates when the backend record is already Done, non-Done items are never forced into derived-Done by a stale snapshot.

## Sessions

Each work item may have an associated Claude Code session running inside
its worktree. The session is the interactive element - it is where the user
does the actual work.

Session states:

- **Alive**: Claude Code process is running
- **Dead**: The process has exited. The worktree still exists.

A dead session does not destroy the work item. The worktree persists, and
the session can be respawned. Only deleting the backend record destroys the
work item.

## Work Item Identity

A work item is identified by its backend record ID (a file path in v1).
Backend records define what work items exist. Derived data (worktrees, PRs,
issues) is assembled on top.

This means:

- One work item can span multiple repos (via multiple repo associations).
- One issue can be referenced by multiple work items (different branches,
  different worktrees). A future aggregation view could group them.
- One branch can never have multiple worktrees on the same machine,
  because git prohibits two worktrees on the same branch.

## Deleting Work Items

Deleting a work item (Ctrl+D/Delete in the TUI) performs comprehensive cleanup
of all associated resources. The cleanup is best-effort: failures produce
warnings but do not prevent the delete.

### Resources cleaned up

- Backend record (the JSON file)
- Activity log (the .jsonl file)
- Worktree directory on disk
- Local git branch (force-deleted with `-D`)
- Open PR on GitHub (closed via `gh pr close`)
- Active Claude session (killed)
- MCP socket server and .mcp.json config file
- In-memory state: rework reasons, review gate findings, no-plan prompt queue,
  merge/rework prompt visibility flags

### 3-step confirmation flow

1. First press: "Press again to delete this work item"
2. Second press checks worktree status:
   - If all worktrees are clean (or no worktrees exist): deletes immediately
   - If any worktree has uncommitted changes: "Worktree has uncommitted changes!
     Press again to force-delete"
3. Third press (only if dirty): force-deletes using `git worktree remove --force`

Any key other than the delete key cancels the confirmation flow.

### Backend-specific cleanup

The `WorkItemBackend` trait provides a `pre_delete_cleanup()` hook called before
the record is deleted. The default implementation is a no-op. Future backends
(GithubIssueBackend, GithubProjectBackend) can override this to close backing
issues or archive project items.

### In-flight operation handling

If worktree creation is in progress for the deleted item, the result is drained
and any orphaned worktree is cleaned up. If PR creation is in progress, it is
cancelled. Pending PR creation queue entries for the deleted item are removed.

## What a Work Item Is NOT

- It is not a task tracker entry. There is no "assigned to," "due date,"
  or "priority" beyond what the linked issue provides.
- It is not shared between machines. Each machine has its own backend
  records and assembles its own view.
- It is not manually configured per-puzzle-piece. The branch-to-issue
  linkage and branch-to-PR lookup are automatic.
