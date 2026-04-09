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

When a work item is deleted - either manually (Ctrl+D) or automatically via
auto-archive - the following resources are cleaned up in order via
`delete_work_item_by_id()`:

1. **Backend record** - `pre_delete_cleanup()` is called (no-op for
   LocalFileBackend; reserved for future backends), then the JSON file is
   deleted from disk.
2. **Session** - if a Claude Code PTY session is running, it receives SIGKILL
   and the session entry is removed from the sessions map.
3. **MCP server** - the MCP socket server and `.mcp.json` config file are
   removed via `cleanup_session_state_for()`.
4. **Worktree** - the git worktree directory is removed via
   `git worktree remove`. Worktree paths are looked up from the last
   fetched `repo_data` by matching branch name.
5. **Local branch** - the local git branch is deleted via `git branch -D`.
   Best-effort: warnings are collected but do not abort the delete.
6. **Open PR** - if a GitHub PR is open for this branch, it is closed via
   `gh pr close`. Merged or already-closed PRs are skipped (state != "OPEN").
7. **In-flight operations** - any pending worktree creation, PR creation/merge,
   review submission, or mergequeue watch for this item is cancelled.
8. **In-memory state** - rework reasons, review gate findings, review reopen
   suppression, no-plan prompt queue, rework prompt state, merge prompt state,
   and review gate state are all cleared.

Steps 4-6 involve blocking I/O (git commands, gh CLI) and are only
executed for user-initiated deletes (Ctrl+D). Auto-archive skips them
via `skip_resource_cleanup: true` to avoid blocking the UI thread during
timer-driven reassembly. This is safe because Done items have already
been through the merge flow which handles worktree/branch/PR cleanup.

Steps 4-8 are best-effort: failures produce warning messages but do not
abort the overall delete. Only a backend delete failure (step 1) is fatal.

The manual delete (Ctrl+D) additionally resets UI selection state and
rebuilds the display list after the shared cleanup completes.

### Force delete

When the selected work item's worktree has uncommitted changes, Ctrl+D shows
a confirmation prompt ("delete anyway?"). If confirmed, the worktree is
removed with `--force` and the branch with `-D`. Auto-archive never
force-removes dirty worktrees - it logs a warning instead.

### Auto-archive

Done work items are automatically deleted after `archive_after_days` (default:
7 days). The clock starts when the item enters Done state (either via user
action or derived from a merged PR). Items without a `done_at` timestamp are
never auto-archived. Setting `archive_after_days` to 0 disables auto-archive.

Auto-archive runs during `reassemble_work_items()`, before the assembly step.
Expired items are deleted and excluded from the record list passed to assembly.
For Done items, steps 4-6 (worktree, branch, PR) are typically no-ops because
the merge flow already removes worktrees and branches, and merged PRs are not
in "OPEN" state.
