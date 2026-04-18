# Crates.io First Publish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prepare Workbridge for a manual first publish to crates.io as the `workbridge` binary crate.

**Architecture:** Keep publishing manual and documentation-driven. Update crate metadata, install docs, changelog, and release checklist, then use Cargo package commands to verify the shipped crate contents and publish readiness. Use `exclude` in `Cargo.toml` only to remove internal agent/workflow files and development-only artifacts from the packaged crate.

**Tech Stack:** Rust 2024, Cargo packaging and publishing, Markdown documentation.

---

## File Structure

- Modify `Cargo.toml`: add crates.io metadata and a focused package `exclude` list.
- Modify `README.md`: document `cargo install workbridge` as the public install path and keep `cargo install --path .` for local development.
- Create `CHANGELOG.md`: add an initial `0.1.0` release entry.
- Create `docs/release-checklist.md`: add the manual release checklist.
- Read-only reference `docs/superpowers/specs/2026-04-18-crates-io-first-publish-design.md`: confirm implementation stays within the approved design.

## Task 1: Package Metadata And Package Contents

**Files:**
- Modify: `Cargo.toml`
- Reference: `docs/superpowers/specs/2026-04-18-crates-io-first-publish-design.md`

- [ ] **Step 1: Reconfirm the approved metadata scope**

Run:

```sh
sed -n '1,140p' docs/superpowers/specs/2026-04-18-crates-io-first-publish-design.md
```

Expected: the spec says the crate remains named `workbridge`, adds conservative crates.io metadata, and uses `docs/release-checklist.md` for the manual checklist.

- [ ] **Step 2: Inspect current package contents**

Run:

```sh
cargo package --list
```

Expected before this task: output includes internal/development files such as `.claude/`, `docs/superpowers/`, `hooks/`, `CLAUDE.md`, `AGENTS.md`, `README`, and `workbridge-sidebar-playground.html`.

- [ ] **Step 3: Update package metadata and exclude internal files**

Edit the `[package]` section of `Cargo.toml` so it contains these fields:

```toml
[package]
name = "workbridge"
version = "0.1.0"
edition = "2024"
license = "MIT"
description = "Multi-repo Claude Code orchestration in your terminal."
repository = "https://github.com/jkirsteins/workbridge"
homepage = "https://github.com/jkirsteins/workbridge"
readme = "README.md"
keywords = ["claude", "git", "tui", "workflow", "worktree"]
categories = ["command-line-utilities", "development-tools"]
exclude = [
    ".claude/**",
    "AGENTS.md",
    "CLAUDE.md",
    "README",
    "docs/superpowers/**",
    "hooks/**",
    "workbridge-sidebar-playground.html",
]
```

Keep the existing `[dependencies]`, `[features]`, and `[dev-dependencies]` sections unchanged.

- [ ] **Step 4: Verify the manifest parses**

Run:

```sh
cargo metadata --no-deps --format-version 1
```

Expected: command exits 0 and the JSON contains `"name":"workbridge"`.

- [ ] **Step 5: Verify package contents after excludes**

Run:

```sh
cargo package --list
```

Expected: output no longer includes `.claude/`, `docs/superpowers/`, `hooks/`, `CLAUDE.md`, `AGENTS.md`, `README`, or `workbridge-sidebar-playground.html`. Output still includes `Cargo.toml`, `Cargo.lock`, `LICENSE`, `README.md`, `assets/logo.png`, `docs/*.md`, `prompts/stage_prompts.json`, and `src/**`.

- [ ] **Step 6: Commit package metadata**

Run:

```sh
git add Cargo.toml
git commit -m "Prepare crate metadata for crates.io"
```

Expected: pre-commit hook runs format, clippy, git env checks, and agent spawn checks; commit succeeds.

## Task 2: README Installation Documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the install section**

Replace the body of `README.md` section `### 2. Build and install Workbridge` with:

````markdown
Workbridge is distributed as a Rust binary crate:

```sh
cargo install workbridge
```

For local development from a checkout, install the current workspace instead:

```sh
cargo install --path .
```

For local development without installing, `cargo run -- <args>` works the same
way as the installed `workbridge` binary.
````

- [ ] **Step 2: Check rendered Markdown structure**

Run:

```sh
sed -n '36,58p' README.md
```

Expected: the section shows `cargo install workbridge` first, then `cargo install --path .`, then the `cargo run -- <args>` note.

- [ ] **Step 3: Confirm README contains no obsolete install-only wording**

Run:

```sh
rg -n "Build a release binary|only supported install|cargo install --path" README.md
```

Expected: only the intended local-development `cargo install --path .` mention remains; no `Build a release binary` wording remains.

- [ ] **Step 4: Commit README update**

Run:

```sh
git add README.md
git commit -m "Document crates.io installation"
```

Expected: pre-commit hook passes and commit succeeds.

## Task 3: Initial Changelog

**Files:**
- Create: `CHANGELOG.md`

- [ ] **Step 1: Create the changelog**

Create `CHANGELOG.md` with exactly this content:

```markdown
# Changelog

All notable changes to Workbridge are documented in this file.

## 0.1.0

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
```

- [ ] **Step 2: Verify changelog text**

Run:

```sh
sed -n '1,80p' CHANGELOG.md
```

Expected: output matches the content from Step 1.

- [ ] **Step 3: Commit changelog**

Run:

```sh
git add CHANGELOG.md
git commit -m "Add initial changelog"
```

Expected: pre-commit hook passes and commit succeeds.

## Task 4: Manual Release Checklist

**Files:**
- Create: `docs/release-checklist.md`

- [ ] **Step 1: Create the release checklist**

Create `docs/release-checklist.md` with exactly this content:

````markdown
# Release Checklist

Use this checklist for manual Workbridge releases to crates.io.

## First Publish Name Check

- Confirm `workbridge` is still available immediately before the first publish:

  ```sh
  cargo search workbridge --limit 5
  ```

- If a crate with the exact name `workbridge` appears, stop. Choose a new crate
  name and audit `Cargo.toml`, README install commands, UI text, docs, and
  release notes before publishing.

## Preflight

- Confirm the intended release commit is checked out:

  ```sh
  git status --short
  git log --oneline -1
  ```

- Do not use `--no-verify` for release commits.
- Do not use `--allow-dirty` for Cargo package or publish commands.
- Confirm `CHANGELOG.md` has an entry for the version in `Cargo.toml`.

## Verification

Run the project gates:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test --all-features
```

Inspect the packaged crate contents:

```sh
cargo package --list
```

The package should include source, runtime prompts, assets, public docs, the
license, README, changelog, and Cargo files. It should not include agent-local
files, git hooks, scratch playgrounds, or internal planning specs.

Run Cargo's publish preflight:

```sh
cargo publish --dry-run
```

## Publish

Publish the crate manually:

```sh
cargo publish
```

## Tag

After crates.io accepts the package, tag the published commit:

```sh
git tag v0.1.0
git push origin v0.1.0
```

## Optional GitHub Release

Create a GitHub release for the tag if maintainers want release notes mirrored
outside crates.io. Use the matching `CHANGELOG.md` entry as the release-note
source.
````

- [ ] **Step 2: Verify checklist wording**

Run:

```sh
sed -n '1,180p' docs/release-checklist.md
```

Expected: checklist includes final crate-name check, no `--no-verify`, no `--allow-dirty`, package-list inspection, dry-run, publish, tag, and optional GitHub release.

- [ ] **Step 3: Commit release checklist**

Run:

```sh
git add docs/release-checklist.md
git commit -m "Add crates.io release checklist"
```

Expected: pre-commit hook passes and commit succeeds.

## Task 5: Full Verification

**Files:**
- Verify: `Cargo.toml`
- Verify: `README.md`
- Verify: `CHANGELOG.md`
- Verify: `docs/release-checklist.md`

- [ ] **Step 1: Run formatting check**

Run:

```sh
cargo fmt --check
```

Expected: command exits 0.

- [ ] **Step 2: Run clippy**

Run:

```sh
cargo clippy --all-targets --all-features
```

Expected: command exits 0 with no warnings. Do not add `#[allow(...)]`, suppress warnings, or relax linting.

- [ ] **Step 3: Run tests**

Run:

```sh
cargo test --all-features
```

Expected: command exits 0.

- [ ] **Step 4: Verify final package contents**

Run:

```sh
cargo package --list
```

Expected: output includes `CHANGELOG.md` and `docs/release-checklist.md`, and excludes `.claude/`, `docs/superpowers/`, `hooks/`, `CLAUDE.md`, `AGENTS.md`, `README`, and `workbridge-sidebar-playground.html`.

- [ ] **Step 5: Run publish dry-run**

Run:

```sh
cargo publish --dry-run
```

Expected: command exits 0 when registry network/authentication is available. If it fails because crates.io cannot be reached or authentication is unavailable in the execution environment, record the exact error and confirm `docs/release-checklist.md` tells maintainers to run the same command before the real publish.

- [ ] **Step 6: Review final diff**

Run:

```sh
git diff HEAD~4..HEAD -- Cargo.toml README.md CHANGELOG.md docs/release-checklist.md
git status --short
```

Expected: diff shows only the approved package metadata, install docs, changelog, and release checklist changes; status is clean.
