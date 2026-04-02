# Work Items

> STATUS: NOT IMPLEMENTED. This document describes the target design.

A work item is WorkBridge's central abstraction. It represents one unit of
in-progress software development work.

## Definition

A work item is the combination of:

- **A local git worktree** (mandatory, always present)
- **A GitHub issue** (optional, derived from branch name)
- **A GitHub pull request** (optional, derived from branch-to-PR lookup)

These three components are called "puzzle pieces." The worktree is always
present. The issue and PR are discovered automatically and attached when found.

A work item is NOT a record in a database. It is a live, assembled view of
the current state of a branch, computed from git and GitHub every time
WorkBridge runs.

## Puzzle Pieces

### Worktree (mandatory)

The worktree is the anchor. Without it, there is no work item. The worktree
gives WorkBridge:

- A path on disk (where the code lives)
- A branch name (the identity of the work)
- Dirty/clean status
- Ahead/behind remote counts
- A location to spawn a Claude Code session

### GitHub Issue (optional)

Linked when the branch name begins with a number: `42-resize-bug` links to
issue #42. The issue gives WorkBridge:

- Issue title, state (open/closed)
- Labels
- Assignees

If the branch name has no leading number, there is no issue. This is fine --
the work item simply has no issue piece.

### Pull Request (optional)

Discovered by querying GitHub for open PRs whose head branch matches the
worktree's branch. The PR gives WorkBridge:

- PR number, title, state
- Review status (draft, review requested, approved, changes requested)
- CI check status
- Reviewers

If no PR exists for the branch, there is no PR piece.

## Work Item Lifecycle

```
1. CREATED
   User creates a worktree (Ctrl+N in TUI, or workbridge new <branch>).
   Work item appears in the sidebar with just the worktree piece.
   Branch name may link an issue immediately.

2. ACTIVE
   User works in the Claude Code session.
   Worktree shows as alive, dirty/clean, ahead/behind.
   User may open a PR -- PR piece appears automatically on next scan.

3. IN REVIEW
   PR is open. Checks are running. Reviewers are assigned.
   Work item shows all three pieces (if issue was linked).

4. COMPLETED
   PR is merged. The work is done.
   Worktree can be removed (Ctrl+D or workbridge clean).
   Work item disappears. The PR and issue are the permanent record.

5. FOLLOW-UP
   If the same issue needs more work, the user creates a NEW worktree
   with a new branch (e.g., 42-followup). This is a new work item that
   happens to link to the same issue. Aggregation views can group them.
```

## Sessions

Each work item may have an associated Claude Code session running inside
its worktree. The session is the interactive element -- it's where the user
does the actual work.

Session states:

- **Alive**: Claude Code is running and responsive
- **Idle**: Claude Code is running but no recent interaction
- **Dead**: The process has exited. The worktree still exists.

A dead session does not destroy the work item. The worktree persists, and
the session can be respawned. Only removing the worktree destroys the work
item.

The distinction between alive and idle is a convenience for the UI -- it
helps the user identify which workstreams have recent activity. The threshold
for "idle" is a display concern, not a system state.

## Work Item Identity

A work item is identified by its worktree path. Two worktrees at different
paths are always different work items, even if they share the same issue
number.

This means:

- Machine A and Machine B can both have work items for the same branch.
  They are independent work items that happen to share a remote branch.
- One issue can have multiple work items (different branches, different
  worktrees). The aggregation view groups them.
- One branch can never have multiple work items on the same machine,
  because git prohibits two worktrees on the same branch.

## What a Work Item Is NOT

- It is not a task tracker entry. There is no "assigned to," "due date,"
  or "priority" beyond what the linked issue provides.
- It is not persistent across worktree deletion. When the worktree goes,
  the work item goes.
- It is not shared between machines. Each machine assembles its own view.
- It is not manually configured (with one exception: the branch-to-issue
  override described in [data-assembly.md](data-assembly.md)).
