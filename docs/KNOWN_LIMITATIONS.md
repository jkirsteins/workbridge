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

## Single-threaded event loop

**What:** All event handling (keyboard input, terminal resize, liveness checks)
runs on a single thread - the main thread.

**When it is a problem:** Many tabs with heavy output can slow tick processing,
since each tick iteration renders the UI and checks liveness for all tabs.

**Why it is accepted:** Reader threads handle output draining off the UI thread.
Each tab has a dedicated reader thread that continuously reads PTY output and
feeds it to the vt100 parser. The UI thread only locks parsers briefly to call
`.screen()` for rendering. For typical Claude Code usage (1-5 tabs), this is
not a bottleneck. The tick rate (200ms) is fast enough for responsive UI updates
without burning CPU on idle polling.
