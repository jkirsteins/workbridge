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
