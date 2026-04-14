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
- `default_branch(repo_path)` - Get the default branch name (checks
  symbolic-ref, falls back to local main/master)
- `github_remote(repo_path)` - Get the GitHub remote owner/repo pair
- `fetch_branch(repo_path, branch)` - Fetch a branch from origin
- `create_branch(repo_path, branch)` - Create a new local branch from
  the repo's default branch (or HEAD). Used as a fallback when
  `fetch_branch` fails (e.g., the branch does not exist on origin yet).

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

If a stale worktree already exists at the expected target location
(e.g. a prior delete cleanup failed non-blockingly and left the directory
behind), WorkBridge reuses it instead of calling `git worktree add`
again. Reuse is deliberately narrow: the match must satisfy all three
conditions:

1. `list_worktrees` returns an entry for the target branch.
2. The entry is NOT the main worktree (`is_main = false`).
3. The entry's canonicalized path equals the canonicalized target path
   under `.worktrees/<branch>`.

If the branch is checked out at any other location - the user's primary
repo checkout, a manually-created worktree, or a worktree for another
work item - reuse is refused and `git worktree add` is allowed to fail
with its native "already checked out" error. This prevents workbridge
from silently hijacking unrelated checkouts (which would violate
invariant #3 in `docs/invariants.md`).

Cancel/orphan cleanup paths (`poll_worktree_creation` and the delete
handler's phase 5) track whether a worktree was created by the
background thread or merely reused, and skip destructive
`remove_worktree` calls on reused paths - they never owned that
worktree in the first place.

### Auto-create on session spawn

When a session is spawned for a work item that has a branch but no worktree,
WorkBridge creates the worktree automatically before launching the Claude
Code session. If the branch no longer exists (cannot be fetched from origin
and cannot be created locally), a "Worktree Creation Failed" dialog is shown
offering to delete the orphaned work item or dismiss.

The same reuse rules described under "Auto-create on import" apply here:
a stale worktree at the expected target location is reused, but any
other existing checkout of the branch is left alone and surfaces as a
git error.

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

Deleting a work item performs comprehensive resource cleanup. There
are two entry points; both run the slow git/GitHub I/O (worktree
removal, branch deletion, `gh pr close`) on a background thread via
`spawn_delete_cleanup()` / `poll_delete_cleanup()` so the UI thread
is not blocked by those steps (see `docs/UI.md` "Blocking I/O
Prohibition"):

- **Manual delete (Ctrl+D/Delete)** - opens a `Delete Work Item`
  confirmation modal. The modal body warns that uncommitted changes
  will be lost and offers `[y]` to confirm or `[Esc]` to cancel. On
  'y', synchronous in-memory phases (backend delete, session kill,
  in-flight cancellation) run on the UI thread and the modal flips
  into an in-progress state with a spinner while the background
  thread performs resource cleanup. All input (keystrokes, paste
  events, mouse events) is swallowed while the spinner is visible so
  the user cannot interact with stale state. On completion the modal
  closes and a status message (success) or red alert dialog
  (warnings) is shown.
- **MCP delete (`workbridge_delete` tool)** - runs the non-blocking
  phases on the main thread and spawns the same background resource
  cleanup. Progress is surfaced via a status-bar activity spinner
  (`"Deleting work item resources..."`) rather than a modal because
  the user did not explicitly trigger the delete from a dialog.

Both paths perform the same cleanup steps, in this order per repo
association:

1. Closes the open GitHub PR via `gh pr close` FIRST. If the close
   fails (auth, network, merge queue state), local worktree and
   branch for that association are preserved so the user can recover
   unpushed commits and retry the close manually - the subsequent
   destructive local steps are skipped for that association and a
   warning is surfaced.
2. Removes worktree directories via `remove_worktree` (always with
   `--force`). Only runs if step 1 succeeded (or there was no open PR).
3. Deletes local git branches via `delete_branch` (force delete).
   Only runs if step 1 succeeded (or there was no open PR).
4. Kills active sessions and MCP servers (these run on the UI thread
   before the background cleanup spawn).
5. Cleans up in-memory state (rework reasons, review gate findings, etc.)

Both paths always pass `force=true` through to
`spawn_delete_cleanup()`. The modal body warns the user that
uncommitted changes will be lost, and the MCP path has no interactive
confirmation, so there is no scenario in which the non-force code
path would be safer. Critically, `open_delete_prompt()` does NOT
shell out to `git status --porcelain` before opening the modal -
doing so would be blocking I/O on the UI thread. The
`WorktreeService` trait no longer exposes a dirty-check method at
all, making this violation structurally impossible.

The worktree-creation race is handled entirely off the UI thread. If
a worktree-creation background thread has already completed (its
result is sitting in the `UserActionPayload::WorktreeCreate` receiver
stored under `UserActionKey::WorktreeCreate` in the user-action guard)
at the moment the user confirms the delete, `delete_work_item_by_id()`
Phase 5 drains the receiver and appends the orphan's `(repo_path,
worktree_path, branch)` to an `orphan_worktrees` vector passed in by
the caller. Each caller
(MCP delete and `confirm_delete_from_prompt`) then forwards every
orphan entry to `spawn_delete_cleanup()` as a synthesized
`DeleteCleanupInfo`, so `git worktree remove --force` and the follow-up
`git branch -D` run on the same background thread used for normal
delete cleanup. No synchronous `git worktree remove` or
`git branch -D` runs on the UI thread. See `docs/CLEANUP.md` for
the full async flow.

Both entry points guard against concurrent cleanup: if a previous
`spawn_delete_cleanup()` is still running (either path) when a new
delete is confirmed, the new delete is refused BEFORE the backend
record or session is touched, with an alert asking the user to wait.
This preserves the "only one `spawn_delete_cleanup()` in flight at a
time" invariant that prevents concurrent git worktree operations from
clobbering each other AND eliminates the orphaned-resource hole where
`spawn_delete_cleanup` would otherwise early-return after the backend
had already been destroyed.

All cleanup failures are non-blocking - warnings are shown but the delete proceeds.

### Cleanup ordering and main worktree handling

Cleanup steps must happen in the correct order:

1. **Close the remote PR first** (`gh pr close`), then run destructive
   local cleanup (worktree removal and branch deletion). Reversing this
   order creates a data-loss hazard: a `gh pr close` failure would
   leave the user with an open PR and no local branch or worktree to
   recover unpushed commits from. When PR close fails, both local
   destructive steps are skipped for that association and a warning
   is surfaced pointing the user at the preserved paths.

2. **Remove worktree before branch** (`git worktree remove`, then
   `git branch -D`). Reversing these two causes "branch used by
   worktree" errors.

3. **Main worktree detection**: `WorktreeInfo.is_main` indicates whether
   a worktree is the repo's primary checkout. When the branch to be
   deleted is checked out in the main worktree, both worktree removal
   and branch deletion are skipped - git forbids deleting the currently
   checked-out branch, and the main worktree cannot be removed via
   `git worktree remove`.

4. **Fresh vs. cached worktree state**: Different cleanup paths use
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

5. **Concurrent cleanup protection**: Only one `spawn_delete_cleanup()`
   thread can run at a time. If a second MCP delete is requested while
   a previous cleanup is still running, the resource cleanup phase is
   skipped and an alert dialog warns the user that worktrees, branches,
   and open PRs may need manual cleanup. The backend record and session
   are still deleted immediately.

6. **PR eviction tracking**: When the background cleanup thread closes a
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

## Multi-Machine Workflow

WorkBridge does not synchronize state between machines. It relies on git
remotes and GitHub as the shared state layer. Work started on Machine A
appears as an unlinked PR on Machine B, which can import it to continue
working.
