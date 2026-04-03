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
- Status (Todo or InProgress)
- Repo associations (repo path + optional branch name)

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

If no PR exists for the branch, there is no PR piece.

## Work Item Status

Work items have two statuses:

- **Todo** - Work has been planned but not started.
- **InProgress** - Work is actively being done.

These are stored in the backend record and do not change automatically based
on PR state or session activity. Status transitions are user-initiated.

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

## What a Work Item Is NOT

- It is not a task tracker entry. There is no "assigned to," "due date,"
  or "priority" beyond what the linked issue provides.
- It is not shared between machines. Each machine has its own backend
  records and assembles its own view.
- It is not manually configured per-puzzle-piece. The branch-to-issue
  linkage and branch-to-PR lookup are automatic.
