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
