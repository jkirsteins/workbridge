# Data Assembly

> STATUS: NOT IMPLEMENTED. This document describes the target design.

WorkBridge does not store metadata about work items. It assembles metadata
on the fly from local git state and GitHub API calls, caches it in memory,
and discards it when the process exits.

## The Derivation Chain

Starting from a worktree, WorkBridge derives everything else:

```
worktree exists on disk
  -> git branch --show-current
    -> branch name "42-resize-bug"
      -> regex extracts "42"
        -> GitHub API: get issue #42
          -> title, labels, state, assignees
      -> GitHub API: list PRs with head=42-resize-bug
        -> PR number, title, state, checks, reviewers
  -> git status
    -> dirty/clean, staged files
  -> git rev-list
    -> ahead/behind remote
```

Every piece of metadata is derived from something that already exists.
Nothing is entered by the user except the branch name (at worktree creation
time) and the one-time repo registration.

## Data Tiers

Data is grouped into tiers by how it is obtained:

### Tier 0: Local git (instant, always available)

- Worktree path and existence
- Branch name
- Last commit time
- Dirty/clean status
- Ahead/behind remote tracking branch

This data comes from local git operations that complete in milliseconds.
It is always available, even offline. The UI renders with this data
immediately on startup.

### Tier 1: Git remote (fast, usually available)

- Does the remote branch exist?
- Remote tracking status (up to date, behind, ahead, diverged)

This requires network access to the git remote but is a lightweight
operation (ls-remote or fetch --dry-run). Available unless the network
is down or the remote is unreachable.

### Tier 2: GitHub API (async, rate-limited)

- Pull requests for the branch
- PR status, checks, reviewers
- Issue details, labels, assignees
- Issue state (open/closed)

This requires GitHub API access with authentication. Responses take
200-500ms typically. Rate-limited to 5000 requests/hour with a token.

### Tier 3: Derived (pure computation)

- Work item composite status (synthesized from all tiers)
- Aggregation group keys
- Display labels and sort order

Computed locally from the assembled data. Recomputed whenever any
upstream tier updates.

## Refresh Strategy

### On Startup

1. Enumerate all worktrees across registered repos (Tier 0, sync)
2. Render the sidebar immediately with branch names and local status
3. Spawn async tasks per worktree for Tier 1 and Tier 2 data
4. As each task completes, update the work item and re-render

### Periodic Refresh

Tier 2 data is refreshed on a configurable interval (default: 120 seconds).
Each refresh cycle re-queries GitHub for all active work items. Tier 0 and
Tier 1 data is refreshed more frequently (every tick, ~1 second) since it
is cheap.

### Event-Driven Refresh

Certain user actions should trigger an immediate refresh of relevant data:

- User pushes from a session -> refresh PR status for that branch
- User switches to a work item -> refresh all tiers for that item
- User returns to the sidebar after being in a session -> refresh stale items

Event detection can use:

- File system watching on `.git/refs/` (branch pointer changed)
- Tmux output pattern matching (detecting push/pull commands)
- Focus change events (user switched panels)

### Offline Behavior

When GitHub API calls fail (network down, rate limited, auth expired),
WorkBridge continues operating with Tier 0 and Tier 1 data. GitHub-derived
pieces (issue, PR) show a stale indicator or are absent.

The UI should distinguish between "no PR exists" and "couldn't check for
PRs." A missing badge means no PR. A badge with a stale/error indicator
means the data couldn't be refreshed.

## The Override File

In rare cases, automatic derivation fails or is wrong. The branch name
might not encode an issue number, or the user wants to associate a branch
with a different issue than what the name suggests.

For these cases, a per-worktree override file can be placed at:

```
.git/worktrees/<worktree-name>/workbridge.json
```

For the main worktree (if tracked):

```
.git/workbridge.json
```

This file contains only the fields that override automatic derivation:

```json
{
  "issue": 42
}
```

This is the ONLY stored metadata in the system. It is optional, local to
the machine, never committed, and cleaned up automatically when the
worktree is removed.

The override file should be a last resort. If the branch name can encode
the issue number, it should.

## API Budget

With 5000 GitHub API requests per hour and a 120-second refresh cycle,
the budget per cycle is ~166 requests. Each work item costs approximately:

- 1 request: search PRs by head branch
- 1 request: get issue details (if linked)
- 1 request: get PR check runs (if PR exists)

So ~2-3 requests per work item per cycle. At 166 requests per cycle, the
system supports ~55-80 active work items before approaching rate limits.
This is well beyond typical usage.

For users with many repos and many open branches, WorkBridge should
prioritize refreshing work items that are currently visible or recently
active, and deprioritize items that are scrolled off-screen or idle.

## Open Questions

- Should WorkBridge cache Tier 2 data to disk so that restarts don't
  require a full re-fetch? This would make startup faster but adds a
  cache invalidation problem. Current stance: no, in-memory only.
  A full refresh on startup with 10 work items takes ~2 seconds, which
  is acceptable for a TUI.

- Should the derivation chain be extensible? For example, a plugin that
  derives data from Linear or Jira instead of GitHub Issues. This is
  architecturally clean (swap the Tier 2 data source) but adds complexity.
  Defer to v2.

- Should WorkBridge pre-fetch data for inbox items (remote PRs without
  local worktrees)? These are Tier 2 only (no local worktree to derive
  from). The cost is additional API calls for items the user might never
  adopt.
