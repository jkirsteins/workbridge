use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// -- Protocol constants --

const PROTO_VERSION: u16 = 1;
const TAG_DATA: u8 = 0x00;
const TAG_RESIZE: u8 = 0x01;
const TAG_DETACH: u8 = 0x02;
const TAG_HELLO: u8 = 0x03;
const TAG_ERROR: u8 = 0x04;
const TAG_SERVER_SHUTDOWN: u8 = 0x05;
const HEADER_LEN: usize = 5; // 1 byte tag + 4 byte length

// -- Protocol messages --

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMessage {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Detach,
    Hello { version: u16, cols: u16, rows: u16 },
    Error(String),
    ServerShutdown,
}

impl DaemonMessage {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            DaemonMessage::Data(data) => {
                w.write_all(&[TAG_DATA])?;
                w.write_all(&(data.len() as u32).to_be_bytes())?;
                w.write_all(data)?;
            }
            DaemonMessage::Resize { cols, rows } => {
                w.write_all(&[TAG_RESIZE])?;
                w.write_all(&4u32.to_be_bytes())?;
                w.write_all(&cols.to_be_bytes())?;
                w.write_all(&rows.to_be_bytes())?;
            }
            DaemonMessage::Detach => {
                w.write_all(&[TAG_DETACH])?;
                w.write_all(&0u32.to_be_bytes())?;
            }
            DaemonMessage::Hello {
                version,
                cols,
                rows,
            } => {
                w.write_all(&[TAG_HELLO])?;
                w.write_all(&6u32.to_be_bytes())?;
                w.write_all(&version.to_be_bytes())?;
                w.write_all(&cols.to_be_bytes())?;
                w.write_all(&rows.to_be_bytes())?;
            }
            DaemonMessage::Error(msg) => {
                let bytes = msg.as_bytes();
                w.write_all(&[TAG_ERROR])?;
                w.write_all(&(bytes.len() as u32).to_be_bytes())?;
                w.write_all(bytes)?;
            }
            DaemonMessage::ServerShutdown => {
                w.write_all(&[TAG_SERVER_SHUTDOWN])?;
                w.write_all(&0u32.to_be_bytes())?;
            }
        }
        w.flush()
    }

    pub fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        r.read_exact(&mut header)?;
        let tag = header[0];
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;

        match tag {
            TAG_DATA => {
                let mut buf = vec![0u8; len];
                r.read_exact(&mut buf)?;
                Ok(DaemonMessage::Data(buf))
            }
            TAG_RESIZE => {
                if len != 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("resize payload must be 4 bytes, got {len}"),
                    ));
                }
                let mut buf = [0u8; 4];
                r.read_exact(&mut buf)?;
                let cols = u16::from_be_bytes([buf[0], buf[1]]);
                let rows = u16::from_be_bytes([buf[2], buf[3]]);
                Ok(DaemonMessage::Resize { cols, rows })
            }
            TAG_DETACH => Ok(DaemonMessage::Detach),
            TAG_HELLO => {
                if len != 6 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("hello payload must be 6 bytes, got {len}"),
                    ));
                }
                let mut buf = [0u8; 6];
                r.read_exact(&mut buf)?;
                let version = u16::from_be_bytes([buf[0], buf[1]]);
                let cols = u16::from_be_bytes([buf[2], buf[3]]);
                let rows = u16::from_be_bytes([buf[4], buf[5]]);
                Ok(DaemonMessage::Hello {
                    version,
                    cols,
                    rows,
                })
            }
            TAG_ERROR => {
                let mut buf = vec![0u8; len];
                r.read_exact(&mut buf)?;
                let msg = String::from_utf8(buf).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("invalid UTF-8: {e}"))
                })?;
                Ok(DaemonMessage::Error(msg))
            }
            TAG_SERVER_SHUTDOWN => Ok(DaemonMessage::ServerShutdown),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown message tag: 0x{tag:02x}"),
            )),
        }
    }
}

// -- Error type --

#[derive(Debug)]
pub enum DaemonError {
    Io(io::Error),
    Protocol(String),
    AlreadyAttached,
    VersionMismatch(u16),
    Timeout,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::Io(e) => write!(f, "daemon I/O error: {e}"),
            DaemonError::Protocol(msg) => write!(f, "daemon protocol error: {msg}"),
            DaemonError::AlreadyAttached => write!(f, "another client is already attached"),
            DaemonError::VersionMismatch(v) => {
                write!(
                    f,
                    "protocol version mismatch: server has v{PROTO_VERSION}, client has v{v}"
                )
            }
            DaemonError::Timeout => write!(f, "timed out waiting for daemon server"),
        }
    }
}

impl From<io::Error> for DaemonError {
    fn from(e: io::Error) -> Self {
        DaemonError::Io(e)
    }
}

// -- Server state --

enum ServerState {
    Detached,
    Attached { stream: UnixStream },
}

// -- Path utilities --

pub fn socket_path(socket_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = socket_dir {
        return dir.join("workbridge.sock");
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("workbridge.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/workbridge-{uid}.sock"))
}

pub fn lock_path(socket_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = socket_dir {
        return dir.join("workbridge.lock");
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("workbridge.lock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/workbridge-{uid}.lock"))
}

// -- Return type for start_server --

pub enum ServerRole {
    /// This process is the daemon server. Continue into run_tui().
    /// master_fd is the PTY master for the server IO threads.
    Server { master_fd: OwnedFd },
    /// This process should run as a thin client.
    Client { stream: UnixStream },
}

// -- Low-level PTY helpers (same as session.rs) --

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

fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
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

fn set_cloexec(fd: RawFd) -> io::Result<()> {
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

fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(1, libc::TIOCGWINSZ as libc::c_ulong, &mut ws) };
    if rc < 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        (80, 24)
    } else {
        (ws.ws_col, ws.ws_row)
    }
}

// -- Lock file --

struct LockFile {
    fd: RawFd,
}

impl LockFile {
    fn acquire(path: &Path) -> io::Result<Self> {
        let c_path = std::ffi::CString::new(path.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "lock path is not valid UTF-8")
        })?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        Ok(LockFile { fd })
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
            libc::close(self.fd);
        }
    }
}

// -- Terminal guard for client --

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        use ratatui_crossterm::crossterm::ExecutableCommand;
        use ratatui_crossterm::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
        enable_raw_mode()?;
        std::io::stdout().execute(EnterAlternateScreen)?;
        Ok(TerminalGuard { active: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            use ratatui_crossterm::crossterm::ExecutableCommand;
            use ratatui_crossterm::crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
            let _ = disable_raw_mode();
            let _ = std::io::stdout().execute(LeaveAlternateScreen);
        }
    }
}

// -- Server implementation --

/// Start the daemon server or connect to an existing one.
///
/// Returns `ServerRole::Server` if this process should run the TUI
/// (fd 0/1/2 are redirected to the server PTY slave), or
/// `ServerRole::Client` if an existing server was found.
///
/// When `no_daemon` is true, returns `Ok(None)` to indicate that the
/// caller should run the TUI directly without any daemon layer.
pub fn start_server(
    no_daemon: bool,
    socket_dir: Option<&Path>,
    attach_timeout_secs: u64,
) -> Result<Option<ServerRole>, DaemonError> {
    if no_daemon {
        return Ok(None);
    }

    let sock_path = socket_path(socket_dir);
    let lck_path = lock_path(socket_dir);

    // Ensure the parent directory exists.
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let lock = LockFile::acquire(&lck_path)?;

    // Try to connect to an existing server.
    if sock_path.exists() {
        match try_connect_existing(&sock_path) {
            Ok(stream) => {
                drop(lock);
                return Ok(Some(ServerRole::Client { stream }));
            }
            Err(_) => {
                // Stale socket - remove while holding lock.
                let _ = std::fs::remove_file(&sock_path);
            }
        }
    }

    // No existing server. Fork and start one.
    // Create readiness pipe.
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
        return Err(DaemonError::Io(io::Error::last_os_error()));
    }
    let pipe_read = pipe_fds[0];
    let pipe_write = pipe_fds[1];

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(pipe_read);
            libc::close(pipe_write);
        }
        return Err(DaemonError::Io(io::Error::last_os_error()));
    }

    if pid == 0 {
        // Child process (becomes the daemon server).
        unsafe { libc::close(pipe_read) };

        // New session, detach from terminal.
        if unsafe { libc::setsid() } < 0 {
            let err = io::Error::last_os_error();
            eprintln!("workbridge daemon: setsid failed: {err}");
            unsafe { libc::_exit(1) };
        }

        // Allocate server PTY.
        let (master_fd, slave_fd) = match openpty() {
            Ok(fds) => fds,
            Err(e) => {
                eprintln!("workbridge daemon: openpty failed: {e}");
                unsafe { libc::_exit(1) };
            }
        };

        let master_raw = master_fd.as_raw_fd();
        if let Err(e) = set_cloexec(master_raw) {
            eprintln!("workbridge daemon: set_cloexec failed: {e}");
            unsafe { libc::_exit(1) };
        }

        // Set initial PTY size.
        let _ = set_winsize(master_raw, 80, 24);

        // Redirect fd 0/1/2 to the slave.
        let slave_raw = slave_fd.as_raw_fd();
        unsafe {
            libc::dup2(slave_raw, 0);
            libc::dup2(slave_raw, 1);
            libc::dup2(slave_raw, 2);
        }
        // Close the original slave fd if it's not 0, 1, or 2.
        if slave_raw > 2 {
            drop(slave_fd);
        } else {
            // Prevent OwnedFd from closing a stdio fd.
            std::mem::forget(slave_fd);
        }

        // Set the PTY as the controlling terminal.
        unsafe {
            libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0);
        }

        // Bind the Unix socket.
        let listener = match UnixListener::bind(&sock_path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("workbridge daemon: bind failed: {e}");
                unsafe { libc::_exit(1) };
            }
        };

        // Dup the master for the PTY reader thread.
        let reader_fd_raw = unsafe { libc::dup(master_raw) };
        if reader_fd_raw < 0 {
            eprintln!(
                "workbridge daemon: dup master failed: {}",
                io::Error::last_os_error()
            );
            unsafe { libc::_exit(1) };
        }
        let reader_fd = unsafe { OwnedFd::from_raw_fd(reader_fd_raw) };
        let _ = set_cloexec(reader_fd.as_raw_fd());

        // Shared server state.
        let state = Arc::new(Mutex::new(ServerState::Detached));

        // Spawn permanent PTY reader thread.
        let pty_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("daemon-pty-reader".into())
            .spawn(move || {
                run_pty_reader(reader_fd, pty_state);
            })
            .expect("failed to spawn PTY reader thread");

        // Spawn accept loop thread.
        let accept_state = Arc::clone(&state);
        let accept_master_raw = master_raw;
        let sock_path_clone = sock_path.clone();
        std::thread::Builder::new()
            .name("daemon-accept".into())
            .spawn(move || {
                run_accept_loop(listener, accept_state, accept_master_raw, sock_path_clone);
            })
            .expect("failed to spawn accept loop thread");

        // Signal readiness to parent.
        unsafe {
            libc::write(pipe_write, &1u8 as *const u8 as *const libc::c_void, 1);
            libc::close(pipe_write);
        }

        // Release the lock.
        drop(lock);

        // Return Server role so the caller proceeds to run_tui().
        return Ok(Some(ServerRole::Server { master_fd }));
    }

    // Parent process (becomes the client).
    unsafe { libc::close(pipe_write) };

    // Wait for readiness byte with timeout.
    let ready = wait_for_readiness(pipe_read, attach_timeout_secs);
    unsafe { libc::close(pipe_read) };

    if !ready {
        drop(lock);
        return Err(DaemonError::Timeout);
    }

    // Connect to the server.
    let stream = match try_connect_existing(&sock_path) {
        Ok(s) => s,
        Err(e) => {
            drop(lock);
            return Err(e);
        }
    };

    drop(lock);
    Ok(Some(ServerRole::Client { stream }))
}

/// Try to connect to an existing daemon and do the Hello handshake.
fn try_connect_existing(sock_path: &Path) -> Result<UnixStream, DaemonError> {
    let mut stream = UnixStream::connect(sock_path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let (cols, rows) = terminal_size();
    let hello = DaemonMessage::Hello {
        version: PROTO_VERSION,
        cols,
        rows,
    };
    hello.encode(&mut stream)?;

    let response = DaemonMessage::decode(&mut stream)
        .map_err(|e| DaemonError::Protocol(format!("failed to read Hello response: {e}")))?;

    match response {
        DaemonMessage::Hello { version, .. } => {
            if version != PROTO_VERSION {
                return Err(DaemonError::VersionMismatch(version));
            }
            // Clear timeouts for normal operation.
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
            Ok(stream)
        }
        DaemonMessage::Error(msg) => {
            if msg.contains("already attached") {
                Err(DaemonError::AlreadyAttached)
            } else {
                Err(DaemonError::Protocol(msg))
            }
        }
        other => Err(DaemonError::Protocol(format!(
            "unexpected response: {other:?}"
        ))),
    }
}

/// Wait for a readiness byte on the pipe fd. Returns true if received
/// within timeout, false on timeout or error.
fn wait_for_readiness(pipe_read: RawFd, timeout_secs: u64) -> bool {
    let mut pfd = libc::pollfd {
        fd: pipe_read,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = (timeout_secs * 1000) as i32;
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if rc <= 0 {
        return false;
    }
    let mut byte = 0u8;
    let n = unsafe { libc::read(pipe_read, &mut byte as *mut u8 as *mut libc::c_void, 1) };
    n == 1
}

// -- Server threads --

/// Permanent PTY reader thread. Reads from PTY master and forwards to
/// the attached client (or discards if detached).
fn run_pty_reader(reader_fd: OwnedFd, state: Arc<Mutex<ServerState>>) {
    let fd = reader_fd.as_raw_fd();
    let mut buf = [0u8; 16384];

    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };

        if n > 0 {
            let data = &buf[..n as usize];
            let msg = DaemonMessage::Data(data.to_vec());

            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(_) => break, // mutex poisoned
            };

            if let ServerState::Attached { ref mut stream } = *guard {
                // Set a write timeout to avoid blocking on wedged client.
                let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                if msg.encode(stream).is_err() {
                    // Client is gone or wedged. Transition to Detached.
                    *guard = ServerState::Detached;
                }
            }
            // If Detached, just drop the data.
        } else if n == 0 {
            // EOF - server PTY slave closed. Server is shutting down.
            break;
        } else {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // Real error - exit.
            break;
        }
    }
}

/// Accept loop thread. Listens for client connections.
fn run_accept_loop(
    listener: UnixListener,
    state: Arc<Mutex<ServerState>>,
    master_fd: RawFd,
    sock_path: PathBuf,
) {
    // Set a timeout so we can periodically check if the server is still alive.
    listener
        .set_nonblocking(false)
        .expect("failed to set listener blocking");

    for stream_result in listener.incoming() {
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // Listener error - server probably shutting down.
                break;
            }
        };

        // Read Hello from client (with timeout).
        if stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
        {
            continue;
        }
        if stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .is_err()
        {
            continue;
        }

        let hello = match DaemonMessage::decode(&mut stream) {
            Ok(msg) => msg,
            Err(_) => continue, // invalid client, skip
        };

        let (version, cols, rows) = match hello {
            DaemonMessage::Hello {
                version,
                cols,
                rows,
            } => (version, cols, rows),
            _ => {
                let _ = DaemonMessage::Error("expected Hello".into()).encode(&mut stream);
                continue;
            }
        };

        if version != PROTO_VERSION {
            let _ = DaemonMessage::Error(format!(
                "version mismatch: server v{PROTO_VERSION}, client v{version}"
            ))
            .encode(&mut stream);
            continue;
        }

        // Atomically claim the client slot.
        {
            let mut guard = match state.lock() {
                Ok(g) => g,
                Err(_) => break,
            };

            if matches!(*guard, ServerState::Attached { .. }) {
                let _ = DaemonMessage::Error("already attached".into()).encode(&mut stream);
                continue;
            }

            // Send ACK before transitioning to Attached.
            let ack = DaemonMessage::Hello {
                version: PROTO_VERSION,
                cols: 0,
                rows: 0,
            };
            if ack.encode(&mut stream).is_err() {
                continue;
            }

            // Clear timeouts for the attached stream copy used by PTY reader.
            let _ = stream.set_read_timeout(None);
            let _ = stream.set_write_timeout(None);

            let client_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            };

            *guard = ServerState::Attached {
                stream: client_stream,
            };
        }

        // Resize the PTY to the client's dimensions.
        if cols > 0 && rows > 0 {
            let _ = set_winsize(master_fd, cols, rows);
        }

        // Spawn per-client input reader thread.
        let client_state = Arc::clone(&state);
        let client_master_fd = master_fd;
        std::thread::Builder::new()
            .name("daemon-client-reader".into())
            .spawn(move || {
                run_client_reader(stream, client_state, client_master_fd);
            })
            .expect("failed to spawn client reader thread");
    }

    // Clean up socket on exit.
    let _ = std::fs::remove_file(&sock_path);
}

/// Per-client input reader. Reads from client socket and writes to PTY master.
fn run_client_reader(mut stream: UnixStream, state: Arc<Mutex<ServerState>>, master_fd: RawFd) {
    loop {
        let msg = match DaemonMessage::decode(&mut stream) {
            Ok(m) => m,
            Err(_) => {
                // EOF or error - client disconnected.
                transition_to_detached(&state);
                return;
            }
        };

        match msg {
            DaemonMessage::Data(data) => {
                // Write to PTY master.
                let mut offset = 0;
                while offset < data.len() {
                    let n = unsafe {
                        libc::write(
                            master_fd,
                            data[offset..].as_ptr() as *const libc::c_void,
                            data.len() - offset,
                        )
                    };
                    if n <= 0 {
                        // PTY write error - server shutting down.
                        transition_to_detached(&state);
                        return;
                    }
                    offset += n as usize;
                }
            }
            DaemonMessage::Resize { cols, rows } => {
                let _ = set_winsize(master_fd, cols, rows);
            }
            DaemonMessage::Detach => {
                transition_to_detached(&state);
                return;
            }
            _ => {
                // Unexpected message from client, ignore.
            }
        }
    }
}

fn transition_to_detached(state: &Arc<Mutex<ServerState>>) {
    if let Ok(mut guard) = state.lock() {
        *guard = ServerState::Detached;
    }
}

// -- Client implementation --

/// Run as a thin daemon client, proxying terminal I/O to/from the server.
/// This function does not return on success (calls process::exit).
pub fn run_client(stream: UnixStream) -> ! {
    let _guard = match TerminalGuard::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("workbridge: failed to set up terminal: {e}");
            std::process::exit(1);
        }
    };

    // Install SIGWINCH handler.
    let sigwinch = Arc::new(AtomicBool::new(false));
    if let Err(e) =
        signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&sigwinch))
    {
        eprintln!("workbridge: failed to register SIGWINCH handler: {e}");
        std::process::exit(1);
    }

    // Install SIGHUP handler (SSH disconnect).
    let sighup = Arc::new(AtomicBool::new(false));
    if let Err(e) = signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&sighup)) {
        eprintln!("workbridge: failed to register SIGHUP handler: {e}");
        std::process::exit(1);
    }

    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("workbridge: failed to clone stream: {e}");
            std::process::exit(1);
        }
    };

    // Thread 1: socket -> stdout (server output to terminal)
    let stdout_thread = std::thread::Builder::new()
        .name("client-stdout".into())
        .spawn(move || {
            let mut reader = reader_stream;
            loop {
                let msg = match DaemonMessage::decode(&mut reader) {
                    Ok(m) => m,
                    Err(_) => return, // server disconnected
                };
                match msg {
                    DaemonMessage::Data(data) => {
                        let mut stdout = io::stdout().lock();
                        if stdout.write_all(&data).is_err() {
                            return;
                        }
                        if stdout.flush().is_err() {
                            return;
                        }
                    }
                    DaemonMessage::ServerShutdown => return,
                    DaemonMessage::Error(msg) => {
                        // Restore terminal before printing.
                        use ratatui_crossterm::crossterm::ExecutableCommand;
                        use ratatui_crossterm::crossterm::terminal::{
                            LeaveAlternateScreen, disable_raw_mode,
                        };
                        let _ = disable_raw_mode();
                        let _ = io::stdout().execute(LeaveAlternateScreen);
                        eprintln!("workbridge daemon error: {msg}");
                        return;
                    }
                    _ => {} // ignore unexpected messages
                }
            }
        })
        .expect("failed to spawn client stdout thread");

    // Thread 2: stdin -> socket (user input to server)
    // Also handles SIGWINCH and SIGHUP in a polling loop.
    let mut writer = stream;
    let stdin_fd = io::stdin().as_raw_fd();
    loop {
        // Check SIGHUP.
        if sighup.load(Ordering::Relaxed) {
            let _ = DaemonMessage::Detach.encode(&mut writer);
            break;
        }

        // Check SIGWINCH.
        if sigwinch.swap(false, Ordering::Relaxed) {
            let (cols, rows) = terminal_size();
            let _ = DaemonMessage::Resize { cols, rows }.encode(&mut writer);
        }

        // Poll stdin with a short timeout so we can check signals.
        let mut pfd = libc::pollfd {
            fd: stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 100) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue; // timeout, check signals again
        }

        // Read from stdin.
        let mut buf = [0u8; 8192];
        let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            break; // EOF or error
        }
        let msg = DaemonMessage::Data(buf[..n as usize].to_vec());
        if msg.encode(&mut writer).is_err() {
            break;
        }
    }

    let _ = stdout_thread.join();
    // TerminalGuard drops here, restoring the terminal.
    std::process::exit(0);
}

// -- Unit tests --

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_roundtrip() {
        let messages = vec![
            DaemonMessage::Data(vec![1, 2, 3, 4]),
            DaemonMessage::Data(vec![]),
            DaemonMessage::Resize {
                cols: 120,
                rows: 40,
            },
            DaemonMessage::Detach,
            DaemonMessage::Hello {
                version: 1,
                cols: 80,
                rows: 24,
            },
            DaemonMessage::Error("test error".into()),
            DaemonMessage::ServerShutdown,
        ];

        for msg in &messages {
            let mut buf = Vec::new();
            msg.encode(&mut buf).expect("encode failed");
            let mut cursor = io::Cursor::new(buf);
            let decoded = DaemonMessage::decode(&mut cursor).expect("decode failed");
            assert_eq!(*msg, decoded, "roundtrip failed for {msg:?}");
        }
    }

    #[test]
    fn test_socket_path_respects_env() {
        // When socket_dir is None, the path depends on XDG_RUNTIME_DIR.
        // We can't safely mutate env vars in parallel tests, so just
        // verify the path ends with the expected filename.
        let path = socket_path(None);
        let filename = path.file_name().unwrap().to_str().unwrap();
        if std::env::var("XDG_RUNTIME_DIR").is_ok() {
            assert_eq!(filename, "workbridge.sock");
        } else {
            let uid = unsafe { libc::getuid() };
            assert_eq!(filename, format!("workbridge-{uid}.sock"));
        }
    }

    #[test]
    fn test_socket_path_custom_dir() {
        let path = socket_path(Some(Path::new("/custom/dir")));
        assert_eq!(path, PathBuf::from("/custom/dir/workbridge.sock"));
    }

    #[test]
    fn test_lock_path_matches_socket_path_pattern() {
        let sock = socket_path(Some(Path::new("/custom")));
        let lck = lock_path(Some(Path::new("/custom")));
        assert_eq!(sock.parent(), lck.parent());
        assert_eq!(lck.file_name().unwrap(), "workbridge.lock");
    }

    #[test]
    fn test_decode_unknown_tag() {
        let buf = vec![0xFF, 0, 0, 0, 0]; // unknown tag, 0 length
        let mut cursor = io::Cursor::new(buf);
        let result = DaemonMessage::decode(&mut cursor);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown message tag"));
    }

    #[test]
    fn test_decode_truncated() {
        let buf = vec![TAG_DATA, 0, 0, 0, 5, 1, 2]; // claims 5 bytes but only 2
        let mut cursor = io::Cursor::new(buf);
        let result = DaemonMessage::decode(&mut cursor);
        assert!(result.is_err());
    }
}
