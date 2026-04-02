# Worktree Management

> STATUS: NOT IMPLEMENTED. This document describes the target design.

WorkBridge owns the creation, placement, and removal of git worktrees.
This is the mechanical layer that turns "I want to work on this" into a
directory on disk with a branch checked out.

## Placement Convention

All worktrees created by WorkBridge live under a `.worktrees/` directory
at the root of the repository:

```
/Projects/workbridge/               <- main worktree (the repo root)
/Projects/workbridge/.worktrees/    <- managed worktrees live here
  42-resize-bug/
  refactor-backend/
  docs-tmux/
```

The `.worktrees/` directory is added to `.gitignore` by WorkBridge on
first use. This keeps worktree directories out of version control.

The placement directory is configurable per-repo for users who prefer a
different layout (e.g., sibling directories or a bare-repo workflow).
See [repository-registry.md](repository-registry.md) for configuration.

## Branch Naming

WorkBridge does not enforce a branch naming scheme, but it uses a convention
to derive issue linkage:

- Branch names starting with a number are parsed as `<issue-number>-<slug>`.
  Example: `42-resize-bug` links to issue #42.
- Branch names without a leading number have no automatic issue linkage.
  Example: `refactor-backend` has no linked issue.

This convention is the contract for issue linkage. It is simple, visible in
every git log, and requires no external metadata.

The pattern for extracting issue numbers is configurable per-repo for teams
that use different conventions (e.g., `JIRA-123-description` or
`feature/123/description`). The default pattern is `^(\d+)-`.

## Worktree Creation

When the user creates a new work item, WorkBridge determines the branch
state and acts accordingly:

### Branch States

| State | Local branch | Remote branch | Worktree | Action |
|-------|-------------|---------------|----------|--------|
| Fresh | no | no | no | `git worktree add <path> -b <branch>` |
| Remote only | no | yes | no | `git fetch` then `git worktree add <path> --track origin/<branch>` |
| Local only | yes | no | no | `git worktree add <path> <branch>` |
| Already checked out | yes | - | yes | Error: "Branch is already checked out at <path>" |
| Diverged | yes | yes (different) | no | `git worktree add <path> <branch>` + warning badge |

The "Fresh" state is the normal case: the user is starting new work.
The "Remote only" state is the adoption case: picking up work from another
machine or from the inbox.

### Diverged Branches

When local and remote branches point to different commits, WorkBridge creates
the worktree from the local branch and flags the work item with a "diverged"
warning. It does NOT auto-rebase or auto-merge. The user resolves the
divergence inside the Claude Code session.

This is consistent with the principle of surfacing problems rather than
guessing at solutions.

## Worktree Removal

Work items end when their worktree is removed. Removal can happen:

- **Manually**: user presses Ctrl+D in the TUI, or runs `workbridge rm <branch>`
- **After merge**: WorkBridge detects the PR was merged and offers to clean up

WorkBridge should confirm before removal, since the worktree may contain
uncommitted or unpushed work. The confirmation message should state what
would be lost:

```
Remove worktree 42-resize-bug?
  Branch: 42-resize-bug
  Uncommitted changes: 3 files modified
  Unpushed commits: 0
  PR #15: merged

  Press Ctrl+D again to confirm.
```

If the branch has been merged and there are no uncommitted changes, removal
is low-risk and the confirmation can be lighter.

After removal, the worktree directory is deleted and the branch is optionally
pruned (deleted locally and from the remote). Branch pruning should be a
separate explicit action, not automatic -- the user may want to keep the
branch for reference.

## Multi-Machine Workflow

WorkBridge does not synchronize state between machines. Instead, it relies
on git remotes and GitHub as the shared state layer.

### Scenario: Continuing Work on Another Machine

```
Machine A:
  1. Creates worktree for 42-resize-bug
  2. Works, commits, pushes
  3. Opens PR #15
  4. Stops working (closes laptop, etc.)

Machine B:
  1. Has the same repo registered
  2. On startup, scans GitHub -> finds PR #15 on branch 42-resize-bug
  3. PR appears in the Inbox (no local worktree)
  4. User adopts it -> WorkBridge fetches branch, creates worktree
  5. Work continues. Pushes go to the same remote branch.

Machine A (later):
  1. On startup, finds existing worktree for 42-resize-bug
  2. Worktree is behind remote (Machine B pushed new commits)
  3. Work item shows "behind 3" indicator
  4. User pulls inside the session
```

No sync protocol needed. Git push/pull is the protocol.

### Stale Worktrees

If a worktree exists locally but the remote branch has been deleted (e.g.,
after merge + branch cleanup on GitHub), WorkBridge flags the work item:

```
42-resize-bug [remote deleted]
  PR #15: merged
  Local branch still exists with 0 unpushed commits.
  Ctrl+D to clean up.
```

This is an informational nudge, not an error. The worktree is still usable.

## Open Questions

- Should WorkBridge ever create worktrees outside the configured directory?
  For example, if the user clones a repo and starts working in the main
  worktree, should that count as a work item? Current stance: no, the main
  worktree is special and not tracked as a work item.

- Should WorkBridge support converting an existing checkout (not a worktree)
  into a managed worktree? This would require moving files, which is risky.
  Current stance: no, create a fresh worktree instead.

- What about submodules? A worktree in a repo with submodules needs
  `git submodule update --init` after creation. Should WorkBridge handle
  this automatically? Probably yes, but it adds failure modes.
