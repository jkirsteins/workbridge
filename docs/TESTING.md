# Testing Rules

## No production side effects

Tests must NEVER read or write the real config file
(`~/Library/Application Support/workbridge/config.toml` on macOS,
`~/.config/workbridge/config.toml` on Linux). A test that touches the
real config file will silently clobber user data.

## Use InMemoryConfigProvider

All tests that construct an `App` must use `InMemoryConfigProvider` (or an
equivalent mock) for config persistence. The convenience constructors
`App::new()` and `App::with_config()` do this automatically. If you call
`App::with_config_and_worktree_service()` directly, pass
`Box::new(InMemoryConfigProvider::new())` as the `config_provider` argument.

## Use temp directories for filesystem operations

Tests that need real directories on disk must use `tempfile::tempdir()`.
The returned `TempDir` binds the directory to a unique path and removes
it on drop, so parallel test threads cannot collide and `/tmp` does not
accumulate predictable `workbridge-test-*` directories between runs.

Pattern:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
// ... test body uses `dir` ...
// _tmp is dropped at end of scope and removes the directory
```

Do NOT use `std::env::temp_dir().join("fixed-name")`. The pre-commit
hook rejects bare `std::env::temp_dir` outside `src/side_effects/`.
UUID-suffixed names (`std::env::temp_dir().join(format!("...{}", Uuid::new_v4()))`)
are technically collision-safe but still pollute `/tmp`; prefer
`tempfile::tempdir()` for uniformity.

## No host system side effects

Tests must not leave side effects on the host system. This includes:

- Writing to production config or data directories
- Creating persistent files outside of temp directories
- Modifying environment variables without restoring them
- Spawning processes that outlive the test
- Writing to the system clipboard (via `arboard`, OSC 52, `NSPasteboard`,
  or any other path)
- Writing raw terminal escape sequences to stdout / stderr outside
  `src/side_effects/`
- Reading wall-clock time or sleeping via `Instant::now`, `SystemTime::now`,
  `thread::sleep`, `std::thread::sleep`, `Instant::elapsed()`,
  `Receiver::recv_timeout` / `recv_deadline`, `Condvar::wait_timeout(_while)`,
  or `Thread::park_timeout` outside `src/side_effects/clock.rs`
- Using notification, audio, or visual system APIs

## Side-effect gating module

All code paths that reach the host system outside `std::env::temp_dir()`
live in `src/side_effects/`. That module is the ONLY place in the crate
allowed to call `arboard::`, `directories::ProjectDirs` / `BaseDirs` /
`UserDirs`, `std::env::home_dir`, `std::env::temp_dir`, read wall-clock
time, sleep the current thread, or write raw terminal escape sequences
(such as OSC 52) to stdout.

Under `#[cfg(test)]` every gated wrapper in `side_effects::` returns a
no-op (`copy` returns `false`) or `None` (`paths::project_dirs`,
`paths::home_dir`). That maps cleanly to existing error branches
(`ConfigError::NoConfigDir`, `BackendError::Io("could not determine
data directory")`) that tests already exercise through
`InMemoryConfigProvider` and `LocalFileBackend::with_dir`.

The pre-commit hook (`hooks/pre-commit`) enforces the boundary
structurally: a staged `.rs` file outside `src/side_effects/` that
references any of the gated symbols is rejected at commit time. See
the P0 rule in `CLAUDE.md` "Severity overrides" for the review policy.

## Wall-clock in tests

Tests must not read real wall-clock time or block on real sleeps. Use
`crate::side_effects::clock::instant_now()`,
`crate::side_effects::clock::system_now()`, and
`crate::side_effects::clock::sleep()` instead of `Instant::now`,
`SystemTime::now`, `thread::sleep`, or `std::thread::sleep`.

In production these wrappers forward to the standard library. Under
`#[cfg(test)]`, `instant_now()` and `system_now()` read a deterministic
mock clock, and `sleep()` advances that mock clock while yielding the
current thread. Polling loops therefore terminate in tests without waiting
for real time to pass. The mock `SystemTime` starts at
`UNIX_EPOCH + 1_700_000_000s`, so ordinary subtraction in tests has room
before the Unix epoch.

The mock clock is **per-OS-thread**, not process-global: both the
synthetic-time offset and the sleep-safety counter live in
`thread_local! { Cell<u64> }` storage. `cargo test`'s default libtest
harness spawns a fresh OS thread per test, so each test observes its
own mock clock starting at offset zero - two parallel tests cannot
advance each other's clock, and cannot trip each other's safety cap.
This keeps the "deterministic mock clock" contract true under
`cargo test` default parallelism. Thread-local values persist for the
lifetime of their thread; if a future test harness reuses OS threads
across tests, each test still observes a monotonic clock but offsets
accumulate. That matches the libtest default today.

In addition to the `Instant::now` / `SystemTime::now` / `thread::sleep`
wrappers, tests must NOT read the wall-clock via:

- `Instant::elapsed()` - this expands to `Instant::now() - *self` and
  silently falls back to the real clock even when `self` was captured
  via the mock `instant_now()` wrapper. Use
  `crate::side_effects::clock::elapsed_since(start)` instead, which
  diffs against the mock clock in tests and the real clock in
  production.
- `Receiver::recv_timeout`, `Receiver::recv_deadline`,
  `Condvar::wait_timeout(_while)`, `Thread::park_timeout` - these
  stdlib bounded-wait APIs internally read the monotonic clock via
  `Condvar::wait_timeout`. Tests that need a bounded receive must
  use `crate::side_effects::clock::bounded_recv(&rx, "context")`,
  the shared generic helper that polls `try_recv` on a mock-clock
  driven timer. It works with both `std::sync::mpsc::Receiver` and
  `crossbeam_channel::Receiver` via the `PollableReceiver` trait
  impls in `src/side_effects/clock.rs`. Adding a new channel kind
  only requires another trait impl.

The pre-commit hook rejects staged Rust files outside `src/side_effects/`
that call any of these APIs directly (`.elapsed(`, `recv_timeout(`,
`recv_deadline(`, `wait_timeout(`, `park_timeout(` are all on the
forbidden list alongside `Instant::now`, `SystemTime::now`, and
`thread::sleep`). If a test needs to move time forward without going
through a sleep path, call
`crate::side_effects::clock::advance_mock_clock()` from test-only code.

If you genuinely need a new host-visible side effect (for example, a
new notification API), the add path is:

1. Add the call inside `src/side_effects/` behind `#[cfg(not(test))]`,
   returning a no-op / `None` / `false` under `cfg(test)`.
2. Expose a narrow wrapper from `side_effects::` and route callers
   through it.
3. Update `docs/TESTING.md` (this file) and the pre-commit hook's
   grep pattern if the new API uses a new symbol name.

Pre-authorized bounded exceptions that live outside the module and are
documented here rather than routed through the gate:

- `src/session.rs` spawns short-lived `sleep 60` / `sleep 0` child
  processes for PTY lifecycle smoke tests. These subprocesses are
  reaped by the test itself and do not outlive it.
- `src/mcp.rs` binds a UUID-suffixed Unix socket under the process temp
  dir for the socket-server smoke test. The UUID makes the path
  collision-free and the socket is unlinked during test teardown.

## Never use `git config` in tests

Tests must NEVER call `git config` to set values, even in temp directories.
In git worktrees, `git config --local` writes to the PARENT repo's
`.git/config`, not the worktree's. This means a test that calls
`git config user.email` in a worktree can poison the real repo's config.

Instead, use `-c` flags on git commands that need author identity:
```
git -c user.email=test@test.com -c user.name=Test commit -m "message"
```

This sets values for a single command without writing to any config file.

## Use `git_command()` for all git subprocesses

All code that spawns `git` as a child process must use
`worktree_service::git_command()` instead of `Command::new("git")`. This
helper clears inherited git env vars (`GIT_DIR`, `GIT_WORK_TREE`, etc.) that
git sets when running inside hooks or worktrees. Without clearing, child
processes operate on the parent repo instead of their target directory -
which is how `core.bare=true` corruption happened.

The pre-commit hook enforces this: any staged `.rs` file with
`Command::new("git")` that lacks `env_remove("GIT_DIR")` or `git_command()`
will be rejected.

## Integration tests

Tests that shell out to real `git` commands (creating repos, worktrees,
branches) are gated behind the `integration` Cargo feature. They do not
run on `cargo test` by default.

Run them explicitly:
```sh
cargo test --features integration
```

The pre-push hook runs `cargo test --all-features` which includes
integration tests. This ensures they pass before code reaches the remote.

Integration tests live in `src/worktree_service.rs` in the
`integration_tests` module. Unit tests (like `parse_porcelain` tests that
don't touch the filesystem) remain in the regular `tests` module and run
on every `cargo test`.

## Sandbox-only verification

Some local agent sandboxes allow ordinary temp-file I/O but deny
Unix-domain socket creation. In that environment, the MCP socket smoke
tests fail while binding the socket with `PermissionDenied` /
`Operation not permitted`, even though the same tests pass from a normal
developer shell.

For sandbox-only verification, exclude only these socket-dependent tests:

```sh
cargo test -- \
  --skip mcp::tests::socket_server_starts_and_stops \
  --skip mcp::tests::mcp_tool_call_produces_channel_event
```

This exception is only for restricted sandboxes that cannot bind Unix
sockets. Normal local shells, CI, and git hooks must still run the full
test suite, including those two MCP socket tests. If a sandbox run skips
them, verify them separately outside the sandbox before relying on MCP
socket behavior:

```sh
cargo test mcp::tests::socket_server_starts_and_stops -- --nocapture
cargo test mcp::tests::mcp_tool_call_produces_channel_event -- --nocapture
```

## CI gate

GitHub Actions (`.github/workflows/ci.yml`) runs on every pull request and
on pushes to `master` with the following parallel jobs:

- `fmt` - `cargo fmt --all -- --check`
- `clippy` - `cargo clippy --all-targets --all-features -- -D warnings`
- `test` - `cargo test --all-features`
- `audit` - `cargo audit` (RustSec advisory database)
- `deny` - `cargo deny check advisories licenses bans sources`
- `machete` - `cargo machete` (unused dependency scan)
- `typos` - `crate-ci/typos` action with `typos.toml`
- `msrv` - `cargo check --all-features` pinned to the rust-version in `Cargo.toml` (1.88)
- `budget` - `./hooks/budget-check.sh` enforcing the uniform 700-line ceiling on every tracked `src/**/*.rs` file
- `ratatui-builtin` - `./hooks/ratatui-builtin-check.sh` (warn-only heuristic)

The `test` job uses `--all-features` deliberately so the merge gate exercises
the same integration tests as the pre-push hook. Keeping the two in sync
means the "integration tests pass before code reaches the remote" invariant
above holds even when a developer bypasses the local hook.

`audit`, `deny`, `machete`, and `typos` are the hard CI counterparts of
the pre-commit / pre-push checks that skip locally with an install hint
when the tool is missing. `budget` and `ratatui-builtin` re-run the same
hook scripts CI-side so a local hook bypass still gets caught.

The repository ruleset references the pre-existing job names (`fmt`, `clippy`,
`test`) as required status checks. New jobs added in Phase 1 of the hygiene
campaign (`audit`, `deny`, `machete`, `typos`, `msrv`, `budget`,
`ratatui-builtin`) run on every PR but are not yet required-status-check
gated; promote them to required via the repository ruleset once they have
a stable green baseline.
