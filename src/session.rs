use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Grace period after SIGTERM before escalating to SIGKILL. 50ms is long
/// enough for most well-behaved processes to handle SIGTERM, short enough
/// to avoid noticeable UI lag.
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
/// The session owns the PTY master fd and the child process handle.
/// A dedicated reader thread continuously reads PTY output and feeds it
/// to a shared vt100::Parser behind an Arc<Mutex>. The UI thread never
/// reads from PTY fds - it just locks the parser and calls .screen()
/// to render.
///
/// When the session is dropped, the child process is killed and the
/// reader thread is joined.
pub struct Session {
    master: OwnedFd,
    child: Option<Child>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    reader_handle: Option<JoinHandle<()>>,
}

impl Session {
    /// Spawn a new PTY session running a command with the given dimensions.
    ///
    /// `command` is the program and its arguments (e.g. `&["claude"]` or
    /// `&["sleep", "60"]`). The first element is the program name.
    ///
    /// Opens a PTY pair, sets the slave side to the requested size, forks
    /// the child process with the slave as its controlling terminal, and
    /// returns a Session holding the master fd.
    ///
    /// A reader thread is spawned that continuously reads from a dup'd
    /// copy of the master fd and feeds bytes to the shared parser.
    ///
    /// If `cwd` is provided, the child process starts in that directory.
    /// Otherwise it inherits the parent's working directory.
    pub fn spawn(
        cols: u16,
        rows: u16,
        cwd: Option<&Path>,
        command: &[&str],
    ) -> io::Result<Session> {
        // Open a PTY master/slave pair.
        let (master_fd, slave_fd) = openpty()?;

        let master_raw = master_fd.as_raw_fd();

        // Set FD_CLOEXEC on the master fd so it is not inherited by child
        // processes. Without this, each child would hold open all master fds
        // from other tabs, preventing proper PTY teardown.
        set_cloexec(master_raw)?;

        // Set the initial window size on the slave before the child starts,
        // so the child sees the correct dimensions from the beginning.
        set_winsize(master_raw, cols, rows)?;

        // Both the master fd and the dup'd reader fd stay in blocking mode.
        // Blocking reads are exactly what the reader thread wants, and
        // blocking writes are fine for interactive input (small payloads,
        // PTY buffer is typically 4KB+).

        // Dup the master fd for the reader thread. Both fds share the same
        // underlying file description, but closing one does not close the
        // other.
        let reader_fd_raw = unsafe { libc::dup(master_raw) };
        if reader_fd_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // Wrap in OwnedFd immediately so it is closed on any early return.
        // This prevents fd leaks when Command::spawn() or other setup fails.
        let reader_fd = unsafe { OwnedFd::from_raw_fd(reader_fd_raw) };
        // Set FD_CLOEXEC on the reader fd too.
        set_cloexec(reader_fd.as_raw_fd())?;

        // Consume the OwnedFd to get a raw fd for the slave side. This
        // transfers ownership so there is no double-close. We then dup()
        // for stdout and stderr so each Stdio owns a distinct fd.
        let slave_raw = slave_fd.into_raw_fd();
        let slave_stdout = unsafe { libc::dup(slave_raw) };
        if slave_stdout < 0 {
            // Clean up the original fd on failure. reader_fd is an OwnedFd
            // and will be closed automatically when it drops.
            unsafe { libc::close(slave_raw) };
            return Err(io::Error::last_os_error());
        }
        let slave_stderr = unsafe { libc::dup(slave_raw) };
        if slave_stderr < 0 {
            unsafe { libc::close(slave_raw) };
            unsafe { libc::close(slave_stdout) };
            return Err(io::Error::last_os_error());
        }

        // Build the child process. We need to set up the slave fd as
        // stdin/stdout/stderr and establish a new session with a controlling
        // terminal. This requires a pre_exec hook.
        let program = command.first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "command must not be empty")
        })?;
        let mut cmd = std::process::Command::new(program);
        if command.len() > 1 {
            cmd.args(&command[1..]);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let child = unsafe {
            cmd.stdin(std::process::Stdio::from_raw_fd(slave_raw))
                .stdout(std::process::Stdio::from_raw_fd(slave_stdout))
                .stderr(std::process::Stdio::from_raw_fd(slave_stderr))
                .pre_exec(|| {
                    // Create a new session and set the slave as the
                    // controlling terminal.
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    // TIOCSCTTY: set controlling terminal. Use fd 0 (stdin)
                    // since the original slave fd may have been closed during
                    // stdio setup (dup2 + close). Stdin is guaranteed to
                    // point to the slave PTY at this point. The argument 0
                    // means "don't steal from another session".
                    if libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()?
        };

        // Note: slave fds are now owned by the Command/child process.
        // The Command::spawn consumed the Stdio objects which close the fds
        // in the parent after fork. No explicit close needed here.

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));

        // Spawn the reader thread. It takes ownership of reader_fd (the
        // dup'd master OwnedFd) and a clone of the parser Arc. When the
        // thread exits, reader_fd drops and closes the dup'd fd.
        let parser_clone = Arc::clone(&parser);
        let reader_handle = std::thread::spawn(move || {
            let reader_fd = reader_fd; // move ownership into thread
            let mut buf = [0u8; 16384];
            loop {
                let n = unsafe {
                    libc::read(
                        reader_fd.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n > 0 {
                    match parser_clone.lock() {
                        Ok(mut parser) => {
                            parser.process(&buf[..n as usize]);
                        }
                        Err(_poisoned) => {
                            // Mutex is poisoned (another thread panicked
                            // while holding the lock). The session is in a
                            // broken state - exit the reader thread.
                            break;
                        }
                    }
                } else if n == 0 {
                    // EOF - slave side closed, child exited.
                    break;
                } else {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        // EINTR - a signal interrupted the read. This is
                        // not a real error, just retry.
                        continue;
                    }
                    // Real error (EBADF, EIO, etc.) - fd closed, session
                    // dropped, or something fatal. Exit the reader thread
                    // so the session becomes [dead].
                    break;
                }
            }
        });

        Ok(Session {
            master: master_fd,
            child: Some(child),
            parser,
            reader_handle: Some(reader_handle),
        })
    }

    /// Write bytes to the PTY master (sends input to the child process).
    ///
    /// The master fd is in blocking mode, so write() blocks until the kernel
    /// buffer has space. This is fine for interactive input which is small.
    /// Guards against zero-length writes.
    pub fn write_bytes(&self, data: &[u8]) -> io::Result<()> {
        let fd = self.master.as_raw_fd();
        let mut offset = 0;
        while offset < data.len() {
            let n = unsafe {
                libc::write(
                    fd,
                    data[offset..].as_ptr() as *const libc::c_void,
                    data.len() - offset,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                // write() returned 0 - should not happen on a PTY, but guard
                // against an infinite loop.
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "write to PTY returned 0",
                ));
            }
            offset += n as usize;
        }
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
    /// Locks the parser BEFORE calling set_winsize so that the reader
    /// thread cannot feed new-dimension output into an old-dimension
    /// parser. The lock ensures: lock -> set_winsize (kernel sends
    /// SIGWINCH) -> set_size -> unlock, so parser dimensions are always
    /// in sync when the reader thread next acquires the lock.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if let Ok(mut parser) = self.parser.lock() {
            set_winsize(self.master.as_raw_fd(), cols, rows)?;
            parser.set_size(rows, cols);
        }
        Ok(())
    }

    /// Send SIGTERM to the child's process group without waiting.
    ///
    /// Used during graceful shutdown: the main loop continues running so
    /// the UI stays responsive while children handle SIGTERM at their own
    /// pace. Does not consume the child - liveness checks and reaping
    /// happen via is_alive() on subsequent ticks.
    pub fn send_sigterm(&mut self) {
        let Some(ref child) = self.child else {
            return;
        };
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::killpg(pid, libc::SIGTERM);
        }
    }

    /// Send SIGKILL to the child's process group and reap it.
    ///
    /// Used for force-quit during shutdown and in Drop (crash/panic path).
    /// Consumes the child so no further signals can be sent.
    pub fn force_kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
        let _ = child.wait();
    }

    /// Kill the child process group and wait for it to exit.
    ///
    /// Sends SIGTERM first and waits up to SIGTERM_GRACE_MS before
    /// escalating to SIGKILL. Used for single-tab deletion where a
    /// brief blocking wait is acceptable.
    pub fn kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        let pid = child.id() as libc::pid_t;

        // SIGTERM the entire process group for graceful shutdown.
        unsafe {
            libc::killpg(pid, libc::SIGTERM);
        }

        // Give the process group a brief window to exit gracefully.
        std::thread::sleep(Duration::from_millis(SIGTERM_GRACE_MS));

        // If still alive, force-kill the entire process group.
        if matches!(child.try_wait(), Ok(None)) {
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }

        // Reap the child to prevent zombies.
        let _ = child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Force-kill the child process if still alive. Uses SIGKILL
        // immediately - this is the crash/panic path where no UI is
        // available for graceful shutdown.
        self.force_kill();

        // Close the master fd (happens automatically via OwnedFd drop)
        // which causes the reader thread's read() to fail, making it exit.
        // We need to drop master BEFORE joining the reader thread.
        // However, Rust's drop order for struct fields is declaration order,
        // and we are in Drop::drop where we have &mut self. We need to
        // explicitly close the master fd to unblock the reader thread
        // before joining.

        // Take the master fd and drop it to close, unblocking the reader.
        // We use a raw fd swap: replace the OwnedFd with a dummy, then
        // drop the original. Actually, we can't easily take an OwnedFd
        // out of &mut self. Instead, we join with a timeout or just let
        // the reader thread notice the fd is gone.
        //
        // Since kill() already killed the child, the slave side of the PTY
        // is closed, which means the reader thread's read() will return
        // 0 (EOF). So the reader thread should exit naturally after kill().

        // Join the reader thread. It should have exited because:
        // 1. kill() killed the child, closing the slave side
        // 2. With no writers on the slave side, read() on master returns 0
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Set the FD_CLOEXEC flag on a file descriptor so it is automatically
/// closed when exec() is called in child processes.
fn set_cloexec(fd: std::os::fd::RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Open a PTY master/slave pair using the POSIX openpty interface.
fn openpty() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master_raw: libc::c_int = -1;
    let mut slave_raw: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master_raw,
            &mut slave_raw,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(master_raw) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };
    Ok((master, slave))
}

/// Set the window size on a PTY fd via TIOCSWINSZ ioctl.
fn set_winsize(fd: std::os::fd::RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as libc::c_ulong, &ws) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
