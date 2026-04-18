# Changelog

All notable changes to Workbridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Windows support: the crate now builds and runs on
  `x86_64-pc-windows-msvc`. PTY lifecycle is driven by ConPTY via
  `portable-pty`; signal / termination handling uses `ctrlc` for
  cross-platform Ctrl+C + Ctrl+Break parity with Unix SIGINT/SIGTERM.
- GitHub Actions release workflow
  ([`.github/workflows/release.yml`](.github/workflows/release.yml))
  that attaches pre-built Linux, macOS (x86_64 + aarch64), and Windows
  binaries plus a `SHA256SUMS` manifest to each `v*` tag's GitHub
  Release. No additional credentials are required beyond the default
  `GITHUB_TOKEN`.
- Windows CI coverage: the `clippy` and `test` jobs now run on
  `ubuntu-latest`, `macos-latest`, and `windows-latest` so future PRs
  that regress Windows compatibility are caught at PR time.

### Changed

- Signal handling uses `ctrlc` in place of `signal-hook` for
  cross-platform parity.
- Raw Unix PTY primitives in `src/session.rs` (`openpty` / `setsid` /
  `TIOCSCTTY` / `libc::read` / `libc::write`) are replaced by the
  cross-platform `portable-pty` API. Unix behaviour is preserved:
  sessions still run in their own process group so `killpg` cascades
  to grandchildren. See the updated
  [harness contract](docs/harness-contract.md) clause C10 for the
  Windows divergence.
- `libc` is now a Unix-only target dependency; the Windows build pulls
  in `windows-sys` for the rebase-gate cancellation path instead.
- Added `cargo-release` support and consolidated the release documentation
  into [RELEASING.md](RELEASING.md) ([#130](https://github.com/jkirsteins/workbridge/pull/130)).

## [0.1.1] - 2026-04-18

### Changed

- Update `Cargo.toml` description and keywords to match the agent-agnostic
  README framing.

## [0.1.0] - 2026-04-18

Initial public release.

### Added

- Terminal UI for managing Workbridge work items across Backlog, Planning,
  Implementing, Blocked, Review, Mergequeue, and Done stages.
- Git worktree orchestration for per-item implementation branches.
- Embedded Claude Code and Codex session support through the harness abstraction.
- Repository registration for explicit repos and one-level base directory scans.
- GitHub pull request discovery, review-request import, CI status display, review
  gate checks, merge prechecks, and mergequeue handling.
- Clipboard support, paste routing for TUI text inputs, mouse selection, and
  board/list navigation.

[Unreleased]: https://github.com/jkirsteins/workbridge/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/jkirsteins/workbridge/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jkirsteins/workbridge/releases/tag/v0.1.0
