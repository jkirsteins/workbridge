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
