# Worktree Management

WorkBridge manages the creation, listing, and removal of git worktrees
through the WorktreeService trait and its GitWorktreeService implementation.

## What Is Implemented

### WorktreeService trait

The `WorktreeService` trait (in `worktree_service.rs`) defines the API for
worktree operations:

- `list_worktrees(repo_path)` - List all worktrees for a repo (parses
  `git worktree list --porcelain`)
- `create_worktree(repo_path, branch, target_dir)` - Create a new worktree
  for a branch at a given directory
- `remove_worktree(repo_path, worktree_path, delete_branch)` - Remove a
  worktree and optionally delete the branch
- `default_branch(repo_path)` - Get the default branch name (checks
  symbolic-ref, falls back to local main/master)
- `github_remote(repo_path)` - Get the GitHub remote owner/repo pair
- `fetch_branch(repo_path, branch)` - Fetch a branch from origin

`GitWorktreeService` implements this trait by shelling out to the git CLI.
A `StubWorktreeService` exists for tests.

### Auto-create on import

When an unlinked PR is imported, WorkBridge automatically:
1. Fetches the branch from origin (`fetch_branch`)
2. Creates a worktree for the branch (`create_worktree`)
3. Creates a backend record linking the work item to the repo and branch

### Auto-create on session spawn

When a session is spawned for a work item that has a branch but no worktree,
WorkBridge creates the worktree automatically before launching the Claude
Code session.

## Placement Convention

Worktrees created by WorkBridge live under a `.worktrees/` directory
at the root of the repository:

```
/Projects/workbridge/               <- main worktree (the repo root)
/Projects/workbridge/.worktrees/    <- managed worktrees live here
  42-resize-bug/
  refactor-backend/
```

The `.worktrees/` directory is added to `.gitignore` by WorkBridge on
first use. The placement directory is configurable per-repo.

## Branch Naming

WorkBridge does not enforce a branch naming scheme, but it uses a convention
to derive issue linkage:

- Branch names starting with a number are parsed as `<issue-number>-<slug>`.
  Example: `42-resize-bug` links to issue #42.
- Branch names without a leading number have no automatic issue linkage.

The issue number extraction pattern is configurable per-repo (default:
`^(\d+)-`).

## What Is Planned

The following features are defined in the API but lack full UI flows:

### Worktree removal UI

The `remove_worktree` method exists and is tested, but the TUI does not
yet expose a delete-worktree action (e.g., Ctrl+D). Deleting a work item
currently removes the backend record but does not remove the worktree
from disk.

### Divergence handling

When local and remote branches point to different commits, WorkBridge
should create the worktree from the local branch and flag the work item
with a "diverged" warning. This requires real git state derivation (dirty,
ahead/behind), which is not yet implemented.

### Post-merge cleanup

After a PR is merged, WorkBridge should detect it and offer to clean up
the worktree and optionally prune the branch. This is not yet implemented.

### Branch state detection on creation

The full matrix of branch states (fresh, remote-only, local-only, already
checked out, diverged) is not yet handled in the UI. Currently
`create_worktree` handles the common cases (fresh branch, existing local
branch) and `fetch_branch` + `create_worktree` handles the remote-only
case during import.

## Multi-Machine Workflow

WorkBridge does not synchronize state between machines. It relies on git
remotes and GitHub as the shared state layer. Work started on Machine A
appears as an unlinked PR on Machine B, which can import it to continue
working.
