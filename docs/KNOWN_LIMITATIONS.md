# Known Limitations

## Blocking PTY writes

**What:** `write_bytes()` performs a blocking `libc::write()` on the UI thread.
The master fd is in blocking mode, so write blocks until the kernel PTY buffer
has space for the data.

**When it is a problem:** Large paste operations (>64KB) while the child process
is not reading stdin. The kernel PTY buffer is typically 4-64KB depending on the
OS. If the buffer is full because the child is busy and not consuming input,
the write call blocks the entire UI thread.

**Why it is accepted:** Single keystrokes are 1-4 bytes, well within what the
kernel buffer can absorb instantly. Claude Code reads stdin promptly during
normal operation. The realistic failure case - pasting megabytes of text while
Claude is mid-computation and not reading stdin - is a narrow scenario for our
use case.

**Impact:** The UI freezes until the child process reads from stdin and frees
buffer space. No data is lost - the write completes once the child catches up.

**Future fix options:**
- `poll()` before write with a timeout to detect a full buffer, then queue
  the data and retry on the next tick.
- Dedicated writer thread with a channel: the UI thread sends bytes to the
  channel (non-blocking), and the writer thread does the blocking write
  off the UI thread.

**Workaround:** If the UI freezes after a large paste, wait for the child
process to catch up. The freeze resolves on its own once the child reads
the buffered input.

## Worktree reuse via symlink-resolved path equality

**What:** `App::find_reusable_worktree` (src/app.rs) decides whether an
existing git worktree can be reused in place of `git worktree add` by
comparing the canonicalized `.worktrees/<branch>` target against the
canonicalized path git reports. `config::canonicalize_path` resolves
symlinks, so if the `.worktrees/<branch>` leaf is itself a symlink that
happens to point at another registered worktree on the same branch,
equality holds and the reuse path is taken.

**When it is a problem:** Only when `.worktrees/` or `.worktrees/<branch>`
has been manually replaced with a symlink to another git-registered
worktree on the matching branch. Workbridge never creates such symlinks,
and `git worktree add` always creates real directories. The bypass
requires a user or external script to deliberately set up the symlink.

**Why it is accepted:** This is a "you did this to yourself" setup on a
local dev tool. Normal checkout paths that happen to live under a
symlinked parent (e.g. `/Users/foo/Projects -> /Volumes/SSD/Projects`)
are NOT affected: both sides of the comparison resolve through the same
symlink to the same physical location, which is the correct outcome.
The guard only fails open when a leaf component is an intentional
misdirection, which no realistic workflow produces.

**Impact:** A work item could be bound to a worktree workbridge did
not intend to adopt. Session spawn, MCP state, and delete-time cleanup
would then operate on that alternate worktree. Because reused paths
are flagged with `reused: true`, the destructive orphan-cleanup paths
still skip `remove_worktree`, so there is no force-delete escalation.

**Future fix options:**
- Require a purely lexical (non-symlink-resolving) equality between
  git's reported worktree path and `wt_target`, falling back to
  canonicalization only for OS-level display quirks like macOS
  `/tmp` vs `/private/tmp`.
- Reject any match where a component of `wt_target` is a symlink.

**Workaround:** Do not replace `.worktrees/<branch>` with a symlink.

## Single-threaded event loop

**What:** All event handling (keyboard input, terminal resize, liveness checks)
runs on a single thread - the main thread.

**When it is a problem:** Many tabs with heavy output can slow tick processing,
since each tick iteration renders the UI and checks liveness for all tabs.

**Why it is accepted:** Reader threads handle output draining off the UI thread.
Each tab has a dedicated reader thread that continuously reads PTY output and
feeds it to the vt100 parser. The UI thread only locks parsers briefly to call
`.screen()` for rendering. For typical Claude Code usage (1-5 tabs), this is
not a bottleneck. The render tick fires at ~120fps (8ms) so PTY output renders
smoothly, and heavy background work (liveness checks, fetch drains) is throttled
to roughly every 25th tick (~200ms) to keep CPU usage reasonable.

## Mergequeue watch can bind to the wrong PR after an app restart

**What:** When a work item is in Mergequeue and the TUI is closed, the
in-memory `MergequeueWatch.pr_number` pin is lost. On next launch,
`reconstruct_mergequeue_watches` rebuilds the watch with `pr_number = None`
and the first poll falls back to `gh pr view <branch>`. `gh` resolves a
branch name to "the most recent PR for that branch", not the specific PR
the user pressed `[p] Poll` on. After the first successful poll, the
watch is pinned to whatever PR `gh` returned, and subsequent polls (and
any eventual auto-transition to Done via `save_pr_identity`) reference
that PR.

**When it is a problem:** A branch has more than one PR over its lifetime.
Concretely: user opens PR A on branch `foo`, enters Mergequeue, quits the
TUI. PR A is closed (manually or by `gh pr close`). Someone opens PR B on
branch `foo`. User relaunches the TUI. `reconstruct_mergequeue_watches`
rebuilds the watch; the first poll resolves `foo` to PR B. If PR B then
merges, the work item auto-advances to Done with PR B's number, title,
and URL persisted into `pr_identity`. The work item is stamped with a PR
it was never associated with.

**Why it is accepted:** The live-entry path (pressing `[p] Poll` while
the TUI is running) already pins `pr_number` from `assoc.pr.number`
immediately and is never vulnerable. The remaining window is narrow:
TUI closed AND original PR closed AND new PR opened on same branch AND
first poll of reconstructed watch - all before the user retreats the
item or re-enters Mergequeue against the new PR. In practice the user
who entered Mergequeue usually owns the branch and would notice the
swap. Pinning across restarts would require persisting `pr_number` on
the backend record, which adds migration complexity for existing
Mergequeue tickets without a stored number.

**Impact:** The Done item carries `pr_identity` for the wrong PR
(number, title, URL). Any audit log or detail pane keyed on
`pr_identity` shows the replacement PR. The backend record's other
fields are untouched.

**Workaround:** If you suspect the branch has had a different PR opened
on it while the TUI was closed, retreat the work item back to Review
with Shift+Left before the first poll cycle completes (inside the first
30 seconds after relaunch), then re-enter Mergequeue against the
intended PR.

**Future fix options:**
- Persist `pr_number` (and a short-lived `pr_identity`) on the backend
  record at `enter_mergequeue` time, and reconstruct watches directly
  from the persisted number so the branch-fallback path is never used.
- Reject `MERGED` results from the first poll of a reconstructed watch
  unless the returned `pr_identity.number` matches some fingerprint the
  user confirmed.
- Surface a warning in the detail pane when the first poll of a
  reconstructed watch resolves to a PR number that differs from anything
  the work item has referenced before.

