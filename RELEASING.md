# Releasing

How to cut a new release of Workbridge.

Workbridge uses [`cargo-release`](https://github.com/crate-ci/cargo-release)
to bump the version, update the changelog, tag the commit, publish to
crates.io, and push the commit and tag to origin in one command. The
release configuration lives in [`release.toml`](release.toml) at the
repo root; this document covers the human workflow.

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

4. Verify the declared MSRV still matches the transitive `rust_version`
   floor of the locked dependency graph. The pre-release hook only
   re-runs `fmt` / `clippy` / `test` against the caller's toolchain
   (typically the latest stable), so it does not catch a stale
   `package.rust-version` on its own. Compute the real floor from
   `Cargo.lock` and compare it to `package.rust-version` in
   `Cargo.toml`:

   ```sh
   cargo metadata --format-version 1 --locked \
     | python3 -c 'import json, sys
   d = json.load(sys.stdin)
   pkgs = {p["id"]: p for p in d["packages"]}
   nodes = {n["id"]: n for n in d["resolve"]["nodes"]}
   root = d["resolve"]["root"]
   visited, queue = set(), [root]
   while queue:
       nid = queue.pop()
       if nid in visited:
           continue
       visited.add(nid)
       for dep in nodes[nid].get("deps", []):
           kinds = [k.get("kind") for k in dep.get("dep_kinds", [])]
           if any(k is None for k in kinds):
               queue.append(dep["pkg"])
   versions = [pkgs[pid].get("rust_version") for pid in visited]
   versions = [v for v in versions if v]
   versions.sort(key=lambda v: tuple(int(x) for x in v.split(".")), reverse=True)
   print(versions[0] if versions else "none")'
   ```

   If the value printed is higher than the current `package.rust-version`,
   land a separate `chore:` commit that bumps `package.rust-version` to
   match BEFORE running `cargo release`. Publishing a declared MSRV below
   the actual lockfile floor causes `cargo install --locked workbridge`
   to fail to compile on toolchains that satisfy the declared MSRV but
   not the true transitive floor.

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

Before cutting the first `cargo release` run against a brand-new crate
name, confirm the crate name is actually available on crates.io. The
authoritative signal is the crates.io registry itself, not any local
tooling:

- **Primary check:** confirm that `https://crates.io/crates/workbridge`
  returns a 404 in a browser. Any other response (200, redirect, etc.)
  means the name is already registered.

- **Optional cross-check** with `cargo info` (stable since Rust 1.79).
  This command MUST be run from a directory OUTSIDE the workbridge
  workspace, because from inside the workspace it resolves the local
  package and exits 0 even when the name is unpublished. From a scratch
  directory it hits the registry and returns the correct signal:

  ```sh
  cd /tmp && cargo info workbridge
  ```

  Exit 101 with a "could not find `workbridge` in registry" message
  means the name is available. Exit 0 with registry metadata (authors,
  versions, downloads) means the name is already taken.

If the crates.io URL resolves to anything other than 404 (or the from-
`/tmp` `cargo info` invocation exits 0 with registry metadata), stop.
Choose a new crate name and audit `Cargo.toml`, README install commands,
UI text, docs, and release notes before publishing.

Once the name is confirmed available, the first real release should be
the smallest possible bump so the whole chain can be validated against
crates.io without committing to a visible version jump:

```sh
cargo release patch --execute
```

If anything goes wrong, the patched version is trivially yankable and
the next attempt cuts the next patch.
