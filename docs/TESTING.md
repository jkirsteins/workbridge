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

Tests that need real directories on disk (e.g. to test git repo discovery)
must create them under `std::env::temp_dir()` and clean up after themselves.
Never use hard-coded paths that could collide with real user data.

## No host system side effects

Tests must not leave side effects on the host system. This includes:

- Writing to production config or data directories
- Creating persistent files outside of temp directories
- Modifying environment variables without restoring them
- Spawning processes that outlive the test

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
