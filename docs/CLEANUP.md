# Cleanup and Shutdown Behavior

## Normal quit (Q twice)

1. First Q press shows a confirmation prompt.
2. Second Q press sends SIGTERM to all child process groups.
3. The UI stays responsive while waiting for children to exit (up to 10 seconds).
4. If all sessions exit within the deadline, the app exits cleanly.
5. If the 10-second deadline is reached, all remaining sessions receive SIGKILL
   and the app exits immediately.

## Force quit (Q during wait)

- During the shutdown wait, pressing Q sends SIGKILL to all remaining sessions
  and exits immediately without waiting for graceful shutdown.
- The status bar shows the remaining seconds and the Q shortcut.

## External signals

- First SIGTERM or SIGINT: initiates the same graceful shutdown flow as
  keyboard quit (SIGTERM all children, wait up to 10s, then auto-SIGKILL).
- Second SIGTERM or SIGINT during the wait: sends SIGKILL to all remaining
  sessions and exits immediately (same as pressing Q during wait).

## Panic/crash (Drop path)

- If the app panics, each Session's Drop impl sends SIGKILL to its child
  process group immediately. There is no graceful shutdown in this path.
- The TerminalGuard restores the terminal (disable raw mode, leave alternate
  screen) before the panic message is printed.

## PTY close

- When the PTY master fd is closed (either explicitly or via Drop), the kernel
  sends SIGHUP to the child's process group. Most well-behaved programs treat
  SIGHUP as a termination signal.
- The reader thread detects the closed fd via EOF (read returns 0) and exits.

## Work item deletion

When a work item is deleted - manually (Ctrl+D), via the `workbridge_delete`
MCP tool, or automatically via auto-archive - the following resources are
cleaned up in order via `delete_work_item_by_id()`:

1. **Backend record** - `pre_delete_cleanup()` is called (no-op for
   LocalFileBackend; reserved for future backends), then the JSON file is
   deleted from disk.
2. **Sessions** - if a Claude Code PTY session is running, it receives SIGKILL
   and the session entry is removed from the sessions map. If a terminal PTY
   session is running (spawned via the Terminal tab), it also receives SIGKILL
   and is removed from the terminal sessions map.
3. **MCP server** - the MCP socket server and `.mcp.json` config file are
   removed via `cleanup_session_state_for()`.
4. **Open PR** - if a GitHub PR is open for this branch, it is closed
   FIRST via `gh pr close`. Merged or already-closed PRs are skipped
   (state != "OPEN"). If the close fails (auth error, network error,
   merge queue state, etc.), the local worktree and branch for that
   association are **preserved** so the user can recover unpushed
   commits and retry the PR close manually - the backend record is
   already gone at this point, so a loud "preserved local worktree X
   (PR close failed)" warning is the user's only breadcrumb.
5. **Worktree** - the git worktree directory is removed via
   `git worktree remove`. Worktree paths are looked up from the last
   fetched `repo_data` by matching branch name. Only runs if step 4
   succeeded (or the association had no open PR).
6. **Local branch** - the local git branch is deleted via `git branch -D`.
   Only runs if step 4 succeeded (or the association had no open PR).
   Best-effort: warnings are collected but do not abort the delete.
7. **In-flight operations** - any pending worktree creation, PR creation/merge,
   review submission, or mergequeue watch for this item is cancelled.
8. **In-memory state** - rework reasons, review gate findings, review reopen
   suppression, no-plan prompt queue, rework prompt state, merge prompt state,
   and review gate state are all cleared.

Steps 4-6 involve blocking I/O (git commands, gh CLI) and are NEVER run
from `delete_work_item_by_id` itself - that function now performs only
the non-blocking phases (backend delete, session kill, in-flight
cancellation, in-memory cleanup). Callers that need resource cleanup
first snapshot the per-association data via `gather_delete_cleanup_infos`
(pure cache lookups against `repo_data`) and then spawn a background
thread via `spawn_delete_cleanup()` / `poll_delete_cleanup()` so the UI
thread is never blocked. Auto-archive does not gather any cleanup infos
at all because Done items have already been through the merge flow,
which handles worktree/branch/PR removal.

The worktree-creation race is also handled off the UI thread: if a
worktree-creation background thread has already completed (its result
is queued in `worktree_create_rx`) at the moment the user confirms the
delete, `delete_work_item_by_id()` Phase 5 drains the receiver and
appends the orphan's `(repo_path, worktree_path, branch)` to an
`orphan_worktrees` vector passed in by the caller. Each orphan entry
is then forwarded to `spawn_delete_cleanup()` as a synthesized
`DeleteCleanupInfo` (with `branch_in_main_worktree: false` because a
freshly-created worktree is never the main worktree), so
`git worktree remove --force` and `git branch -D` both run on the same
background thread used for the normal delete cleanup path. There is
no remaining synchronous `git worktree remove` or `git branch -D` on
the UI thread.

The related "worktree created after work item was deleted" race - the
worktree-creation thread finishes while the item is already gone and
there is no active delete cleanup to join - is handled by
`spawn_orphan_worktree_cleanup()` (a dedicated fire-and-forget
background thread), not by `spawn_delete_cleanup()`. See
`docs/worktree-management.md` for details.

The Ctrl+D path invokes `confirm_delete_from_prompt()`, which calls
`delete_work_item_by_id()` (backend delete + session kill + in-flight
cancellation) and then spawns the background cleanup thread. The MCP
path (`McpEvent::DeleteWorkItem`) does the same ordering: delete the
backend record and kill sessions on the main thread, then spawn the
background cleanup. In both cases the `delete_cleanup_rx` receiver is
polled from `poll_delete_cleanup()` on each timer tick and the result
is surfaced via the modal (Ctrl+D) or the status-bar activity (MCP).

Steps 4-8 are best-effort: failures produce warning messages but do not
abort the overall delete. Only a backend delete failure (step 1) is
fatal. Note that a step 4 (`gh pr close`) failure does NOT abort the
delete either, but it does gate steps 5 and 6 (local destructive
cleanup) for that specific repo association, so the user is never left
with an open PR and no local branch to recover commits from.

Both manual delete (Ctrl+D) and MCP delete (`workbridge_delete`)
additionally reset UI selection state and rebuild the display list after
the non-blocking phases complete. Both paths always pass `force=true`
to the background thread: the modal body warns the user that
uncommitted changes will be lost, and the MCP path has no interactive
confirmation. The `workbridge_delete` tool is available for all
non-read-only sessions (both regular work items and review requests).

When the background delete-cleanup thread closes a PR, the
(repo_path, branch) pair is returned in `CleanupResult::closed_pr_branches`
and added to `cleanup_evicted_branches` so that stale fetch data does
not resurrect the closed PR as a phantom unlinked item. This mirrors the
eviction tracking used by the unlinked PR cleanup flow.

Only one delete cleanup can be in flight at a time. Both the modal
path (`confirm_delete_from_prompt`) and the MCP path
(`McpEvent::DeleteWorkItem`) check `delete_cleanup_rx.is_some()` BEFORE
touching the backend. If a previous cleanup is still running, the new
delete is refused with an alert and the backend record / session are
left intact - avoiding the orphaned-resource hole where
`spawn_delete_cleanup` would otherwise early-return after the backend
had already been destroyed.

### Delete confirmation modal

Manual delete (Ctrl+D or Delete key) opens a `Delete Work Item`
confirmation modal via `open_delete_prompt()`. The modal body warns
`"Delete '<title>' (uncommitted changes will be lost)?"` and offers
`[y]` to confirm or `[Esc]` to cancel. This wording is
unconditional - `open_delete_prompt` does NOT shell out to `git status
--porcelain` to pre-detect dirty worktrees, because blocking I/O on
the UI thread is forbidden (see `docs/UI.md` "Blocking I/O
Prohibition"). The cost is that users with clean worktrees see a
warning about uncommitted changes, but the benefit is that the UI
thread stays responsive and there is no dirty/clean split in the
confirm wording.

On `[y]`, `confirm_delete_from_prompt()` runs Phases 2-6 and spawns
the background cleanup. The modal remains visible with a braille
spinner (`"Removing worktree, branches, and open PRs..."`) and all
keystrokes are swallowed (including paste and mouse events) so the
PTY pane below cannot receive stray input while cleanup runs. Q and
Ctrl+Q still trigger the force-quit path as an escape hatch if the
background thread hangs. On completion, `poll_delete_cleanup()` closes
the modal and shows either a success status message or a red alert
with warnings.

### Auto-archive

Done work items are automatically deleted after `archive_after_days` (default:
7 days). The clock starts when the item enters Done state (either via user
action or derived from a merged PR). Items without a `done_at` timestamp are
never auto-archived. Setting `archive_after_days` to 0 disables auto-archive.

Auto-archive runs during `reassemble_work_items()`, after assembly and re-open
detection. This ordering ensures re-opened items have their `done_at` cleared
before the archive check, preventing incorrect deletion. Expired items are
deleted and a final reassembly updates the display state.
For Done items, steps 4-6 (worktree, branch, PR) are typically no-ops because
the merge flow already removes worktrees and branches, and merged PRs are not
in "OPEN" state.

## Unlinked PR cleanup

Unlinked PRs (open PRs whose branch is not claimed by any work item) can
be closed via Ctrl+D in the left panel. The cleanup flow runs entirely
on a background thread to avoid blocking the UI.

### User flow

1. Select an unlinked item and press Ctrl+D. A confirmation dialog
   appears with three options:
   - [Enter] Close with a reason (posts a comment on the PR first)
   - [d] Close directly (no comment)
   - [Esc] Cancel
2. After confirmation, the dialog transitions to a progress state: a
   braille spinner with "Closing PR #N... Please wait." All keys are
   swallowed during this phase.
3. On completion, the dialog closes. Warnings (if any) appear as a red
   alert dialog; success shows "Unlinked item closed" in the status bar.

### Background thread operations

The background thread (`spawn_unlinked_cleanup`) performs these steps:

1. Post optional reason comment via `gh pr comment`
2. Close the PR via `gh pr close`
3. Get a **fresh** worktree list via `list_worktrees()` (not cached
   `repo_data`, which may be stale if the user switched branches since
   the last fetch)
4. If the branch has a linked worktree: remove it, then delete branch
5. If the branch is the main worktree's current branch: skip both
   (git forbids deleting the currently checked-out branch)
6. If no worktree: just delete the branch

### Cache eviction

After the background thread completes, the closed PR must be removed
from the cached `repo_data.prs` to prevent it from reappearing as
unlinked. A simple eviction (removing the PR once) is insufficient
because an in-flight fetch (started before the close) can arrive later
and overwrite the cache with stale data that includes the now-closed PR.

To handle this, `cleanup_evicted_branches: Vec<(PathBuf, String)>`
tracks recently-closed (repo_path, branch) pairs. After every
`drain_fetch_results()` in the timer callback, `apply_cleanup_evictions()`
re-removes these branches from `repo_data.prs` and then clears the
vector. A single application is sufficient because the fresh fetch that
triggered `drain_fetch_results()` queries `--state open` and naturally
excludes the closed PR.

As a defensive measure, `collect_unlinked_prs()` in `assembly.rs` also
filters out PRs whose state is not "OPEN".
