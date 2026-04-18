# Crates.io First Publish Design

## Context

Workbridge is a Rust binary crate currently named `workbridge` at version
`0.1.0`. The manifest already includes the required core crates.io metadata:
`license`, `description`, `repository`, and `readme`.

The goal is to prepare the repository for a manual first publish to crates.io,
following Rust community conventions without adding release automation before
the first release process has been exercised.

The `workbridge` crate name was checked against the crates.io index path
`wo/rk/workbridge` and was not present at planning time. The fallback names
`work_bridge` and `work-bridge` were also not present. Because crate names are
allocated first-come, first-served, the release checklist must still require a
final name-availability check immediately before publishing.

## Scope

In scope:

- Prepare package metadata for public crates.io distribution.
- Document public installation with `cargo install workbridge`.
- Preserve local development installation with `cargo install --path .`.
- Add a conventional changelog with an initial `0.1.0` entry.
- Add a manual release checklist at `docs/release-checklist.md`.
- Audit package contents before choosing whether to add `include` or `exclude`
  manifest rules.
- Verify package readiness with Cargo's package and publish dry-run commands.

Out of scope:

- Running the real `cargo publish` command during implementation.
- Adding GitHub Actions publishing automation.
- Adding `cargo-release`, `cargo-smart-release`, `release-plz`, or other release
  tooling.
- Renaming the product or crate while `workbridge` remains available.

## Design

### Package Metadata

`Cargo.toml` should keep `package.name = "workbridge"`. The implementation
should review crates.io-facing metadata and add conventional discoverability
fields where they fit the project, such as `keywords`, `categories`, and
`homepage`. The homepage should point at the GitHub repository unless a more
specific project site already exists in the repo. Do not add a `documentation`
field unless there is a maintained public documentation URL distinct from the
repository README.

The metadata should remain accurate and conservative. The crate is a terminal
application, not a library, so categories and keywords should describe the CLI,
terminal UI, git/worktree orchestration, and development workflow aspects rather
than advertising unsupported use cases.

### Installation Documentation

`README.md` should make crates.io installation the normal user path after
publication:

```sh
cargo install workbridge
```

The README should continue to document local development installation:

```sh
cargo install --path .
```

Any wording that implies the only supported install path is local source install
should be updated.

### Changelog

Add `CHANGELOG.md` with an initial `0.1.0` entry. The entry should summarize the
first public release rather than trying to reconstruct every historical commit.
The file should use a common, lightweight changelog shape with newest releases
first.

### Release Checklist

Add `docs/release-checklist.md` as the manual maintainer checklist. It should
cover:

- Verify the `workbridge` crate name is still available immediately before the
  first publish.
- Confirm the working tree is clean and the intended release commit is checked
  out.
- Run formatting, lint, and tests.
- Inspect packaged files with `cargo package --list`.
- Run `cargo publish --dry-run`.
- Run the real `cargo publish` manually.
- Tag the published commit.
- Optionally create a GitHub release.

The checklist should explicitly say not to use `--no-verify` or `--allow-dirty`
for normal releases. This matches project rules against skipping validation and
avoids publishing unreviewed local state.

### Package Contents

The implementation should run `cargo package --list` before deciding whether to
add manifest `include` or `exclude` rules. If Cargo's default VCS-based package
contents are already appropriate, no explicit package file list is needed.

If the package includes files that are not useful to build, install, audit, or
understand the crate, the implementation should add the smallest clear
`exclude` list. Avoid using an overly narrow `include` list unless it is clearly
safer, because it can accidentally omit docs, assets, or other files needed by
the package.

## Verification

Implementation should run:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features
cargo test --all-features
cargo package --list
cargo publish --dry-run
```

If `cargo publish --dry-run` cannot complete because the sandbox cannot reach
crates.io, report that explicitly and include the command in
`docs/release-checklist.md` for the maintainer to run outside the sandbox.

## Future Automation

After the first manual release, future work may add release automation if the
manual process becomes repetitive. Reasonable Rust ecosystem options include
`cargo-release`, `cargo-smart-release`, `release-plz`, or crates.io trusted
publishing from GitHub Actions. This design intentionally does not choose one
before the first release.
