# Changelog

All notable changes to Workbridge are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
