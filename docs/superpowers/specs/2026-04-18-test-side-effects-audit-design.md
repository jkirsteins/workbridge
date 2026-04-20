# Design: audit and prevent test-suite side effects on the developer environment

Date: 2026-04-18
Branch: `janis.kirsteins/quickstart-6850`

## Problem

`cargo test` currently writes strings like `feat/my-branch` and `workbridge`
to the developer's real system clipboard, silently overwriting whatever the
user had there. The leak was diagnosed in a prior RCA on this branch:

- `src/clipboard.rs::copy` gates the OSC 52 stdout write behind
  `#[cfg(not(test))]` (lines 43-50), but the `arboard::Clipboard::new().set_text(...)`
  block at lines 52-57 is NOT gated.
- Two unit tests in `src/event.rs` drive the full mouse-event pipeline into
  `App::fire_chrome_copy`, which calls `crate::clipboard::copy(value)`:
  - `chrome_click_inside_right_panel_still_fires` (line 3774) uses value
    `"feat/my-branch"` (line 3790).
  - `chrome_click_inside_global_drawer_still_fires` (line 3926) uses value
    `"workbridge"` (line 3954).
- On every `cargo test` run, both strings land on the real clipboard via
  `arboard`.

This is a direct violation of `docs/invariants.md` invariant 9 ("Tests must
not modify production state"), which is tagged `[ABSOLUTE]` in `CLAUDE.md`.
The invariant already exists; what is missing is structural enforcement.
Review discipline alone is not enough - one missing `#[cfg(not(test))]` in
a helper slipped past review and ships the bug on every test run.

## Goal

Close the gap structurally, not by policy. After this change:

1. No test run may touch the system clipboard, the user's real config / data
   directories, `$HOME`, the user's env vars, or any other host-visible
   side-effect channel.
2. Adding a new side-effect call site in the future is physically blocked at
   commit time unless the code routes through a single sanctioned module.
3. The review policy treats a test-side-effect regression as P0.

## Non-goals

- Trait-injecting a clipboard backend so tests can assert on copied
  content. YAGNI: no current test needs that, and the `#[cfg(test)]` no-op
  path is sufficient. Promoting clipboard to a trait is additive if a future
  test needs it.
- Eliminating the real-process spawns in `src/session.rs` (`sleep 60`,
  `sleep 0`) or the real Unix socket bind in `src/mcp.rs`. Both are bounded,
  documented in `docs/TESTING.md`, and converting them to in-process mocks
  is orthogonal. Flagged as "known acceptable side effects" and left alone.
- Modifying `docs/invariants.md`. Its wording already covers this semantically,
  and `CLAUDE.md` pins invariant edits as P0. The gap is enforcement, not
  specification.

## Audit summary

The audit sweep (grep + read) across the whole tree produced:

| Risk class                         | Status                                               |
| ---------------------------------- | ---------------------------------------------------- |
| System clipboard via `arboard`     | **LEAKING** (`src/clipboard.rs`, 2 tests in event.rs)|
| OSC 52 via stdout                  | Safe (gated by `#[cfg(not(test))]`)                  |
| Environment variables              | Clean (no `env::set_var` / `remove_var` in tree)     |
| Real network                       | Clean (no `reqwest` / `ureq` / `hyper` / `octocrab`) |
| Production config / data dir       | Safe by convention, not structurally enforced        |
| Fixed-name temp dirs               | Flaky under parallel test threads (~18 sites)        |
| Real process spawns                | Bounded (`sleep` in session tests; integration gate) |
| Unix socket bind                   | Bounded (UUID-suffixed tempfile socket)              |
| Raw stdout/stderr terminal escapes | Safe (only OSC 52, already gated)                    |

Leak severity: the clipboard write happens on every normal `cargo test` run.
All other classes are either clean or bounded.

## Design

### 1. New module: `src/side_effects/`

Create `src/side_effects/mod.rs` that re-exports two submodules:

```
src/side_effects/
    mod.rs
    clipboard.rs      // relocated from src/clipboard.rs
    paths.rs          // new: ProjectDirs / BaseDirs / UserDirs / home_dir wrappers
```

This is the ONLY place in the tree allowed to reference:

- `arboard::` (any item)
- `directories::ProjectDirs`, `directories::BaseDirs`, `directories::UserDirs`
- `std::env::home_dir` (deprecated but still worth blocking)
- Raw OSC 52 escape construction (`\x1b]52`) written to stdout
- `std::env::temp_dir()` (forbidden outside `side_effects/` and a small
  `test_support` helper that returns a `tempfile::TempDir`)

All callers must go through the wrappers.

#### 1a. `src/side_effects/clipboard.rs`

Relocated from `src/clipboard.rs`. Same public API (`pub fn copy(text: &str) -> bool`,
`pub fn osc52_sequence(text: &str) -> String`). The change is structural:

```rust
pub fn copy(text: &str) -> bool {
    #[cfg(test)]
    { let _ = text; return false; }   // test-mode no-op, byte-identical contract

    #[cfg(not(test))]
    {
        let mut ok = false;
        // ... existing OSC 52 + arboard bodies, unchanged ...
        ok
    }
}
```

The `arboard` block moves INSIDE the `#[cfg(not(test))]` wrapper so it cannot
execute during `cargo test`. `osc52_sequence` stays pure and testable.

Rationale for returning `false` in test mode: the two calling tests
(`chrome_click_inside_right_panel_still_fires`, `chrome_click_inside_global_drawer_still_fires`)
assert on toast-text substring matching the copied value (`src/event.rs:3822, 3969`).
`App::fire_chrome_copy` at `src/app.rs:2523-2527` emits `Copy failed: <short>`
when `ok == false` and `Copied: <short>` when true; both contain the value.
Tests stay green either way.

#### 1b. `src/side_effects/paths.rs`

New wrappers, all gated:

```rust
#[cfg(not(test))]
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "workbridge")
}
#[cfg(test)]
pub fn project_dirs() -> Option<directories::ProjectDirs> { None }

#[cfg(not(test))]
pub fn home_dir() -> Option<std::path::PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}
#[cfg(test)]
pub fn home_dir() -> Option<std::path::PathBuf> { None }
```

Returning `None` under `cfg(test)` matches the existing `NoConfigDir` error
branch in `src/config.rs:251`, so tests that go through this path already
handle the `None` case. Tests that need specific config behaviour construct
`Config::for_test()` directly.

### 2. Call-site migration

| Existing call                                                    | New call                                |
| ---------------------------------------------------------------- | --------------------------------------- |
| `crate::clipboard::copy(x)`                                      | `crate::side_effects::clipboard::copy(x)` |
| `directories::ProjectDirs::from("", "", "workbridge")`           | `crate::side_effects::paths::project_dirs()` |
| `directories::UserDirs::new().map(...)` (in `src/config.rs:74`)  | `crate::side_effects::paths::home_dir()` |

Specific sites:

- `src/app.rs::fire_chrome_copy` (2521)
- `src/event.rs::copy_selection_to_clipboard` (2823)
- `src/config.rs::home_dir` (74), `src/config.rs::config_path` (251)
- `src/work_item_backend.rs::LocalFileBackend::new` (364)
- `src/metrics.rs::default_data_dir` (454)

The old `src/clipboard.rs` file is deleted. Its tests (`osc52_sequence_shape`,
`osc52_sequence_roundtrips_various_lengths`, `osc52_sequence_handles_unicode`)
move to `src/side_effects/clipboard.rs` unchanged.

### 3. Tempdir migration

Replace every `std::env::temp_dir().join("workbridge-test-<fixed-name>")`
site with `tempfile::tempdir()` (auto-cleanup on drop, unique per run).

Sites (from the audit):

- `src/app.rs`: 9 sites at lines 13080, 13129, 13158, 13620, 14138, 18753,
  18841, 20448, 24646.
- `src/config.rs`: 7 sites at lines 627, 650, 666, 684, 698, 711, 900.
- `src/work_item_backend.rs`: 1 site at line 1010.
- `src/metrics.rs`: 1 site at line 488.

Already-collision-safe sites using `Uuid::new_v4()` or `tempfile::tempdir()`
(`src/app.rs:15120, 19075, 19321`, plus all `src/worktree_service.rs` tests)
are left alone - they already satisfy the invariant. They are flagged as
optional to convert for uniformity but do not need to ship in this PR.

Each migration follows the pattern:

```rust
// BEFORE
let dir = std::env::temp_dir().join("workbridge-test-xyz");
let _ = std::fs::remove_dir_all(&dir);
// ... test body uses `dir` ...
let _ = std::fs::remove_dir_all(&dir);

// AFTER
let tmp = tempfile::tempdir().expect("tempdir");
let dir = tmp.path().to_path_buf();
// ... test body uses `dir` ...
// auto-cleanup on drop of `tmp`
```

### 4. Pre-commit grep check

Extend `hooks/pre-commit` with a new block, same pattern as the existing
`bare_git_files` and `bare_agent_files` blocks:

```bash
echo "=== pre-commit: checking for bypasses of side_effects/ ==="
bare_sideeffect_files=""
for f in $(git diff --cached --name-only -- '*.rs'); do
    # side_effects/* is the ONLY place allowed to reach the real env.
    if [[ "$f" == src/side_effects/* ]]; then
        continue
    fi
    # test_support is also allowed to use tempfile::tempdir passthrough.
    if [ ! -f "$f" ]; then continue; fi
    if grep -qE 'arboard::|directories::(ProjectDirs|BaseDirs|UserDirs)|std::env::home_dir|std::env::temp_dir' "$f"; then
        bare_sideeffect_files="$bare_sideeffect_files $f"
    fi
done
if [ -n "$bare_sideeffect_files" ]; then
    echo "ERROR: These files bypass src/side_effects/:"
    echo "$bare_sideeffect_files"
    echo "Use crate::side_effects::{clipboard, paths} or tempfile::tempdir() instead."
    echo "See docs/TESTING.md and src/side_effects/."
    exit 1
fi
echo "Side-effect gate check OK."
```

Notes:

- The check matches substrings, not full paths, so an `std::env::temp_dir`
  call typed as `env::temp_dir()` is NOT caught - that is acceptable because
  the review policy P0 rule covers semantic equivalents, and the check is
  defense-in-depth rather than sole gate.
- `side_effects/` files are unconditionally allowed to reference the real
  APIs. That is the whole point of the module.
- A future `src/test_support/` helper that re-exports `tempfile::tempdir`
  does not need a bypass entry because `tempfile::tempdir` is not one of
  the blocked patterns.

### 5. Regression test

Add one test in `src/side_effects/clipboard.rs`:

```rust
#[cfg(test)]
mod copy_test_mode_is_noop {
    use super::copy;

    #[test]
    fn copy_returns_false_under_cfg_test() {
        // The test-mode build of `copy` must NOT touch arboard or stdout.
        // Contract: always return false; never write to the system clipboard.
        assert!(!copy("unit-test-probe-value"));
    }
}
```

This pins the contract so a future refactor that accidentally removes the
`#[cfg(test)]` gate fails this test rather than silently regressing.

### 6. Documentation

#### 6a. `docs/TESTING.md`

Add bullets to the "No host system side effects" list (line 26-32):

- System clipboard writes (via `arboard`, OSC 52, `NSPasteboard`, or any other path).
- Terminal escape sequences written to stdout/stderr outside `src/side_effects/`.
- Notification / audio / visual APIs.

Add a new section immediately after, "Side-effect gating module":

> All code paths that reach the host system outside `std::env::temp_dir()`
> live in `src/side_effects/`. That module is the ONLY place allowed to call
> `arboard`, `directories::ProjectDirs` / `BaseDirs` / `UserDirs`,
> `std::env::home_dir`, or write raw terminal escape sequences. Under
> `#[cfg(test)]` every wrapper returns a no-op or `None`. The pre-commit
> hook (`hooks/pre-commit`) enforces this structurally by rejecting staged
> `.rs` files outside `src/side_effects/` that reference the gated symbols.

Update the "Use temp directories for filesystem operations" section to
recommend `tempfile::tempdir()` explicitly and warn against bare
`std::env::temp_dir().join("fixed-name")` patterns.

#### 6b. `CLAUDE.md` review policy

Add one new P0 bullet to the "Severity overrides" section (default-overridable,
not `[ABSOLUTE]`):

> - Tests that cause side effects on the host environment outside
>   `std::env::temp_dir()` are P0 unless the specific side effect is covered
>   by a session authorization naming the test and the rationale. Covered
>   side-effect channels include: the system clipboard (via `arboard`,
>   OSC 52, or any other path), `directories::ProjectDirs` / `BaseDirs` /
>   `UserDirs` paths, `$HOME` writes, environment-variable mutations without
>   deterministic restore, persistent files or sockets outside
>   `std::env::temp_dir()`, spawned processes that outlive the test,
>   notification / audio / visual APIs, and terminal escape sequences
>   written to stdout/stderr. The reference anti-pattern was
>   `arboard::Clipboard::new().set_text(...)` running during `cargo test`
>   and clobbering the user's clipboard with test-fixture strings
>   (`"feat/my-branch"`, `"workbridge"`) that had been registered as copy
>   targets in event-pipeline tests. All side-effect APIs must be reached
>   only through `src/side_effects/`, which is `#[cfg(not(test))]`-gated.
>   The pre-commit grep check in `hooks/pre-commit` enforces bypass
>   rejection at commit time. Default interpretation is "any side effect
>   visible outside the test process is a bug".

#### 6c. `docs/invariants.md`

Unchanged. Invariant 9's current wording already covers this semantically
("any other production path" is broad enough to include the clipboard and
terminal escape channels). Per `CLAUDE.md` the file is P0-to-edit and needs
explicit per-line authorization, so leaving it alone is the default-safe
choice.

## Success criteria

1. `cargo test` produces zero writes to the system clipboard, the real
   `~/Library/Application Support/workbridge/` / `~/.config/workbridge/`
   directories, `$HOME`, or any fixed path outside `tempfile::tempdir()`.
   Manually verifiable by: put `"sentinel-value"` on the clipboard, run
   `cargo test`, paste - must still show `"sentinel-value"`.
2. The new unit test `copy_returns_false_under_cfg_test` is green.
3. `hooks/pre-commit` rejects a synthetic commit that adds
   `directories::ProjectDirs::from(...)` to a non-`side_effects/` file.
4. All existing tests stay green (no behavioural regression in test
   assertions).
5. `docs/TESTING.md` lists clipboard and terminal escapes as forbidden.
   `CLAUDE.md` review policy has the new P0 bullet. `docs/invariants.md`
   is byte-identical.

## Rollout

Single PR on `janis.kirsteins/quickstart-6850`. Bisectable:

1. Introduce `src/side_effects/` module with relocated clipboard and new paths wrappers (no caller migration yet, both modules co-exist briefly).
2. Migrate all call sites to the new module.
3. Delete the old `src/clipboard.rs`.
4. Tempdir migration (18 sites, mechanical).
5. Extend `hooks/pre-commit`.
6. Add regression test.
7. Update `docs/TESTING.md` and `CLAUDE.md`.

Each step compiles and tests green on its own; the PR can be split if it
gets large, but a single PR is probably the right size given ~30 files
touched with mostly mechanical changes.
