use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Grace period after SIGTERM before escalating to SIGKILL. 50ms is long
/// enough for most well-behaved processes to handle SIGTERM, short enough
/// to avoid noticeable UI lag.
///
/// Only used on Unix; Windows has no SIGTERM analogue so there is no
/// escalation window to honour.
#[cfg(unix)]
const SIGTERM_GRACE_MS: u64 = 50;

/// Number of scrollback lines retained by the vt100 parser. Lines that
/// scroll off the top of the visible terminal are kept in this buffer so
/// the user can scroll back through past output.
///
/// Note: vt100's `visible_rows()` has a usize underflow bug when
/// `scrollback_offset > terminal_rows`, so the viewport can only scroll
/// back one screenful at a time. The full buffer is still retained for
/// future use (e.g. if vt100 is patched or a custom renderer is added).
pub const SCROLLBACK_LINES: usize = 10_000;

/// A PTY-backed session running a child process (e.g. `claude`).
///
/// The session owns the PTY master handle and the child process. A
/// dedicated reader thread continuously reads PTY output and feeds it
/// to a shared vt100::Parser behind an Arc<Mutex>. The UI thread never
/// reads from PTY handles - it just locks the parser and calls .screen()
/// to render.
///
/// Cross-platform PTY primitives are provided by `portable-pty`, which
/// uses Unix PTYs on Unix and ConPTY (`CreatePseudoConsole`) on Windows.
/// The call sites that wrap the PTY lifecycle (spawn / write / resize /
/// kill / drop) keep the same signatures on both platforms; the
/// divergences are:
///
/// - Unix uses `killpg` + `SIGTERM` / `SIGKILL` on the child's process
///   group (every PTY-spawned child is a session leader, so its PID
///   equals its PGID - this matches the old raw-libc behaviour).
/// - Windows has no signal model; graceful-vs-force is collapsed into a
///   single `Child::kill` (TerminateProcess). Grandchildren are not
///   cascaded on Windows, which is acceptable for the current harness
///   (the reference `claude` binary is not distributed for Windows today
///   and the rebase-gate subprocess tree is short-lived). See
///   `docs/harness-contract.md` clause C10.
///
/// When the session is dropped, the child process is killed and the
/// reader thread is joined.
pub struct Session {
    /// PTY master handle. Dropping it closes the master fd / ConPTY
    /// handle, which in turn causes the reader thread's blocking read
    /// to observe EOF and exit.
    master: Box<dyn MasterPty + Send>,
    /// Child handle for liveness polling and kill. Consumed by
    /// `force_kill` / `kill` / `Drop::drop`.
    child: Option<Box<dyn Child + Send + Sync>>,
    /// Cached child process id. `Child::process_id` returns `None` after
    /// the child has been waited on, but we still need the pid for
    /// `killpg` on Unix; caching it at spawn time means a late signal
    /// against a freshly-reaped PID is at worst `ESRCH` (harmless)
    /// instead of a silent no-op.
    child_pid: Option<u32>,
    /// Writer for PTY input (keystrokes, pasted text). `portable-pty`'s
    /// `Box<dyn Write + Send>` is itself not `Sync`, so we wrap in a
    /// `Mutex` to let `write_bytes(&self, ...)` keep its existing
    /// shared-reference signature without forcing every caller to lock
    /// the Session.
    writer: Mutex<Box<dyn Write + Send>>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    reader_handle: Option<JoinHandle<()>>,
}

impl Session {
    /// Spawn a new PTY session running a command with the given dimensions.
    ///
    /// `command` is the program and its arguments (e.g. `&["claude"]` or
    /// `&["sleep", "60"]`). The first element is the program name.
    ///
    /// Opens a PTY pair via `portable_pty::native_pty_system`, sets the
    /// requested size, spawns the child attached to the PTY slave, and
    /// returns a Session holding the master handle.
    ///
    /// A reader thread is spawned that continuously reads from a cloned
    /// reader obtained from the master handle and feeds bytes to the
    /// shared parser.
    ///
    /// If `cwd` is provided, the child process starts in that directory.
    /// Otherwise it inherits the parent's working directory.
    pub fn spawn(
        cols: u16,
        rows: u16,
        cwd: Option<&Path>,
        command: &[&str],
    ) -> io::Result<Session> {
        let program = command.first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "command must not be empty")
        })?;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)?;

        // Build the command. portable-pty's CommandBuilder inherits the
        // parent environment by default (matching std::process::Command).
        let mut cmd_builder = CommandBuilder::new(program);
        for arg in &command[1..] {
            cmd_builder.arg(arg);
        }
        if let Some(dir) = cwd {
            cmd_builder.cwd(dir);
        }

        // Spawn the child attached to the PTY slave. On Unix this
        // internally runs the same `setsid` + `TIOCSCTTY` dance the
        // old raw-libc implementation did, so the child is its own
        // session leader and its PID equals its process-group id
        // (which is what `killpg` in `send_sigterm` / `force_kill` /
        // `kill` relies on). On Windows it allocates a ConPTY via
        // `CreatePseudoConsole` and wires the child's stdio to it.
        let child = pair
            .slave
            .spawn_command(cmd_builder)
            .map_err(io::Error::other)?;
        let child_pid = child.process_id();

        // Drop the slave in the parent. Matches the old `FD_CLOEXEC`
        // behaviour: the parent must not keep the slave alive, or the
        // reader will never observe EOF when the child exits.
        drop(pair.slave);

        // Reader side: a blocking stream of bytes coming out of the PTY.
        // `try_clone_reader` produces an independent handle on both Unix
        // (dup'd fd) and Windows (cloned ConPTY output handle).
        let mut reader = pair.master.try_clone_reader().map_err(io::Error::other)?;
        // Writer side: use-once per process (portable-pty internally
        // tracks whether the writer has been taken). One writer is
        // enough; we keep it behind a Mutex so `write_bytes(&self)` stays
        // compatible with the existing call sites.
        let writer = pair.master.take_writer().map_err(io::Error::other)?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));

        // Spawn the reader thread. It owns the cloned reader; when the
        // master side is closed (on Session drop or when the child
        // exits and portable-pty closes the underlying handle), read()
        // returns Ok(0) and the thread exits naturally.
        let parser_clone = Arc::clone(&parser);
        let reader_handle = std::thread::spawn(move || {
            let mut buf = [0u8; 16384];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF - child exited / master closed
                    Ok(n) => match parser_clone.lock() {
                        Ok(mut parser) => {
                            parser.process(&buf[..n]);
                        }
                        Err(_poisoned) => {
                            // Mutex is poisoned (another thread panicked
                            // while holding the lock). The session is in
                            // a broken state - exit the reader thread.
                            break;
                        }
                    },
                    Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
                        // EINTR - a signal interrupted the read. This is
                        // not a real error, just retry.
                        continue;
                    }
                    Err(_) => {
                        // Real error (EBADF, EIO, ConPTY closed, etc.) -
                        // exit the reader thread so the session becomes
                        // [dead].
                        break;
                    }
                }
            }
        });

        Ok(Session {
            master: pair.master,
            child: Some(child),
            child_pid,
            writer: Mutex::new(writer),
            parser,
            reader_handle: Some(reader_handle),
        })
    }

    /// Write bytes to the PTY master (sends input to the child process).
    ///
    /// The writer is blocking, so this blocks until the kernel / ConPTY
    /// buffer has space. This is fine for interactive input which is
    /// small. Guards against zero-length writes.
    pub fn write_bytes(&self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("session writer mutex poisoned"))?;
        guard.write_all(data)?;
        guard.flush()?;
        Ok(())
    }

    /// Check if the child process is still running.
    ///
    /// Only marks the child as dead when we have definitive proof via
    /// Ok(Some(status)). On any error (EINTR or otherwise), the child is
    /// assumed alive - if truly dead, the next tick will catch it.
    pub fn is_alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false; // already reaped
        };
        match child.try_wait() {
            Ok(Some(_status)) => {
                // Definitively exited - consume the Child.
                self.child.take();
                false
            }
            Ok(None) => true, // still running
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
                // EINTR - transient, preserve prior state (alive).
                true
            }
            Err(_) => {
                // Other error - preserve prior state rather than falsely
                // marking dead. The child may still be alive, we just
                // could not check. If truly dead, the next tick catches it.
                true
            }
        }
    }

    /// Resize the PTY and the parser to the given dimensions.
    ///
    /// Locks the parser BEFORE calling the PTY resize so that the reader
    /// thread cannot feed new-dimension output into an old-dimension
    /// parser. The lock ensures: lock -> resize (kernel sends SIGWINCH
    /// on Unix / ConPTY notifies on Windows) -> parser.set_size ->
    /// unlock, so parser dimensions are always in sync when the reader
    /// thread next acquires the lock.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if let Ok(mut parser) = self.parser.lock() {
            self.master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(io::Error::other)?;
            parser.set_size(rows, cols);
        }
        Ok(())
    }

    /// Request graceful shutdown of the child process without waiting.
    ///
    /// On Unix: sends SIGTERM to the child's process group (`killpg`),
    /// matching the original raw-libc behaviour - any grandchildren the
    /// harness spawned inside the PTY session are signalled too. Used
    /// during graceful shutdown: the main loop continues running so the
    /// UI stays responsive while children handle SIGTERM at their own
    /// pace.
    ///
    /// On Windows: there is no SIGTERM analogue; `Child::kill` calls
    /// `TerminateProcess`, which terminates the direct child immediately
    /// and does NOT cascade to grandchildren. The 10-second shutdown
    /// deadline in `main.rs` still escalates to `force_kill`, so the
    /// observable UX (a second Ctrl+C force-quits) is the same.
    ///
    /// Does not consume the child - liveness checks and reaping happen
    /// via `is_alive()` on subsequent ticks.
    pub fn send_sigterm(&mut self) {
        #[cfg(unix)]
        {
            if self.child.is_some()
                && let Some(pid) = self.child_pid
            {
                // SAFETY: `libc::killpg` is an FFI call into a
                // stable POSIX syscall; arguments are a process-
                // group id and a signal number, both plain ints.
                // The child was spawned by portable-pty which
                // internally calls `setsid`, so its PID equals its
                // process-group id. `ESRCH` after a freshly-reaped
                // group is harmless.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGTERM);
                }
            }
        }
        #[cfg(windows)]
        {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    /// Force-kill the child and reap it.
    ///
    /// On Unix: sends SIGKILL to the child's process group. On Windows:
    /// `TerminateProcess` on the direct child. Used for force-quit
    /// during shutdown and in `Drop` (crash/panic path). Consumes the
    /// child so no further signals can be sent.
    pub fn force_kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(unix)]
        {
            if let Some(pid) = self.child_pid {
                // SAFETY: see `send_sigterm`.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = child.kill();
        }
        let _ = child.wait();
    }

    /// Kill the child and wait for it to exit.
    ///
    /// On Unix: sends SIGTERM to the process group, waits
    /// `SIGTERM_GRACE_MS`, and escalates to SIGKILL if the child is
    /// still alive - matches the original blocking-shutdown path.
    /// On Windows: single `TerminateProcess` (no signal distinction is
    /// possible). In both cases the child is reaped to prevent zombies.
    pub fn kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        #[cfg(unix)]
        {
            if let Some(pid) = self.child_pid {
                // SIGTERM the entire process group for graceful shutdown.
                // SAFETY: see `send_sigterm`.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGTERM);
                }

                // Give the process group a brief window to exit gracefully.
                std::thread::sleep(Duration::from_millis(SIGTERM_GRACE_MS));

                // If still alive, force-kill the entire process group.
                if matches!(child.try_wait(), Ok(None)) {
                    unsafe {
                        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            // Windows has no SIGTERM; a single TerminateProcess is the
            // best we can do. Matches the escalation target on Unix.
            let _ = child.kill();
        }

        // Reap the child to prevent zombies.
        let _ = child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Force-kill the child process if still alive. Uses SIGKILL
        // immediately on Unix / TerminateProcess on Windows - this is the
        // crash/panic path where no UI is available for graceful shutdown.
        self.force_kill();

        // The master handle closes automatically when `self.master` drops
        // (declaration order: `master` is declared first, so it drops
        // last - but that's fine because `force_kill` above has already
        // reaped the child, and the slave handle we dropped at spawn time
        // means the reader thread sees EOF as soon as the child exits).

        // Join the reader thread. It should have exited because:
        // 1. force_kill killed the child, closing the slave side
        // 2. With no writers on the slave side, the reader's read()
        //    returns 0 (EOF) on Unix / ConPTY returns broken-pipe on
        //    Windows.
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn tests use `sleep`, which is a POSIX utility. On Windows the
    /// equivalent is either `cmd /c timeout` or `powershell -c
    /// Start-Sleep`; both have different argv shapes than `sleep`, so
    /// rather than mud the tests with platform detection we cfg-gate
    /// these lifecycle tests to Unix. The cross-platform PTY lifecycle
    /// is exercised on Windows via the integration-style smoke tests
    /// that spawn a real harness in the wider test matrix.
    #[cfg(unix)]
    #[test]
    fn kill_is_idempotent() {
        // Spawn a real child process (sleep) so we can test the full
        // kill -> reap -> child=None -> second call is no-op flow.
        let mut session =
            Session::spawn(80, 24, None, &["sleep", "60"]).expect("failed to spawn session");

        // Precondition: child starts as Some.
        assert!(session.child.is_some(), "child should start as Some");

        // First kill: should signal and reap the child, then set child to None.
        session.kill();
        assert!(
            session.child.is_none(),
            "child should be None after first kill()"
        );

        // Second kill: should return immediately without signaling.
        // If this were not idempotent, it could signal a reused PID/PGID.
        session.kill();
        assert!(
            session.child.is_none(),
            "child should remain None after second kill()"
        );

        // Drop will call kill() a third time - also a no-op.
    }

    #[cfg(unix)]
    #[test]
    fn is_alive_lifecycle() {
        // Spawn a short-lived process so we can observe alive -> dead.
        let mut session =
            Session::spawn(80, 24, None, &["sleep", "0"]).expect("failed to spawn session");

        // Right after spawn the child should still be alive (or may have
        // already exited - sleep 0 is instant). Either way, after waiting
        // for the child to finish, is_alive must return false.

        // Wait a bit for the process to exit.
        std::thread::sleep(Duration::from_millis(200));

        // After the process exits, is_alive should return false.
        assert!(
            !session.is_alive(),
            "child should be dead after sleep 0 exits"
        );

        // Once dead, child should be consumed (taken).
        assert!(
            session.child.is_none(),
            "child should be None after confirmed dead"
        );

        // Calling is_alive again on a reaped child should return false.
        assert!(
            !session.is_alive(),
            "is_alive should return false after reap"
        );
    }

    /// Regression: vt100's visible_rows() panics when scrollback_offset
    /// exceeds terminal rows due to usize underflow at
    /// `rows_len - self.scrollback_offset`. This test documents the bug
    /// so we know when/if vt100 fixes it.
    #[test]
    #[should_panic]
    fn scrollback_offset_exceeding_rows_panics_in_vt100() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        for i in 0..200 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        // set_scrollback clamps to scrollback.len() (100), so 30 is
        // valid from its perspective but exceeds rows (24).
        parser.set_scrollback(30);
        let _ = parser.screen().cell(0, 0);
    }

    /// Verify that clamping scrollback_offset to terminal rows avoids
    /// the vt100 panic.
    #[test]
    fn scrollback_offset_clamped_to_rows_does_not_panic() {
        let mut parser = vt100::Parser::new(24, 80, 100);
        for i in 0..200 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        let rows = parser.screen().size().0 as usize;
        // Simulate what the render path does: clamp before set_scrollback.
        let clamped = 30_usize.min(rows);
        parser.set_scrollback(clamped);
        assert!(parser.screen().cell(0, 0).is_some());
    }
}
