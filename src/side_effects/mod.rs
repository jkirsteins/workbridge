//! Host-visible side-effect gate.
//!
//! This module is the ONLY place in the crate that may reach the real
//! system clipboard, `directories::ProjectDirs` / `BaseDirs` / `UserDirs`,
//! `std::env::home_dir`, `std::env::temp_dir`, or emit raw terminal
//! escape sequences.
//!
//! Most public items here are gated by `#[cfg(not(test))]` and return
//! a deterministic no-op / `None` under `cfg(test)` so `cargo test`
//! cannot touch the host environment (e.g. `clipboard::copy` returns
//! `false`, `paths::project_dirs` and `paths::home_dir` return `None`).
//!
//! The one exception is `paths::temp_dir()`, which is NOT
//! `#[cfg(test)]`-gated: it wraps `std::env::temp_dir()` so production
//! code that writes workbridge-owned temp files (e.g. the per-session
//! MCP config) has a single authoritative call site, but under tests
//! the underlying syscall still honours `$TMPDIR` and is harmless.
//! Tests that need scratch directories MUST use `tempfile::tempdir()`
//! (which produces a collision-free subdir and auto-removes on drop),
//! NOT this wrapper - see `docs/TESTING.md` for the rationale and the
//! reviewer-enforced rule.
//!
//! See `docs/TESTING.md` and the P0 review-policy rule in `CLAUDE.md`.
//! The pre-commit hook (`hooks/pre-commit`) enforces this boundary
//! structurally by rejecting staged `.rs` files outside this module
//! that reference the gated symbols.

pub mod clipboard;
pub mod paths;
