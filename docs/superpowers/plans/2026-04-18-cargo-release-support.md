# Cargo-release Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `cargo-release` support to Workbridge so a maintainer can cut a release with a single command that runs the same fmt/clippy/test gates as the git hooks, bumps `Cargo.toml`, rewrites `CHANGELOG.md`, tags the commit `v<version>`, publishes to crates.io, and pushes the commit and tag to origin.

**Architecture:** A single `release.toml` at the repo root encodes the whole release flow. A hand-maintained `CHANGELOG.md` in Keep a Changelog format accumulates per-PR bullets under `## [Unreleased]`, which a cargo-release replacement rule rewrites at release time. `docs/releasing.md` documents the human workflow; `README.md` and `CONTRIBUTING.md` point at it. No source code under `src/` is touched.

**Tech Stack:** Rust + Cargo, [`cargo-release`](https://github.com/crate-ci/cargo-release) (installed via `cargo install cargo-release`), Keep a Changelog 1.1.0 format.

**Spec:** [docs/superpowers/specs/2026-04-18-cargo-release-support-design.md](../specs/2026-04-18-cargo-release-support-design.md)

---

## File structure

| Path | State | Responsibility |
|------|-------|----------------|
| `release.toml` | new | cargo-release configuration; single source of truth for the release flow |
| `CHANGELOG.md` | new | Keep a Changelog format; permanent `## [Unreleased]` section at top |
| `docs/releasing.md` | new | Human-facing release workflow doc |
| `README.md` | edit | Add "Releasing" link under Further Reading |
| `CONTRIBUTING.md` | edit | Add `## Changelog` section telling contributors to add a bullet per PR |
| `docs/superpowers/specs/2026-04-18-cargo-release-support-design.md` | already on disk | Design spec, committed in Task 1 |
| `docs/superpowers/plans/2026-04-18-cargo-release-support.md` | this file | Plan, committed in Task 1 |

No `.rs` files are modified. No tests are added - this is build infrastructure. Validation uses `cargo release` itself in dry-run mode.

---

## Task 1: Commit the design spec and plan

**Files:**
- Add: `docs/superpowers/specs/2026-04-18-cargo-release-support-design.md` (already on disk)
- Add: `docs/superpowers/plans/2026-04-18-cargo-release-support.md` (this file, already on disk)

**Why first:** The spec and plan describe the work and should land before the implementation commits, so downstream commits can reference them.

- [ ] **Step 1: Verify both files exist on disk**

```sh
ls -la docs/superpowers/specs/2026-04-18-cargo-release-support-design.md
ls -la docs/superpowers/plans/2026-04-18-cargo-release-support.md
```

Expected: Both files exist and are non-empty.

- [ ] **Step 2: Stage and commit**

```sh
git add docs/superpowers/specs/2026-04-18-cargo-release-support-design.md
git add docs/superpowers/plans/2026-04-18-cargo-release-support.md
git commit -m "$(cat <<'EOF'
docs: add cargo-release design spec and implementation plan

Documents the decision to add cargo-release support (see spec for the
four approved design answers: crates.io publish, hand-maintained
CHANGELOG, full fmt+clippy+test pre-release hook, flat docs/ location
for the human-facing doc).
EOF
)"
```

Expected: Pre-commit hook runs `cargo fmt -- --check` and `cargo clippy --all-targets --all-features -- -D warnings` and passes (markdown-only change does not affect Rust code). Commit succeeds.

---

## Task 2: Add `release.toml` and `CHANGELOG.md`

**Files:**
- Create: `release.toml`
- Create: `CHANGELOG.md`

**Why grouped:** `release.toml` references `CHANGELOG.md`'s `## [Unreleased]` heading via `pre-release-replacements`. If `release.toml` ships without the matching changelog, any `cargo release` invocation fails immediately at the replacement step. They MUST land together.

- [ ] **Step 1: Write `release.toml`**

Create `release.toml` at the repo root with this exact content:

```toml
# cargo-release configuration. See docs/releasing.md for the human workflow.
# Reference: https://github.com/crate-ci/cargo-release/blob/master/docs/reference.md

allow-branch = ["master"]
tag-name = "v{{version}}"
sign-commit = false
sign-tag = false
consolidate-commits = true
publish = true
push = true

pre-release-hook = [
    "bash",
    "-c",
    "cargo fmt -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features",
]

[[pre-release-replacements]]
file    = "CHANGELOG.md"
search  = "## \\[Unreleased\\]"
replace = "## [Unreleased]\n\n## [{{version}}] - {{date}}"
min     = 1
max     = 1
```

Key points that matter for correctness and must not drift:
- `allow-branch = ["master"]` - releases only from master.
- `sign-commit = false` / `sign-tag = false` - do not force `-S`; respect the user's global git config.
- The `pre-release-hook` command string must match the gates in `hooks/pre-commit` and `hooks/pre-push` exactly: `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`. `--all-features` is load-bearing because the `integration` feature (see `Cargo.toml` line 34) gates integration tests that would otherwise be skipped.
- `min = 1, max = 1` on the replacement is load-bearing: if the `## [Unreleased]` marker is missing, cargo-release aborts before tagging or publishing instead of silently doing nothing.

- [ ] **Step 2: Write `CHANGELOG.md`**

Create `CHANGELOG.md` at the repo root with this exact content:

```markdown
# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Added `cargo-release` support and release documentation. See
  [docs/releasing.md](docs/releasing.md).
```

No historical entries are backfilled. The literal `## [Unreleased]` heading must be present exactly as shown - it is what the `pre-release-replacements` regex matches against.

- [ ] **Step 3: Install cargo-release locally (if not already installed)**

```sh
cargo install cargo-release --version "^0.25"
```

Expected: Installs successfully, or reports "binary `cargo-release` already exists" if present.

- [ ] **Step 4: Dry-run `cargo release patch` to validate `release.toml` parses and the whole flow is understood**

```sh
cargo release patch
```

Note: NO `--execute` flag. This is a dry run; nothing is written or pushed.

Expected output includes all of:
- `Upgrading workbridge from 0.1.0 to 0.1.1`
- A block showing the `CHANGELOG.md` replacement from `## [Unreleased]` to `## [Unreleased]\n\n## [0.1.1] - <today's date>`
- The commit message `chore: release 0.1.1` (or similar; cargo-release picks the subject)
- The tag name `v0.1.1`
- A `Publishing workbridge` line
- A `Pushing` line mentioning `master` and `v0.1.1`
- The pre-release-hook command echoed (but NOT executed in dry-run).

If cargo-release complains that `## [Unreleased]` is not found: check that `CHANGELOG.md` is saved with the exact heading, including square brackets, and not `## Unreleased` or `## [UNRELEASED]`.

If cargo-release complains that the tree is dirty: stage or stash your untracked files first.

- [ ] **Step 5: Stage and commit**

```sh
git add release.toml CHANGELOG.md
git commit -m "$(cat <<'EOF'
feat: add cargo-release configuration and changelog

release.toml drives the full release pipeline (fmt + clippy + test gate,
version bump, changelog rewrite, commit, tag, publish, push). CHANGELOG.md
is hand-maintained in Keep a Changelog format; contributors add bullets
under ## [Unreleased] per PR.

See docs/superpowers/specs/2026-04-18-cargo-release-support-design.md
for the full design rationale.
EOF
)"
```

Expected: Pre-commit hook passes (no `.rs` changes). Commit succeeds.

---

## Task 3: Add `docs/releasing.md`

**Files:**
- Create: `docs/releasing.md`

- [ ] **Step 1: Write `docs/releasing.md`**

Create `docs/releasing.md` with this exact content:

```markdown
# Releasing

How to cut a new release of Workbridge.

Workbridge uses [`cargo-release`](https://github.com/crate-ci/cargo-release)
to bump the version, update the changelog, tag the commit, publish to
crates.io, and push the commit and tag to origin in one command. The
release configuration lives in [`release.toml`](../release.toml) at the
repo root; this document covers the human side.

## Prerequisites

1. Install `cargo-release` once:

   ```sh
   cargo install cargo-release
   ```

2. Check out `master` and make sure the tree is clean:

   ```sh
   git checkout master
   git pull origin master
   git status
   ```

   `git status` must report `nothing to commit, working tree clean` with
   no untracked files. `cargo release` will refuse to run against a dirty
   tree and against any branch other than `master`.

3. Make sure you have a crates.io API token registered locally:

   ```sh
   cargo login <your-token>
   ```

   The token lives in `~/.cargo/credentials.toml` after this command. Get
   a token from <https://crates.io/me> if you do not have one.

## Cutting a release

Always dry-run first:

```sh
cargo release patch      # or minor, major, or an explicit x.y.z
```

Without `--execute`, nothing is written or pushed. Eyeball the output; it
lists every step the real run would take, including the version bump, the
changelog rewrite, the commit message, the tag name, the `cargo publish`
command, and the git push destinations.

Once the dry-run looks right, run the real release:

```sh
cargo release patch --execute
```

`<level>` is `patch`, `minor`, `major`, or an explicit `<x.y.z>`. See the
[cargo-release reference](https://github.com/crate-ci/cargo-release/blob/master/docs/reference.md)
for the full flag surface.

## What happens under the hood

`cargo release <level> --execute` runs these steps in order:

1. **Check** - confirms you are on `master`, the tree is clean, and the
   `allow-branch` rule in `release.toml` is satisfied.
2. **Pre-release hook** - runs
   `cargo fmt -- --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --all-features`.
   This is the same set of gates the git hooks enforce on commit and
   push, re-run here against the version-bumped manifest.
3. **Bump** - rewrites the `version` field in `Cargo.toml`.
4. **Replace** - rewrites `## [Unreleased]` in `CHANGELOG.md` to a dated
   version heading, leaving a fresh empty `## [Unreleased]` at the top.
5. **Commit** - creates one `chore: release <version>` commit containing
   both the `Cargo.toml` bump and the `CHANGELOG.md` rewrite.
6. **Tag** - creates a `v<version>` git tag pointing at that commit.
7. **Publish** - runs `cargo publish` to push the crate to crates.io.
8. **Push** - pushes the release commit and the tag to `origin`.

Everything up to step 5 is reversible without side effects. Once step 7
succeeds, crates.io has an artifact that can only be yanked (hidden from
new resolution), not deleted. This is why you dry-run first.

## Recovering from failures

### Pre-release hook failure (step 2)

Nothing has changed on disk. Fix the underlying issue (lint failure,
formatting issue, test failure), commit the fix to `master`, and re-run
`cargo release`.

### `## [Unreleased]` marker missing (step 4)

`cargo release` will abort with a `min = 1` count not satisfied. At this
point `Cargo.toml` has already been bumped on disk but not committed. To
recover:

```sh
git checkout -- Cargo.toml
# restore the `## [Unreleased]` heading in CHANGELOG.md
cargo release <level>      # dry-run first to confirm the fix
```

### `cargo publish` fails (step 7)

This is the most common nasty case: the commit and tag exist locally but
crates.io rejected the publish (missing token, network error, a version
that has already been published, a manifest that fails crates.io's
validation). origin has not been pushed to yet.

Recovery:

```sh
git tag -d v<version>
git reset --hard HEAD~1
# fix the underlying cause (cargo login, update manifest, etc.)
cargo release <level>      # dry-run first
```

### `cargo publish` succeeds but `git push` fails (step 8)

crates.io has the new version. Local commit and tag exist. origin does
not. Do NOT re-run `cargo release` - the release is complete from
crates.io's point of view; re-running would try to publish the same
version again and crates.io would reject it.

Recovery:

```sh
git push origin master
git push origin v<version>
```

If the push fails because someone else pushed to `master` concurrently,
`git pull --rebase origin master` first, then push. The release tag
should still point at your release commit after the rebase.

### Missing or expired crates.io token

`cargo publish` will fail with an authentication error. Run `cargo login
<new-token>` and recover the same way as "`cargo publish` fails" above.
The token persists across releases in `~/.cargo/credentials.toml`.

## First release validation

The first real release after this feature ships should be the smallest
possible bump so the whole chain can be validated against crates.io
without committing to a visible version jump:

```sh
cargo release patch --execute
```

This takes `0.1.0 -> 0.1.1` and exercises the full pipeline end to end.
If anything goes wrong, `0.1.1` is trivially yankable and the next
attempt cuts `0.1.2`.

Before cutting the first release, confirm the crate name `workbridge` is
not already claimed on crates.io by another project:

```sh
cargo search workbridge
```

If a different project owns the name, a rename (and corresponding
`Cargo.toml` `name =` update) is required before the first publish can
succeed.
```

- [ ] **Step 2: Verify the file has all five required sections**

```sh
grep -c '^## ' docs/releasing.md
```

Expected: `5` (Prerequisites, Cutting a release, What happens under the hood, Recovering from failures, First release validation).

```sh
grep -n '^### ' docs/releasing.md
```

Expected: Four subsection headings, all under "Recovering from failures":
- `### Pre-release hook failure (step 2)`
- `### \`## [Unreleased]\` marker missing (step 4)`
- `### \`cargo publish\` fails (step 7)`
- `### \`cargo publish\` succeeds but \`git push\` fails (step 8)`

(The fifth subsection `### Missing or expired crates.io token` is also expected; total is 5 `###` headings.)

- [ ] **Step 3: Stage and commit**

```sh
git add docs/releasing.md
git commit -m "$(cat <<'EOF'
docs: add releasing.md

Human-facing release workflow: prerequisites, dry-run-first command,
under-the-hood step list, five recovery recipes for the known failure
modes, and a first-release validation recommendation.
EOF
)"
```

Expected: Pre-commit hook passes. Commit succeeds.

---

## Task 4: Wire up contributor-facing docs

**Files:**
- Modify: `README.md`
- Modify: `CONTRIBUTING.md`

- [ ] **Step 1: Add `docs/releasing.md` link to `README.md` Further Reading**

Locate this block in `README.md` (around line 124-130):

```markdown
## Further Reading

- [CONTRIBUTING.md](CONTRIBUTING.md) - coding standards, error handling, UI rules
- [docs/repository-registry.md](docs/repository-registry.md) - repo registration and config
- [docs/work-items.md](docs/work-items.md) - work item lifecycle and stages
- [docs/UI.md](docs/UI.md) - TUI layout and interactions
- [docs/invariants.md](docs/invariants.md) - project invariants (read-only)
```

Insert the `docs/releasing.md` line between `docs/UI.md` and `docs/invariants.md`:

```markdown
## Further Reading

- [CONTRIBUTING.md](CONTRIBUTING.md) - coding standards, error handling, UI rules
- [docs/repository-registry.md](docs/repository-registry.md) - repo registration and config
- [docs/work-items.md](docs/work-items.md) - work item lifecycle and stages
- [docs/UI.md](docs/UI.md) - TUI layout and interactions
- [docs/releasing.md](docs/releasing.md) - cutting a new release
- [docs/invariants.md](docs/invariants.md) - project invariants (read-only)
```

- [ ] **Step 2: Add `## Changelog` section to `CONTRIBUTING.md`**

Locate the current end of the `## Error Handling` section in `CONTRIBUTING.md` (around line 27) and the start of `## UI and Color` (around line 29).

Insert a new `## Changelog` section between them:

```markdown
## Changelog

Workbridge keeps a `CHANGELOG.md` in [Keep a Changelog](https://keepachangelog.com)
format. When you open a PR, add a bullet under `## [Unreleased]`
describing the user-visible change in one line. Tag the bullet with the
PR number:

    - Fix worktree cleanup when the upstream branch is gone ([#123](https://github.com/jkirsteins/workbridge/pull/123))

Internal-only changes (refactors, test-only work, doc-only tweaks) do
not need an entry.

Never rename or remove the `## [Unreleased]` heading - the release
tooling relies on it matching the literal string `## [Unreleased]` so
the pre-release replacement can fire. Renaming it causes `cargo release`
to abort with `min = 1` not met.

```

The exact insertion point: after the line that ends the `## Error Handling` section (the sentence ending in "`log or display the error first.`") and before the line `## UI and Color`. Make sure there is exactly one blank line between the end of the Changelog section and the `## UI and Color` heading.

- [ ] **Step 3: Verify both edits**

```sh
grep -n 'docs/releasing.md' README.md
```

Expected: One match, on a line that reads `- [docs/releasing.md](docs/releasing.md) - cutting a new release`.

```sh
grep -n '^## Changelog' CONTRIBUTING.md
```

Expected: One match, with `## Changelog` appearing after `## Error Handling` and before `## UI and Color`.

```sh
grep -n '^## ' CONTRIBUTING.md
```

Expected order (line numbers will differ):
```
## Getting Started
## Linting and Formatting
## Error Handling
## Changelog
## UI and Color
```

- [ ] **Step 4: Stage and commit**

```sh
git add README.md CONTRIBUTING.md
git commit -m "$(cat <<'EOF'
docs: link releasing.md and document changelog convention

README Further Reading now points at the new docs/releasing.md.
CONTRIBUTING gains a Changelog section telling contributors to add a
bullet under ## [Unreleased] per PR, with an explicit warning that the
## [Unreleased] heading must not be renamed (release tooling depends on
it).
EOF
)"
```

Expected: Pre-commit hook passes. Commit succeeds.

---

## Task 5: End-to-end dry-run validation

No files are modified in this task. The goal is to confirm the whole chain works end-to-end in dry-run mode on the feature branch before the PR is opened.

- [ ] **Step 1: Ensure cargo-release is installed**

```sh
cargo release --version
```

Expected: Prints a version number. If `cargo release` is not found, install it: `cargo install cargo-release --version "^0.25"`.

- [ ] **Step 2: Confirm working tree is clean**

```sh
git status
```

Expected: `nothing to commit, working tree clean` with no untracked files. All four previous tasks must have been committed.

- [ ] **Step 3: Run `cargo release patch` as a dry-run from the feature branch**

Note: `allow-branch = ["master"]` in `release.toml` will reject this on the feature branch, which is the expected outcome and proves the allow-list is wired correctly. Run it anyway to see the error:

```sh
cargo release patch
```

Expected: `cargo release` aborts with a message indicating the current branch is not in `allow-branch`. If it does NOT abort and proceeds with the dry run, the `allow-branch` key is misconfigured - go back to Task 2 Step 1 and verify the key.

- [ ] **Step 4: Temporarily override the branch check to see the full dry-run**

`--allow-branch` takes one or more exact branch names. Pass the current
branch explicitly so the override does not depend on glob support:

```sh
cargo release patch --allow-branch "$(git symbolic-ref --short HEAD)"
```

Expected output includes all of:
- `Upgrading workbridge from 0.1.0 to 0.1.1`
- A replacement block for `CHANGELOG.md` showing `## [Unreleased]` becoming `## [Unreleased]\n\n## [0.1.1] - <today's date>`
- A commit message (`chore: release 0.1.1` or similar)
- A tag name line mentioning `v0.1.1`
- A `Publishing workbridge` line
- A `Pushing` or `git push` line referencing `master` and `v0.1.1`
- The pre-release-hook command displayed (but NOT executed in dry-run)

If any of the above is missing, the corresponding `release.toml` key is wrong. Cross-reference Task 2 Step 1.

- [ ] **Step 5: Confirm no files were modified by the dry-run**

```sh
git status
```

Expected: Still `nothing to commit, working tree clean`. `cargo release` without `--execute` must not write to disk.

If the tree is dirty, something in `release.toml` is misconfigured (likely `[[pre-release-replacements]]` running outside of a `--execute` context, which is a cargo-release bug to report separately). Reset: `git checkout -- .`.

- [ ] **Step 6: No commit required**

This task produces no file changes. Proceed to opening the PR.

---

## Self-review checklist (run after implementing all tasks)

After Task 5 completes, check:

1. **Spec coverage:** Every section of `docs/superpowers/specs/2026-04-18-cargo-release-support-design.md` should have a corresponding task:
   - `release.toml` spec -> Task 2 Step 1
   - `CHANGELOG.md` seed -> Task 2 Step 2
   - `docs/releasing.md` five sections -> Task 3 Step 1
   - `README.md` edit -> Task 4 Step 1
   - `CONTRIBUTING.md` edit -> Task 4 Step 2
   - Validation plan -> Task 5
   - Non-goals (NG1-NG4) -> NOT implemented by design; each has a technical fix description in the spec for future work.

2. **Command-string consistency:** The pre-release-hook command in `release.toml` (Task 2 Step 1), the "What happens under the hood" list in `docs/releasing.md` (Task 3 Step 1), and the "Pre-release hook" step in the spec must all state the exact same three commands in the exact same order: `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`.

3. **Heading literal consistency:** `## [Unreleased]` must appear identically in:
   - The `CHANGELOG.md` seed (Task 2 Step 2)
   - The `pre-release-replacements` regex pattern in `release.toml` (Task 2 Step 1, as `## \\[Unreleased\\]` - the double-backslash is TOML escaping for a single backslash regex escape)
   - The `CONTRIBUTING.md` Changelog section (Task 4 Step 2)
   - The recovery recipe in `docs/releasing.md` (Task 3 Step 1)

4. **No source files touched:** `git diff master -- 'src/*.rs' 'tests/*.rs' 'Cargo.toml' 'Cargo.lock'` should show zero diff. If `Cargo.toml` or `Cargo.lock` are modified, something ran `cargo release --execute` by accident.

---

## Out of scope (deferred per spec)

None of these are implemented by this plan. See spec NG1-NG4 for the technical fix descriptions.

- Windows binary distribution via GitHub Releases (NG1).
- Cross-target `cargo publish` verification (NG2).
- GitHub Actions-driven releases (NG3).
- Auto-generated changelog via git-cliff or similar (NG4).
