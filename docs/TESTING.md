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
