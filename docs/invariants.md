# Invariants

Invariants are the non-negotiable rules of the system. They define what
WorkBridge considers valid. Code must enforce them. If reality violates
an invariant, the system surfaces an error rather than guessing.

These are requirements, not features. All implementation decisions should
be evaluated against these rules.

## The Rules

### 1. One worktree = one work item

A work item cannot exist without a worktree. A worktree always produces
exactly one work item. There is no work item that spans multiple worktrees
and no worktree that produces multiple work items.

If a worktree disappears (removed externally), the work item disappears
on the next scan. No tombstone, no history. The PR and issue on GitHub
are the permanent record.

### 2. A worktree must be on a named branch

Detached HEAD worktrees are not valid work items. The branch name is the
identity of the work -- it drives issue linkage, PR discovery, and display.
Without a branch name, none of the derivation chain works.

### 3. A worktree must not be on the default branch

The default branch (main, master, or configured) is the trunk. It is not
"work in progress." The main worktree of a repo is on the default branch,
but it is not a work item. Additional worktrees on the default branch are
an error.

### 4. One branch = at most one open PR

If multiple open PRs share the same head branch, WorkBridge cannot determine
which is current. This is an error state on the work item. The user must
close the stale PR.

### 5. Issue linkage is derived from the branch name

The branch name is the contract for issue linkage. A configurable regex
extracts zero or one issue identifier from the branch name. If the regex
matches, the issue is linked. If it doesn't match, there is no issue. There
is no manual override for this at the UI level -- the override file
(see [data-assembly.md](data-assembly.md)) exists for edge cases but is
not a primary workflow.

### 6. Derive, don't store

WorkBridge does not maintain a database of work item state. All metadata
is derived from git (worktrees, branches, remotes) and GitHub (PRs, issues)
on every scan. The only persistent state is:

- The list of registered repositories (config file)
- Optional per-worktree override files (rare, for edge cases)

If it can be derived, it must not be stored. Stored state drifts from
reality. Derived state is reality.

### 7. Inbox items are not work items

A remote PR without a local worktree appears in the inbox, not in the
work items list. Inbox items cannot have sessions, cannot be edited,
cannot be aggregated with work items. They become work items only when
adopted (which creates a local worktree).

### 8. One registered repo = one GitHub remote

Each registered repo maps to exactly one GitHub owner/repo for API calls,
derived from the `origin` remote URL. If the remote is not GitHub, GitHub
features are disabled for that repo. There is no support for multiple
GitHub remotes (e.g., fork + upstream) in v1.

## Why Strict Invariants

The alternative to strict invariants is heuristics: "if there are two open
PRs, pick the most recent one." Heuristics work most of the time, but when
they fail, they fail silently. The user sees wrong data and doesn't know it.

Strict invariants fail loudly. The user sees an error message with the
conflicting data and a suggested fix. This costs a few seconds of attention
but prevents decisions based on incorrect state.

The system can always be loosened later if a strict rule proves too
restrictive. Loosening a strict system is safe. Tightening a loose system
breaks existing workflows.

## Consequences

These invariants have direct consequences for the user:

- **Want issue linkage?** Name your branch with the issue number prefix.
  No config UI, no linking step, no "associate issue" dialog.

- **Want to track follow-up work on the same issue?** Create a new branch
  (e.g., `42-followup`). It becomes a new work item. The aggregation view
  groups them by issue.

- **Want to work on someone else's PR?** Adopt it from the inbox. This
  creates a worktree. Now it's your work item.

- **Merged and done?** Remove the worktree. The work item vanishes. GitHub
  has the history.

The system trades flexibility for predictability. Every work item behaves
the same way. Every branch follows the same rules. There are no special
cases.
