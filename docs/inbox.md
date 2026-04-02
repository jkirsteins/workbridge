# Inbox

> STATUS: NOT IMPLEMENTED. This document describes the target design.

The inbox contains remote work that has no local worktree. These are NOT
work items -- they are candidates that can become work items when the user
adopts them.

## What Appears in the Inbox

The inbox is populated by querying GitHub for the user's open PRs across
all registered repositories:

```
for each registered repo:
  gh pr list --author @me --json headRefName,number,title,state,...
  
  for each PR:
    if no local worktree exists for this branch:
      -> add to inbox
    else:
      -> already a work item, skip
```

An inbox item is a PR that exists on GitHub but has no corresponding local
worktree. Common reasons:

- The work was started on another machine
- A collaborator opened a PR and requested review
- The local worktree was removed but the PR is still open

## Inbox vs. Work Items

Inbox items are fundamentally different from work items:

| | Work Item | Inbox Item |
|---|---|---|
| Local worktree | yes (mandatory) | no |
| Claude Code session | yes | no |
| Editable | yes | no (read-only) |
| Tier 0 data (git) | yes | no |
| Tier 2 data (GitHub) | yes | yes |
| Can be adopted | n/a | yes |

An inbox item becomes a work item when the user adopts it, which creates
a local worktree for the PR's branch (see
[worktree-management.md](worktree-management.md) for the adoption flow).

## Inbox Scope

The inbox only shows PRs from registered repositories. If the user has
open PRs on repos not registered with WorkBridge, those PRs do not appear.

This is intentional. WorkBridge cannot create worktrees for repos it
doesn't know about. Showing PRs the user can't act on is noise.

## Filtering

Not all of the user's PRs are relevant to the inbox. PRs that already
have a local worktree are excluded. Beyond that, filtering options:

- By review state (draft, review requested, approved, changes requested)
- By repo

The inbox is a secondary UI element. It should not compete with work items
for attention. It is a queue of things that might need action, not an
active workspace.

## Open Questions

- Should the inbox include PRs where the user is a reviewer, not the
  author? These are "someone else's work that needs my attention." They
  can't be adopted in the same way (the user probably doesn't want to
  check out someone else's branch). They might warrant a separate section
  like "Review Requests."

- Should the inbox include GitHub Issues assigned to the user that have
  no branch yet? These are "work I should start." They're not PRs, so
  the adoption flow is different (create a new branch, not fetch an
  existing one). This could be useful but expands the inbox concept
  significantly.
