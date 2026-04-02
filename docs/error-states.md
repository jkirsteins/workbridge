# Error States

> STATUS: NOT IMPLEMENTED. This document describes the target design.

When a work item violates an invariant (see [invariants.md](invariants.md)),
it enters an error state. This document describes how errors are detected,
presented, and resolved.

## Error Presentation

Errors are shown inline on the affected work item, not in a separate error
log or modal. The work item appears in the sidebar with a visual error
indicator, and selecting it shows the error details in the detail panel.

```
Sidebar:
  42-resize-bug          #42  PR#15 review
  refactor-backend       !    ERROR
  docs-tmux              #29  PR#12 draft

Detail panel (refactor-backend selected):
  refactor-backend [ERROR]
  
  Multiple open PRs detected on this branch:
    PR #14: refactor: extract SessionBackend trait (open)
    PR #19: refactor: backend trait v2 (open, draft)
  
  Close one of these PRs to resolve.
```

The error message must:
1. State what is wrong (the violated invariant)
2. Show the conflicting data (what was found)
3. Suggest how to fix it (what the user should do)

## Error States by Invariant

### Multiple Open PRs

**Detection**: GitHub API returns >1 open PR for the branch.

**Message**: "Multiple open PRs detected on branch X. PR #A: <title>. PR #B: <title>. Close one to resolve."

**Severity**: Error. The work item is still usable (the worktree works fine) but the PR piece is ambiguous and not shown.

### Issue Not Found

**Detection**: Branch name matches the issue pattern, extracting number N, but GitHub API returns 404 for issue #N.

**Message**: "Issue #N not found in <owner/repo>. The branch name suggests issue #N but it does not exist. Rename the branch or create the issue."

**Severity**: Warning. The work item is fully usable; it just has no issue piece. The warning nudges the user to fix the naming mismatch.

### Detached HEAD

**Detection**: `git branch --show-current` returns empty for the worktree.

**Message**: "Detached HEAD. Checkout a branch to track this work item."

**Severity**: Error. No branch means no PR lookup, no issue linkage. The worktree exists and the session works, but the work item is not trackable.

### Worktree on Default Branch

**Detection**: Worktree's branch matches the repo's default branch (main, master, or configured).

**Message**: "Worktree is on the default branch (main). Work items must be on feature branches."

**Severity**: Error. This worktree should not be a work item.

### Diverged Branch

**Detection**: Local and remote branch exist but point to different commits and neither is an ancestor of the other.

**Message**: "Branch has diverged from remote. N commits ahead, M commits behind. Pull or rebase to resolve."

**Severity**: Warning. The worktree is fully usable. The warning prevents the user from being surprised by a conflict later.

### Repository Unavailable

**Detection**: Registered repo path does not exist or is not accessible.

**Message**: "Repository path not found: /path/to/repo. Is the drive mounted?"

**Severity**: Info. All work items from this repo are hidden. The repo appears dimmed in any repo-level UI.

### GitHub API Unreachable

**Detection**: API calls fail (network error, auth expired, rate limited).

**Message**: Per work item: issue and PR badges show a stale indicator. Globally: status bar shows "GitHub: offline" or "GitHub: rate limited (resets in Xm)".

**Severity**: Info. Local data (Tier 0, Tier 1) is unaffected. The system continues working with degraded metadata.

## Philosophy

Every error state listed above was chosen because the alternative was
worse:

- Guessing which PR is correct risks showing wrong data.
- Silently dropping the issue link hides a naming mistake.
- Allowing detached HEAD worktrees creates items that can't be enriched.
- Auto-rebasing diverged branches can destroy work.

The cost of showing an error is low: the user reads a message and takes
action. The cost of guessing wrong is high: the user trusts incorrect
data and makes decisions based on it.

When in doubt, surface the problem. Never hide it.
