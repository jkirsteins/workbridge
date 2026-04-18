# Design: cargo-release support for Workbridge

Date: 2026-04-18
Status: Approved (pending implementation)

## Goal

Add `cargo-release` support to Workbridge and document the release workflow
so that cutting a new version is a single, repeatable command that:

1. Runs the same fmt/clippy/test gates that the git hooks run.
2. Bumps `Cargo.toml` to the target version.
3. Rewrites `CHANGELOG.md` to move accumulated `## [Unreleased]` entries under
   a dated version heading.
4. Creates a `v<version>` git tag.
5. Publishes the crate to crates.io.
6. Pushes the commit and tag to `origin`.

Everything is driven by a single `release.toml` committed to the repo, so any
contributor with a crates.io token can cut a release from a clean `master`
checkout without memorising flags.

## Success criteria

- `cargo install cargo-release` followed by `cargo release patch --execute`
  on a clean `master` checkout takes `0.1.0` -> `0.1.1`, commits, tags,
  publishes to crates.io, and pushes the commit and tag to origin, with
  `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test`
  having run and passed as a pre-release gate.
- A contributor who reads `docs/releasing.md` end-to-end can cut a release
  without reading `release.toml` or cargo-release's upstream docs.
- A contributor who reads the new "Changelog" section in `CONTRIBUTING.md`
  knows where to add a bullet when opening a PR and why the
  `## [Unreleased]` heading must not be renamed.

## Surfaces

Five files; three are new.

| Path                | State | Role |
|---------------------|-------|------|
| `release.toml`      | new   | cargo-release configuration; single source of truth for the release flow. |
| `CHANGELOG.md`      | new   | Keep a Changelog format. Permanent `## [Unreleased]` section at the top. |
| `docs/releasing.md` | new   | Human-facing doc: prerequisites, command, failure recovery, dry-run workflow. |
| `README.md`         | edit  | Add a "Releasing" link under Further Reading pointing at `docs/releasing.md`. |
| `CONTRIBUTING.md`   | edit  | Add a short "Changelog" section instructing contributors to add a bullet under `## [Unreleased]` in their PR. |

No source files under `src/` are touched. No tests are added; this is
build-infrastructure.

## `release.toml` spec

All keys below are final. Implementation must match exactly.

| Key                      | Value | Rationale |
|--------------------------|-------|-----------|
| `allow-branch`           | `["master"]` | Refuse to release from feature branches. |
| `tag-name`               | `"v{{version}}"` | cargo-release default; matches common Rust convention. |
| `sign-commit`            | `false` | Delegate to the user's global git config; do not force `-S`. |
| `sign-tag`               | `false` | Same rationale as `sign-commit`. |
| `consolidate-commits`    | `true` | One commit per release (`chore: release {{version}}`). |
| `publish`                | `true` | Push to crates.io as part of the release. |
| `push`                   | `true` | Push the release commit and the tag to `origin` after publish. |
| `pre-release-hook`       | `["bash", "-c", "cargo fmt -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features"]` | Matches the exact commands in `hooks/pre-commit` + `hooks/pre-push`, re-run at release time against the version-bumped tree. `--all-features` is load-bearing: without it, the `integration` feature's tests are skipped. `--all-targets` lints test/bench/example code too. |
| `pre-release-replacements` | See below | Rewrite `## [Unreleased]` in `CHANGELOG.md`. |

`pre-release-replacements` entry:

```toml
[[pre-release-replacements]]
file     = "CHANGELOG.md"
search   = "## \\[Unreleased\\]"
replace  = "## [Unreleased]\n\n## [{{version}}] - {{date}}"
min      = 1
max      = 1
```

`min = 1, max = 1` is load-bearing: if the `## [Unreleased]` marker has been
renamed, removed, or already rewritten, the release aborts before anything is
tagged or published, and the user is told exactly what is wrong.

## `CHANGELOG.md` seed contents

```markdown
# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Added `cargo-release` support and release documentation. See
  [docs/releasing.md](docs/releasing.md).
```

No historical entries are backfilled. The `## [Unreleased]` heading must be
present and must literally match the regex used by the replacement rule.

## `docs/releasing.md` structure

The doc must cover, in this order, with no sections omitted:

1. **Prerequisites.**
   - `cargo install cargo-release` (one-time).
   - Clean working tree per `git status` (no staged, unstaged, or untracked
     files).
   - On `master`, with `master` up to date with `origin/master`.
   - A crates.io token registered via `cargo login <token>` (stored in
     `~/.cargo/credentials.toml`).

2. **The command.**
   - Dry run: `cargo release <level>` (no `--execute`; nothing is written or
     pushed).
   - Real run: `cargo release <level> --execute`.
   - `<level>` is `patch`, `minor`, `major`, or an explicit `<x.y.z>`. For
     the full flag surface, link out to cargo-release's upstream docs rather
     than re-documenting every flag.

3. **What happens under the hood.** The Data Flow diagram from this design,
   rendered as an ordered list with one line per step.

4. **Failure recovery.** Four named scenarios with exact recovery commands
   (see Failure-mode summary below).

5. **First-release validation.** Recommend cutting `0.1.0 -> 0.1.1` as the
   first release so the whole chain is exercised on the least-consequential
   bump. One-time recommendation, not a policy.

## Data flow

```
cargo release <level> --execute

  check        : on branch master, tree clean, allow-branch passes
  hook         : cargo fmt -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features
  bump         : Cargo.toml version
  replace      : CHANGELOG.md "## [Unreleased]" -> dated heading
  commit       : "chore: release {version}"
  tag          : "v{version}"
  publish      : cargo publish -> crates.io
  push         : origin master && origin v{version}
```

Everything up to `commit` is reversible without side effects. Once `publish`
succeeds, crates.io has an artifact that can only be yanked, not deleted,
so the user MUST have dry-run successfully first.

## README.md edit

Under the existing "Further Reading" list in `README.md`, add one line:

```
- [docs/releasing.md](docs/releasing.md) - cutting a new release
```

Placement: immediately after the `docs/UI.md` entry, before the
`docs/invariants.md` entry.

## CONTRIBUTING.md edit

Add a new `## Changelog` section after the existing `## Error Handling`
section (before `## UI and Color`). Content:

```
## Changelog

Workbridge keeps a `CHANGELOG.md` in [Keep a Changelog](https://keepachangelog.com)
format. When you open a PR, add a bullet under `## [Unreleased]` describing
the user-visible change in one line. Tag the bullet with the PR number:

    - Fix worktree cleanup when the upstream branch is gone ([#123](https://github.com/jkirsteins/workbridge/pull/123))

Internal-only changes (refactors, test-only work, doc-only tweaks) do not
need an entry.

Never rename or remove the `## [Unreleased]` heading - the release tooling
relies on it matching the literal string `## [Unreleased]` so the
pre-release replacement can fire. Renaming it causes `cargo release` to
abort with `min = 1` not met.
```

## Failure-mode summary

| Failure                               | When it fires | Effect | Recovery |
|---------------------------------------|---------------|--------|----------|
| Not on `master`                       | Before anything else | Release refuses to start | Switch branch. |
| Dirty tree                            | Before hook | Release refuses to start | Commit or stash. |
| `cargo fmt --check` / clippy / test  | Pre-release hook | Abort; no file changes | Fix, re-run. |
| `## [Unreleased]` missing             | Replacements step | Abort after manifest bump on disk | `git checkout -- Cargo.toml`, restore heading, re-run. |
| `cargo publish` fails (pre-push)      | After commit + tag, before push | Local commit + local tag exist; crates.io + origin untouched | `git tag -d v<version>`, `git reset --hard HEAD~1`, fix cause, re-run. |
| `cargo publish` succeeds, push fails  | After publish | crates.io has the version; local commit + tag exist; origin has neither | Resolve the push failure (auth, concurrent push); `git push origin master && git push origin v<version>`. Do NOT re-run `cargo release` - the release is done from crates.io's perspective. |
| Missing crates.io token               | During publish | Abort after commit + tag | `cargo login <token>`, then recover as "publish fails (pre-push)" above. |

## Non-goals (and what fixing each would require)

Each non-goal below names the out-of-scope surface, the concrete technical
work required to bring it in scope, and the reason deferring is acceptable
for this design.

### NG1: Windows binary distribution

**What is out of scope:** Pre-built `.exe` artifacts for Windows users who do
not have a Rust toolchain. This spec publishes source to crates.io only.

**Why deferral is acceptable:** crates.io publish is platform-independent -
`cargo publish` uploads source, not compiled artifacts. Windows users with a
Rust toolchain can `cargo install workbridge` and compile locally, which is
the realistic audience today. End-user Windows distribution is a separate
feature with a separate delivery channel (GitHub Releases), not a
release-tooling concern.

**What adding it would require technically:**

1. A GitHub Actions workflow at `.github/workflows/release.yml` triggered on
   `push: tags: v*`, with a matrix of `{ ubuntu-latest, macos-latest,
   windows-latest }` runners.
2. On each runner: `cargo build --release`, strip/codesign as appropriate,
   tar/zip the binary.
3. A final job that downloads all three artifacts and calls `gh release
   create v<version> --generate-notes <artifact-paths...>` using a
   `GITHUB_TOKEN` with `contents: write`.
4. No changes to `release.toml` are needed - the workflow triggers off the
   tag that `cargo release` already pushes.
5. Before adding, audit `src/` for Windows-incompatible code. Known
   candidates: the PTY spawn path (uses Unix primitives), any direct
   `signal-hook` usage on SIGWINCH / SIGCHLD, any hard-coded `/` path
   separators. Each non-portable site needs either a `#[cfg(unix)]` /
   `#[cfg(windows)]` split or a portable replacement. This audit is the
   actual load-bearing work, not the workflow YAML.

**Current workaround for Windows users:** Install a Rust toolchain via
rustup, then `cargo install --git https://github.com/jkirsteins/workbridge`
or `cargo install workbridge` once this spec ships. Expect compile errors
if the Windows portability audit has not been done.

### NG2: `cargo publish` verification against non-host targets

**What is out of scope:** `cargo publish` from macOS only verifies that the
crate builds on macOS. A Windows-incompatible change can be uploaded to
crates.io and the failure only surfaces when a Windows user runs
`cargo install workbridge`.

**Why deferral is acceptable:** Without NG1 (Windows binary distribution),
the Windows install path is already an opt-in power-user workflow.
Contributors on macOS have no local signal that they broke Windows. This
spec adds no Windows checks; it does not make Windows worse than it is
today (there is no Windows CI today either).

**What adding it would require technically:**

1. In the GitHub Actions workflow above (NG1), add a `cargo check --release`
   job on `windows-latest` that runs on pull requests, not just tags. This
   catches Windows-incompatible code at PR time, before it can reach a
   release.
2. Alternatively (cheaper but slower): add `cargo check --target
   x86_64-pc-windows-gnu` to the pre-release hook, with a macOS prerequisite
   of `rustup target add x86_64-pc-windows-gnu` and `brew install mingw-w64`.
   This runs on every release, costs a linker dependency on macOS, and does
   not catch cross-platform runtime issues - only compile errors. Less
   valuable than the CI job.
3. Neither addition changes `release.toml` beyond the pre-release hook line.

**Current workaround:** None from this tooling. Windows users who hit build
errors file bugs; maintainers fix the code and yank the broken version on
crates.io.

### NG3: GitHub Actions CI for the release itself

**What is out of scope:** An Actions workflow that reacts to a manual
`workflow_dispatch` and runs `cargo release` in the cloud so the maintainer
does not need a local crates.io token or toolchain.

**Why deferral is acceptable:** There is exactly one maintainer today and
they already have a local Rust toolchain. The marginal value of moving
releases to CI is low; the marginal cost is non-trivial (managing a
crates.io token as a GitHub secret, managing push permissions from the
Actions runner, adding an approval flow so releases cannot be cut by
anyone with write access to master).

**What adding it would require technically:**

1. A `.github/workflows/release.yml` with a `workflow_dispatch` trigger
   that takes a `level` input (`patch`/`minor`/`major`).
2. A `CARGO_REGISTRY_TOKEN` repository secret stored from
   `cargo login`'s token output. Secret rotation policy defined
   (current cargo-release reads the env var directly).
3. A deploy key or GitHub App token with `contents: write` so the workflow
   can push the release commit and tag back to `master`. A plain
   `GITHUB_TOKEN` does NOT trigger downstream workflows on the pushed tag,
   so if NG1 (tag-triggered binary workflow) is in scope, we need a PAT or
   App token instead.
4. An approval requirement on the `release` environment so that only
   designated maintainers can cut a release, not anyone with merge rights.
5. `release.toml` remains the source of truth; no changes required, but the
   workflow runs `cargo release <level> --execute --no-verify` if we want to
   skip the local hook (since CI has already run fmt/clippy/test on the
   commit).

**Current workaround:** Cut releases locally. This is what `docs/releasing.md`
documents.

### NG4: Auto-generated changelog (conventional commits / git-cliff)

**What is out of scope:** Tooling that rebuilds `CHANGELOG.md` from the git
log at release time using conventional-commit prefixes
(`feat:` / `fix:` / `chore:`). Humans write the changelog by hand.

**Why deferral is acceptable:** The project's existing commit history uses
descriptive PR titles ("Add Quick Start section to README (#114)"), not
conventional-commit prefixes. Adopting auto-generation now would either
retrofit every historical commit or start the changelog from scratch and
lose the retrofit. The manual-entry habit is cheap (one bullet per PR) and
yields a higher-signal changelog than any auto-generator can from this
history style.

**What adding it would require technically:**

1. Adopt conventional-commit prefixes as a repo policy, enforced either via
   a `commit-msg` git hook (added to `hooks/`) or via a `commitlint`-style
   PR check. This is a social change, not a tooling change; both mechanisms
   have false-positive risk against legitimate non-conventional commits
   (revert, merge commits, bot commits).
2. Add `git-cliff` (or `cocogitto`) as a dev-tool dependency. Commit a
   `cliff.toml` describing section grouping (`feat` -> Added, `fix` ->
   Fixed, etc.).
3. Replace the `pre-release-replacements` rule in `release.toml` with a
   `pre-release-hook` step that runs `git cliff -o CHANGELOG.md --tag
   v{{version}}` before the commit step, then commits the updated file
   alongside the manifest bump.
4. Delete the per-PR "add a bullet to `## [Unreleased]`" rule from
   `CONTRIBUTING.md` since entries are now derived from commit messages.
5. Decide what to do about the historical `## [Unreleased]` bullet the
   first release cuts - either discard it or preserve it as an
   "Unreleased (pre-automation)" section.

**Current workaround:** Contributors write bullets by hand per
`CONTRIBUTING.md`.

## Open questions

None. All design-level decisions were locked during brainstorming:

- Publish target: tag + crates.io publish (user answer to Q1).
- Changelog: human-maintained, Keep a Changelog format, cargo-release rewrite
  of `## [Unreleased]` (user answer to Q2).
- Pre-release verification: fmt --check + clippy -D warnings + test
  (user answer to Q3).
- Tag format: `v{version}` (cargo-release default; user accepted by
  approving the design).
- Human-facing doc location: `docs/releasing.md` following the flat `docs/`
  convention (user accepted by approving the design).
- First release: recommendation only, `0.1.0 -> 0.1.1`; not a policy.

## Implementation checklist (for the writing-plans handoff)

1. Create `release.toml` with the keys and replacement rule exactly as
   specified above.
2. Create `CHANGELOG.md` with the seed contents exactly as specified above.
3. Create `docs/releasing.md` with the five mandatory sections in order.
4. Edit `README.md` Further Reading list to add the `docs/releasing.md`
   link in the specified position.
5. Edit `CONTRIBUTING.md` to insert the `## Changelog` section between
   `## Error Handling` and `## UI and Color`.
6. Run `cargo release patch` (dry-run, no `--execute`) as a validation
   step; eyeball the output. Do NOT run `--execute` as part of this PR;
   the first real release is cut post-merge.
7. Confirm that `cargo fmt --check`, `cargo clippy -- -D warnings`, and
   `cargo test` still pass (no source changes were made, but the hook
   invocation in `release.toml` is a new surface that encodes these
   commands).
