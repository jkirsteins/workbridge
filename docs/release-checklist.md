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
