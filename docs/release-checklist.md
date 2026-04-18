# Release Checklist

Use this checklist for manual Workbridge releases to crates.io.

Workbridge uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html) for
`Cargo.toml` `version` and git tag names, and the `CHANGELOG.md` follows the
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format. Each release
is represented in git three ways:

- a bumped `version` field in `Cargo.toml`,
- a dated section in `CHANGELOG.md`, and
- an annotated git tag named `v<MAJOR>.<MINOR>.<PATCH>` that matches the
  `Cargo.toml` version exactly.

All three must agree before publishing.

## First Publish Name Check

The authoritative signal for "is this crate name taken?" is the crates.io
registry itself, not any local tooling. Use the URL check as the primary gate
and `cargo info` only as a cross-check that is careful to hit the registry
rather than the local workspace.

- Primary check: confirm that `https://crates.io/crates/workbridge` returns a
  404 in a browser. Any other response (200, redirect, etc.) means the name is
  already registered.

- Optional cross-check with `cargo info` (stable since Rust 1.79). This command
  must be run from a directory OUTSIDE the workbridge workspace, because from
  inside the workspace it resolves the local package and exits 0 even when the
  name is unpublished. From a scratch directory it hits the registry and
  returns the correct signal:

  ```sh
  cd /tmp && cargo info workbridge
  ```

  Exit 101 with a "could not find `workbridge` in registry" message means the
  name is available. Exit 0 with registry metadata (authors, versions,
  downloads) means the name is already taken.

- If the crates.io URL resolves to anything other than 404 (or the
  from-/tmp `cargo info` invocation exits 0 with registry metadata), stop.
  Choose a new crate name and audit `Cargo.toml`, README install commands, UI
  text, docs, and release notes before publishing.

## Version Bump And Changelog

Do this in a dedicated release-prep commit before running any publish commands.

- Bump `package.version` in `Cargo.toml` to the intended release version,
  following [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
  (MAJOR for breaking changes, MINOR for new features, PATCH for fixes).
- Bump `package.rust-version` in `Cargo.toml` if any new code or dependency in
  this release requires a newer stable toolchain than the currently declared
  MSRV. Leave it alone otherwise.
- In `CHANGELOG.md`, rename the `[Unreleased]` section to
  `[<version>] - YYYY-MM-DD` using today's date, and move any entries that
  belong to this release into it. Leave a fresh, empty `[Unreleased]` section
  at the top for future work.
- Update the reference-style links at the bottom of `CHANGELOG.md` so
  `[Unreleased]` compares against the new tag and `[<version>]` points at the
  new release tag.
- For the very first publish there are no prior entries to move; the existing
  `[0.1.0]` section and its date are already correct, but confirm the date
  still matches the actual publish day.
- Commit the version bump and changelog update together. Example:

  ```sh
  git add Cargo.toml CHANGELOG.md
  git commit -m "Release v<version>"
  ```

## Preflight

- Confirm the intended release commit is checked out:

  ```sh
  git status --short
  git log --oneline -1
  ```

- Confirm `Cargo.toml` `version`, the top dated section in `CHANGELOG.md`, and
  the tag name you plan to push all match.
- Do not use `--no-verify` for release commits.
- Do not use `--allow-dirty` for Cargo package or publish commands.

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

After crates.io accepts the package, create an annotated tag on the published
commit and push it. Annotated tags (`-a`) carry their own message, author, and
date in git, which is the convention for release tags.

```sh
git tag -a v<version> -m "Release v<version>"
git push origin v<version>
```

The tag name must match the `Cargo.toml` `version` exactly, prefixed with `v`
(for example, `v0.1.0`).

## Optional GitHub Release

Create a GitHub release for the tag if maintainers want release notes mirrored
outside crates.io. Use the matching `CHANGELOG.md` entry as the release-note
source.
