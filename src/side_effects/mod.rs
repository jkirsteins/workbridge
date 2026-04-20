//! Host-visible side-effect gate.
//!
//! This module is the ONLY place in the crate that may reach the real
//! system clipboard, `directories::ProjectDirs` / `BaseDirs` / `UserDirs`,
//! `std::env::home_dir`, or emit raw terminal escape sequences. Every
//! public item here is gated by `#[cfg(not(test))]` and returns a
//! deterministic no-op / `None` under `cfg(test)` so `cargo test` cannot
//! touch the host environment.
//!
//! See `docs/TESTING.md` and the P0 review-policy rule in `CLAUDE.md`.
//! The pre-commit hook (`hooks/pre-commit`) enforces this boundary
//! structurally by rejecting staged `.rs` files outside this module
//! that reference the gated symbols.

pub mod clipboard;
pub mod paths;
