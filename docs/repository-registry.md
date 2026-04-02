# Repository Registry

> STATUS: NOT IMPLEMENTED. This document describes the target design.

WorkBridge operates across multiple repositories. The user registers
repositories explicitly, and WorkBridge scans them for worktrees on startup.

## Why a Registry

Git worktrees cannot be discovered globally. `git worktree list` only works
inside a repository. There is no system-level index of "all git repos on
this machine."

WorkBridge needs to know which repositories to scan. Rather than walking the
entire filesystem (slow, noisy, privacy-invasive), the user registers repos
explicitly. This is a one-time cost per repo.

## Configuration File

```
~/.config/workbridge/config.json
```

The config file contains the list of registered repositories and per-repo
settings:

```json
{
  "repos": [
    {
      "path": "/Projects/workbridge",
      "worktree_dir": ".worktrees",
      "branch_issue_pattern": "^(\\d+)-"
    },
    {
      "path": "/Projects/backend-api",
      "worktree_dir": ".worktrees",
      "branch_issue_pattern": "^([A-Z]+-\\d+)-"
    }
  ],
  "defaults": {
    "worktree_dir": ".worktrees",
    "branch_issue_pattern": "^(\\d+)-",
    "refresh_interval_seconds": 120
  }
}
```

### Fields

**repos[].path** (required): Absolute path to the repository root (where
`.git/` lives for the main worktree).

**repos[].worktree_dir** (optional): Directory for managed worktrees,
relative to the repo root. Defaults to `.worktrees`. Can be absolute
for users who prefer worktrees outside the repo.

**repos[].branch_issue_pattern** (optional): Regex for extracting issue
identifiers from branch names. The first capture group is the issue
identifier. Defaults to `^(\d+)-` (leading number).

**defaults**: Fallback values for repos that don't specify overrides.

## CLI Registration

```
workbridge add .                    # register the repo at the current directory
workbridge add ~/Projects/backend   # register a specific path
workbridge add ~/Projects/*         # register all git repos found in the glob
workbridge remove .                 # unregister the current repo
workbridge repos                    # list registered repos and their status
```

`workbridge add` validates that the path contains a git repository before
registering. If the path is not a git repo, it rejects with an error.

`workbridge add` with a glob expands the glob, checks each result for a
`.git/` directory, and registers those that qualify. It reports what it
found and what it skipped.

## Startup Scan

On startup, WorkBridge processes each registered repo:

```
for each registered repo:
  1. Verify the path exists and is accessible
     - If not (e.g., external drive unmounted): mark repo as "unavailable"
     - Show in UI as dimmed, do not error/crash
  
  2. git -C <path> worktree list --porcelain
     - Discovers all worktrees for this repo
     - Each worktree (except main) becomes a work item candidate
  
  3. Determine GitHub remote
     - Parse origin remote URL to extract owner/repo
     - Needed for API calls in data assembly
  
  4. Assemble work items from discovered worktrees
     - Hand off to data assembly for async enrichment
```

## Unavailable Repos

A registered repo may become temporarily unavailable:

- External drive not mounted
- Network drive unreachable
- Directory deleted or moved

WorkBridge should not crash or silently drop these repos. They should appear
in the UI as unavailable, with the reason if determinable:

```
Work Items
  workbridge/42-resize-bug    #42  PR#15 review
  workbridge/refactor-backend      PR#14 approved
  backend-api (unavailable - path not found)
```

The user can re-register with the correct path, or mount the drive and
restart.

## GitHub Remote Detection

WorkBridge needs to map each repo to a GitHub owner/repo pair for API calls.
This is derived from the git remote URL:

```
git@github.com:owner/repo.git       -> owner/repo
https://github.com/owner/repo.git   -> owner/repo
https://github.com/owner/repo       -> owner/repo
```

If the remote URL doesn't match a GitHub pattern, GitHub features (issue
lookup, PR lookup) are disabled for that repo. The work items still function
with Tier 0 and Tier 1 data only.

If the repo has multiple remotes, WorkBridge uses `origin` by default. This
is configurable per-repo if needed.

## Open Questions

- Should WorkBridge auto-discover repos? For example, scan `~/Projects/`
  on first run and offer to register everything it finds. This is convenient
  but potentially noisy. Current stance: explicit registration only.

- Should the config file support repo groups or tags? For example, "work"
  vs "personal" repos with different settings. Defer unless the flat list
  proves insufficient.

- Should WorkBridge watch for new repos appearing in common directories?
  This is a "magic" behavior that might surprise users. Current stance: no.
