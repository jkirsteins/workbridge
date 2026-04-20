//! Production-path resolvers. Under `cfg(test)` every function returns
//! `None`, which matches the existing `NoConfigDir` error branch callers
//! already handle. Tests that need specific config state construct
//! `Config::for_test()` or use `InMemoryConfigProvider`, not these
//! wrappers.

use std::path::PathBuf;

/// Platform-specific `ProjectDirs` handle for the `workbridge` app.
/// Returns `None` under `cfg(test)` so tests cannot reach the real
/// `~/Library/Application Support/workbridge/` or
/// `~/.config/workbridge/` directories.
#[cfg(not(test))]
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "workbridge")
}

#[cfg(test)]
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    None
}

/// Current user's home directory. Returns `None` under `cfg(test)` so
/// tests cannot reach the real `$HOME`.
#[cfg(not(test))]
pub fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

#[cfg(test)]
pub fn home_dir() -> Option<PathBuf> {
    None
}

/// The process-wide temp directory, via `std::env::temp_dir()`. This
/// exists so production code that writes workbridge-owned temp files
/// (e.g. `workbridge-mcp-config-<uuid>.json` under the UI thread
/// session bootstrap) can route through a single wrapper rather than
/// calling `std::env::temp_dir()` directly at every site. The
/// pre-commit hook enforces that this is the only module that may
/// reference `std::env::temp_dir`.
///
/// NOT cfg(test)-gated: the underlying call honours `$TMPDIR`, so tests
/// that need a scratch directory should still prefer `tempfile::tempdir()`
/// (which produces a collision-free subdirectory under the same root
/// and auto-removes on drop), but code paths that merely ask "where is
/// the process temp root" - for example the test helper in
/// `src/app.rs` that enumerates leaked `workbridge-mcp-config-*.json`
/// files to assert cleanup - go through this wrapper.
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}
