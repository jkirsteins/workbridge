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
- `remove_worktree(repo_path, worktree_path, delete_branch, force)` - Remove a
  worktree and optionally delete the branch. When `force` is true, uses
  `--force` for dirty worktrees and `-D` for unmerged branches.
- `delete_branch(repo_path, branch, force)` - Delete a local branch.
  Uses `-d` (safe) or `-D` (force) based on the `force` parameter.
- `is_worktree_dirty(worktree_path)` - Check if a worktree has uncommitted
  changes (staged or unstaged) via `git status --porcelain`.
- `default_branch(repo_path)` - Get the default branch name (checks
  symbolic-ref, falls back to local main/master)
- `github_remote(repo_path)` - Get the GitHub remote owner/repo pair
- `fetch_branch(repo_path, branch)` - Fetch a branch from origin

`GitWorktreeService` implements this trait by shelling out to the git CLI.
A `StubWorktreeService` exists for tests.

### Auto-create on import

When an unlinked PR is imported, WorkBridge:

1. Creates a backend record linking the work item to the repo and branch
   (synchronous - returns immediately).
2. Spawns a background thread (`spawn_import_worktree`) that performs the
   git operations asynchronously, following the Blocking I/O Prohibition
   pattern described in `docs/UI.md`:
   a. Fetches the branch from origin (`fetch_branch`).
   b. Creates a worktree for the branch (`create_worktree`).
   c. Sends the result (success or error) through a bounded channel.
3. The main event loop picks up results via `poll_worktree_creation()` on
   each timer tick. On success it reassembles the work item list so the
   new worktree path is visible. On failure (fetch or create error) it
   displays a status message and the work item remains without a worktree.

Only one worktree creation can be in flight at a time. If a second import
is triggered while one is already running, the backend record is still
created but the worktree creation is queued with a status message.

### Auto-create on session spawn

When a session is spawned for a work item that has a branch but no worktree,
WorkBridge creates the worktree automatically before launching the Claude
Code session. If the branch no longer exists (cannot be fetched from origin
and cannot be created locally), a "Worktree Creation Failed" dialog is shown
offering to delete the orphaned work item or dismiss.

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

### Worktree removal on delete

Deleting a work item performs comprehensive resource cleanup. There are
two entry points:

- **Manual delete (Ctrl+D/Delete)** - runs resource cleanup (worktree
  removal, branch deletion, PR closure) synchronously on the UI thread
  via `delete_work_item_by_id()`.
- **MCP delete (`workbridge_delete` tool)** - runs the non-blocking
  phases (backend delete, session kill, in-memory cleanup) on the main
  thread, then spawns resource cleanup on a background thread via
  `spawn_delete_cleanup()` / `poll_delete_cleanup()` to avoid blocking
  the UI.

Both paths perform the same cleanup steps:

1. Removes worktree directories via `remove_worktree`
2. Deletes local git branches via `delete_branch` (force delete)
3. Closes open PRs on GitHub via `gh pr close`
4. Kills active sessions and MCP servers
5. Cleans up in-memory state (rework reasons, review gate findings, etc.)

The manual delete path uses a 3-step confirmation flow to protect
against accidental deletion:
- First press: "Press again to delete this work item"
- If dirty worktree detected: "Worktree has uncommitted changes! Press again to force-delete"
- Final press: deletes with `--force` for dirty worktrees

The MCP delete path always uses force mode (no interactive confirmation
is possible via MCP).

All cleanup failures are non-blocking - warnings are shown but the delete proceeds.

### Cleanup ordering and main worktree handling

Worktree removal and branch deletion must happen in the correct order:

1. **Remove worktree first** (`git worktree remove`), then delete branch
   (`git branch -D`). Reversing this order causes "branch used by
   worktree" errors.

2. **Main worktree detection**: `WorktreeInfo.is_main` indicates whether
   a worktree is the repo's primary checkout. When the branch to be
   deleted is checked out in the main worktree, both worktree removal
   and branch deletion are skipped - git forbids deleting the currently
   checked-out branch, and the main worktree cannot be removed via
   `git worktree remove`.

3. **Fresh vs. cached worktree state**: Different cleanup paths use
   different data strategies:
   - `spawn_unlinked_cleanup()` calls `list_worktrees()` directly in
     the background thread rather than using the cached `repo_data`
     worktree list. This avoids acting on stale data when the user has
     switched branches since the last fetch cycle.
   - `spawn_delete_cleanup()` uses cached data gathered on the main
     thread before spawning (via `gather_delete_cleanup_infos()`). This
     is acceptable because the work item's worktree and branch are
     known at delete time and unlikely to change between gathering and
     cleanup execution.

4. **Concurrent cleanup protection**: Only one `spawn_delete_cleanup()`
   thread can run at a time. If a second MCP delete is requested while
   a previous cleanup is still running, the resource cleanup phase is
   skipped and an alert dialog warns the user that worktrees, branches,
   and open PRs may need manual cleanup. The backend record and session
   are still deleted immediately.

5. **PR eviction tracking**: When the background cleanup thread closes a
   PR, the (repo_path, branch) pair is added to
   `cleanup_evicted_branches` so that stale fetch data does not
   resurrect the closed PR as a phantom unlinked item. This mirrors the
   eviction tracking used by the unlinked PR cleanup flow.

### Post-merge cleanup

After a PR is merged (Review -> Done transition), WorkBridge cleans up:
- Removes the worktree directory (`remove_worktree` with `delete_branch=true`)
- Deletes the local branch (`-d` safe delete, appropriate for merged branches)
- If no worktree exists but a branch does, the branch is still cleaned up

## What Is Planned

The following features are defined in the API but lack full UI flows:

### Divergence handling

When local and remote branches point to different commits, WorkBridge
should create the worktree from the local branch and flag the work item
with a "diverged" warning. This requires real git state derivation (dirty,
ahead/behind), which is not yet implemented.

### Branch state detection on creation

The full matrix of branch states (fresh, remote-only, local-only, already
checked out, diverged) is not yet handled in the UI. Currently
`create_worktree` handles the common cases (fresh branch, existing local
branch) and `fetch_branch` + `create_worktree` handles the remote-only
case during import.

`create_branch` refuses to create a new branch when the repo has
uncommitted changes (staged, unstaged, or untracked files). This
prevents branching from an ambiguous state where the working tree does
not match the committed base.

## Multi-Machine Workflow

WorkBridge does not synchronize state between machines. It relies on git
remotes and GitHub as the shared state layer. Work started on Machine A
appears as an unlinked PR on Machine B, which can import it to continue
working.
