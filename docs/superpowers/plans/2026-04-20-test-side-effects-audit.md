# Test Side-Effects Audit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Structurally prevent `cargo test` from touching the host system (clipboard, real config/data directories, `$HOME`, fixed-name temp dirs) by centralizing every host-visible API in a new `src/side_effects/` module, migrating all call sites, and enforcing the boundary at commit time with a pre-commit grep check plus a new P0 review-policy rule.

**Architecture:** Move the existing `src/clipboard.rs` into `src/side_effects/clipboard.rs` with the `arboard` path newly gated by `#[cfg(not(test))]`. Add `src/side_effects/paths.rs` with thin `#[cfg(not(test))]`-gated wrappers around `directories::ProjectDirs` and `directories::UserDirs` (returning `None` under `cfg(test)`). Migrate every existing caller. Replace ~18 fixed-name `std::env::temp_dir().join(...)` test sites with `tempfile::tempdir()`. Extend `hooks/pre-commit` with a grep block that rejects staged `.rs` files outside `src/side_effects/` referencing the gated symbols. Leave `docs/invariants.md` untouched (invariant 9 already covers this semantically and is `[ABSOLUTE]`); update `docs/TESTING.md` and `CLAUDE.md` instead.

**Tech Stack:** Rust 2024 edition, existing deps (`arboard = "3"`, `directories = "5"`, `tempfile = "3"`, `uuid`). No new dependencies. Bash for the pre-commit hook.

**Spec reference:** `docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md` (commit `3534dfd`).

---

## File map

**Create:**
- `src/side_effects/mod.rs` - module root, re-exports submodules.
- `src/side_effects/clipboard.rs` - relocated clipboard helper with new `cfg(test)` gating.
- `src/side_effects/paths.rs` - `ProjectDirs` + `home_dir` wrappers.

**Delete:**
- `src/clipboard.rs` (after its contents move to `src/side_effects/clipboard.rs`).

**Modify:**
- `src/main.rs` - change `mod clipboard;` to `mod side_effects;`.
- `src/app.rs` - migrate `fire_chrome_copy` call site (~line 2521); migrate 9 fixed-name tempdir sites.
- `src/event.rs` - migrate `copy_selection_to_clipboard` call site (~line 2823).
- `src/config.rs` - migrate `home_dir` (line 74) and `config_path` (line 251); migrate 7 fixed-name tempdir sites.
- `src/work_item_backend.rs` - migrate `LocalFileBackend::new` (line 364); migrate 1 fixed-name tempdir site.
- `src/metrics.rs` - migrate `default_data_dir` (line 454); migrate 1 fixed-name tempdir site.
- `hooks/pre-commit` - add a new grep block enforcing the gate.
- `docs/TESTING.md` - extend the forbidden-channel list and add a "Side-effect gating module" section.
- `CLAUDE.md` - add a new P0 bullet to the "Severity overrides" list.

**Do not touch:**
- `docs/invariants.md` - invariant 9 already covers this; `CLAUDE.md` pins it P0-to-edit.
- `src/session.rs` `sleep 60` / `sleep 0` tests - out of scope per spec "Non-goals".
- `src/mcp.rs` UUID-suffixed socket test - out of scope per spec "Non-goals".
- `src/app.rs` Uuid-suffixed tempdir sites at 15120, 19075, 19321 - already collision-safe per spec; optional, not in this plan.

---

## Task 1: Scaffold `src/side_effects/` module (empty, compiles)

**Files:**
- Create: `src/side_effects/mod.rs`
- Create: `src/side_effects/clipboard.rs` (stub for now)
- Create: `src/side_effects/paths.rs` (stub for now)
- Modify: `src/main.rs:5`

The first commit introduces the module skeleton with stubs so later commits can migrate callers incrementally without a giant mega-diff. Each stub compiles; nothing calls into it yet.

- [ ] **Step 1.1: Create `src/side_effects/mod.rs`**

```rust
//! Host-visible side-effect gate.
//!
//! This module is the ONLY place in the crate that may reach the real
//! system clipboard, `directories::ProjectDirs` / `BaseDirs` / `UserDirs`,
//! `std::env::home_dir`, or emit raw terminal escape sequences. Every
//! public item here is gated by `#[cfg(not(test))]` and returns a
//! deterministic no-op / `None` under `cfg(test)` so `cargo test` cannot
//! touch the host environment.
//!
//! See `docs/TESTING.md` and the P0 review-policy rule in `CLAUDE.md`.
//! The pre-commit hook (`hooks/pre-commit`) enforces this boundary
//! structurally by rejecting staged `.rs` files outside this module
//! that reference the gated symbols.

pub mod clipboard;
pub mod paths;
```

- [ ] **Step 1.2: Create `src/side_effects/clipboard.rs` as a stub that re-exports the old module temporarily**

At this point the old `src/clipboard.rs` still exists. The stub simply re-exports so Task 2 can replace the body without breaking callers that reference `crate::side_effects::clipboard::copy` during the transition. In this task nothing references the new path yet, but we want the stub file to be valid Rust.

```rust
//! Clipboard writes. Gated by `#[cfg(not(test))]` in Task 2; this file
//! is currently a thin re-export of the legacy `crate::clipboard`
//! module. Task 2 replaces the body.

pub use crate::clipboard::{copy, osc52_sequence};
```

- [ ] **Step 1.3: Create `src/side_effects/paths.rs` with real wrappers**

These can go live in Task 1 because they add new symbols without conflicting with anything existing.

```rust
//! Production-path resolvers. Under `cfg(test)` every function returns
//! `None`, which matches the existing `NoConfigDir` error branch callers
//! already handle. Tests that need specific config state construct
//! `Config::for_test()` or use `InMemoryConfigProvider`, not these
//! wrappers.

use std::path::PathBuf;

/// Platform-specific `ProjectDirs` handle for the `workbridge` app.
/// Returns `None` under `cfg(test)` so tests cannot reach the real
/// `~/Library/Application Support/workbridge/` or
/// `~/.config/workbridge/` directories.
#[cfg(not(test))]
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "workbridge")
}

#[cfg(test)]
pub fn project_dirs() -> Option<directories::ProjectDirs> {
    None
}

/// Current user's home directory. Returns `None` under `cfg(test)` so
/// tests cannot reach the real `$HOME`.
#[cfg(not(test))]
pub fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

#[cfg(test)]
pub fn home_dir() -> Option<PathBuf> {
    None
}
```

- [ ] **Step 1.4: Declare the module in `src/main.rs`**

The current file has `mod clipboard;` at line 5. Keep that for now (Task 2 replaces it) and add the new module declaration alongside.

Edit `src/main.rs` to add `mod side_effects;` on the line after `mod clipboard;`:

```rust
mod clipboard;
mod side_effects;
```

- [ ] **Step 1.5: Verify the crate still builds**

Run: `cargo check --all-targets`
Expected: builds clean, no warnings from the new module. If clippy is run: `cargo clippy --all-targets --all-features -- -D warnings` also passes.

- [ ] **Step 1.6: Commit**

```bash
git add src/side_effects/mod.rs src/side_effects/clipboard.rs src/side_effects/paths.rs src/main.rs
git commit -m "$(cat <<'EOF'
Scaffold src/side_effects/ module for test-side-effect gating

Add empty module root plus a clipboard stub that re-exports the legacy
src/clipboard.rs and a paths module with cfg(test)-gated ProjectDirs /
home_dir wrappers. No callers migrated yet; subsequent commits fold the
real clipboard body in and migrate every ProjectDirs / UserDirs site.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Move clipboard body into `side_effects/clipboard.rs` with full `cfg(test)` gating

**Files:**
- Modify: `src/side_effects/clipboard.rs` (replace the stub from Task 1)
- Delete: `src/clipboard.rs`
- Modify: `src/main.rs:5` (remove `mod clipboard;`)
- Modify: `src/app.rs` (one call site in `fire_chrome_copy`)
- Modify: `src/event.rs` (one call site in `copy_selection_to_clipboard`)

This is the task that actually closes the clipboard leak.

- [ ] **Step 2.1: Replace `src/side_effects/clipboard.rs` with the gated body**

Overwrite the stub with the full clipboard implementation, gated so BOTH the OSC 52 write and the `arboard` write are inside a single `#[cfg(not(test))]` block.

```rust
//! Clipboard backend for workbridge.
//!
//! `copy` writes via BOTH OSC 52 (a terminal escape sequence the
//! terminal emulator captures and pushes to the system clipboard) and
//! `arboard` (a cross-platform native clipboard library). Either path
//! succeeding counts as a successful copy. Attempting both is
//! deliberate:
//!
//! - OSC 52 works over SSH, inside tmux (with `set -g set-clipboard on`),
//!   and in sandboxed environments where `arboard` has no X display.
//! - `arboard` works in terminals that strip OSC 52 without forwarding.
//!
//! There is no reliable cross-terminal way to detect OSC 52 support at
//! runtime, so we write the escape unconditionally. Modern terminals
//! (Ghostty, iTerm2, Alacritty, Kitty, WezTerm, xterm) silently swallow
//! sequences they don't recognize.
//!
//! The OSC 52 sequence is built by `osc52_sequence` (split out from
//! `copy` so it is testable without hitting stdout). The base64
//! encoding is a small inline implementation to keep the dependency
//! tree lean - OSC 52 only needs standard base64 with no URL-safety
//! or padding tricks.
//!
//! **Test-mode contract:** under `#[cfg(test)]`, `copy` is a no-op that
//! returns `false`. Callers render "Copy failed: ..." toasts in that
//! case; existing tests assert on substring matches that hold under
//! both branches. This gating closes the pre-fix leak where test
//! fixtures (e.g. "feat/my-branch") were being written to the real
//! system clipboard via `arboard` during `cargo test`.

#[cfg(not(test))]
use std::io::Write;

/// Attempt to copy `text` to the system clipboard via OSC 52 and
/// `arboard`. Returns `true` if at least one path succeeded.
///
/// Under `#[cfg(test)]` this is a no-op that returns `false`. See the
/// module doc for the rationale.
pub fn copy(text: &str) -> bool {
    #[cfg(test)]
    {
        let _ = text;
        return false;
    }

    #[cfg(not(test))]
    {
        let mut ok = false;

        // OSC 52 path.
        let seq = osc52_sequence(text);
        let mut stdout = std::io::stdout().lock();
        if stdout.write_all(seq.as_bytes()).is_ok() && stdout.flush().is_ok() {
            ok = true;
        }

        // arboard path (best-effort).
        if let Ok(mut clipboard) = arboard::Clipboard::new()
            && clipboard.set_text(text.to_string()).is_ok()
        {
            ok = true;
        }

        ok
    }
}

/// Build the OSC 52 escape sequence that asks the terminal to push
/// `text` onto the system clipboard selection. Format:
///
/// ```text
/// ESC ] 52 ; c ; <base64(text)> BEL
/// ```
///
/// `c` selects the CLIPBOARD buffer (as opposed to `p` for the primary
/// X selection). `BEL` (`\x07`) terminates the sequence; the alternate
/// `ST` (`ESC \\`) terminator is also valid but `BEL` is shorter and
/// universally supported.
pub fn osc52_sequence(text: &str) -> String {
    let mut encoded = String::new();
    base64_encode(text.as_bytes(), &mut encoded);
    format!("\x1b]52;c;{encoded}\x07")
}

/// Minimal standard-alphabet base64 encoder. No padding tricks, no
/// URL-safe variant - OSC 52 wants plain RFC 4648 base64. Inlined to
/// avoid pulling in a base64 crate just for one call site.
fn base64_encode(input: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut i = 0;
    while i + 3 <= input.len() {
        let b0 = input[i];
        let b1 = input[i + 1];
        let b2 = input[i + 2];
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let b0 = input[i];
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[((b0 & 0x03) << 4) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let b0 = input[i];
        let b1 = input[i + 1];
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(ALPHABET[((b1 & 0x0f) << 2) as usize] as char);
        out.push('=');
    }
}

/// Decode a standard (non-URL-safe, padded) base64 string. Used only
/// by unit tests to round-trip-check `osc52_sequence`. Returns `None`
/// for malformed input.
#[cfg(test)]
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i < bytes.len() {
        let c0 = bytes[i];
        let c1 = bytes[i + 1];
        let c2 = bytes[i + 2];
        let c3 = bytes[i + 3];
        let v0 = val(c0)?;
        let v1 = val(c1)?;
        out.push((v0 << 2) | (v1 >> 4));
        if c2 != b'=' {
            let v2 = val(c2)?;
            out.push(((v1 & 0x0f) << 4) | (v2 >> 2));
            if c3 != b'=' {
                let v3 = val(c3)?;
                out.push(((v2 & 0x03) << 6) | v3);
            }
        }
        i += 4;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_shape() {
        let seq = osc52_sequence("hello");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));

        let middle = &seq["\x1b]52;c;".len()..seq.len() - 1];
        let decoded = base64_decode(middle).expect("valid base64");
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn osc52_sequence_roundtrips_various_lengths() {
        for payload in [
            "",
            "a",
            "ab",
            "abc",
            "abcd",
            "abcde",
            "https://example.com/pull/42",
        ] {
            let seq = osc52_sequence(payload);
            let middle = &seq["\x1b]52;c;".len()..seq.len() - 1];
            let decoded = base64_decode(middle).expect("valid base64");
            assert_eq!(decoded, payload.as_bytes(), "payload={payload:?}");
        }
    }

    #[test]
    fn osc52_sequence_handles_unicode() {
        let payload = "feat: ship it \u{1f680}";
        let seq = osc52_sequence(payload);
        let middle = &seq["\x1b]52;c;".len()..seq.len() - 1];
        let decoded = base64_decode(middle).expect("valid base64");
        assert_eq!(decoded, payload.as_bytes());
    }
}
```

- [ ] **Step 2.2: Delete the old `src/clipboard.rs`**

```bash
git rm src/clipboard.rs
```

- [ ] **Step 2.3: Drop `mod clipboard;` from `src/main.rs`**

Remove the `mod clipboard;` line (was at line 5). Keep `mod side_effects;` (added in Task 1). The resulting head of the file declares `side_effects` in place of `clipboard`.

Open `src/main.rs`, locate the pair:

```rust
mod clipboard;
mod side_effects;
```

and delete the first of the two lines so only `mod side_effects;` remains.

- [ ] **Step 2.4: Migrate the first caller - `src/app.rs::fire_chrome_copy`**

Locate `pub fn fire_chrome_copy` in `src/app.rs` (around line 2520). The body currently reads:

```rust
pub fn fire_chrome_copy(&mut self, value: String, kind: ClickKind) {
    let ok = crate::clipboard::copy(&value);
    ...
}
```

Change the `crate::clipboard::copy` reference to `crate::side_effects::clipboard::copy`:

```rust
pub fn fire_chrome_copy(&mut self, value: String, kind: ClickKind) {
    let ok = crate::side_effects::clipboard::copy(&value);
    ...
}
```

- [ ] **Step 2.5: Migrate the second caller - `src/event.rs::copy_selection_to_clipboard`**

Locate `fn copy_selection_to_clipboard` in `src/event.rs` (around line 2792). The body ends with:

```rust
crate::clipboard::copy(&text);
```

Change to:

```rust
crate::side_effects::clipboard::copy(&text);
```

- [ ] **Step 2.6: Verify the crate builds and tests pass**

Run: `cargo test --lib`
Expected: all tests green. In particular, `event::tests::chrome_click_inside_right_panel_still_fires` and `chrome_click_inside_global_drawer_still_fires` must stay green because their assertions only check that the toast text contains the copied value, and `fire_chrome_copy` emits "Copy failed: <value>" under the new no-op (also contains the value).

Also run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 2.7: Behavioural verification - the clipboard is no longer touched by `cargo test`**

Put a sentinel string on your clipboard:

```bash
printf 'sentinel-value-do-not-clobber' | pbcopy  # macOS
# or: echo -n 'sentinel-value-do-not-clobber' | xclip -selection clipboard  # Linux
```

Run: `cargo test --lib`

Verify:

```bash
pbpaste  # macOS; should print 'sentinel-value-do-not-clobber'
# or: xclip -selection clipboard -o  # Linux
```

Expected: exact sentinel string. Before this task the output would have been `feat/my-branch` or `workbridge` depending on test order.

- [ ] **Step 2.8: Commit**

```bash
git add src/side_effects/clipboard.rs src/app.rs src/event.rs src/main.rs
git rm --cached src/clipboard.rs 2>/dev/null || true
# (git rm in step 2.2 already staged the deletion; no-op if so)
git commit -m "$(cat <<'EOF'
Gate clipboard writes behind cfg(not(test))

Move src/clipboard.rs into src/side_effects/clipboard.rs and wrap BOTH
the OSC 52 stdout write and the arboard::Clipboard::new().set_text(...)
call in a single #[cfg(not(test))] block. Under cfg(test) the `copy`
function is a no-op returning false, so `cargo test` can no longer
clobber the user's real system clipboard with test-fixture strings
("feat/my-branch", "workbridge") from the chrome click-to-copy tests.

Migrate the two live callers (App::fire_chrome_copy,
event::copy_selection_to_clipboard) to the new module path. Existing
test assertions on toast-text substring remain valid because
fire_chrome_copy emits "Copy failed: <value>" when copy returns false,
which still contains the fixture value.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Migrate `src/config.rs` to `side_effects::paths`

**Files:**
- Modify: `src/config.rs:73-76` (replace local `home_dir`)
- Modify: `src/config.rs:249-253` (replace body of `config_path`)

- [ ] **Step 3.1: Replace the local `home_dir` function**

Locate lines 73-76 of `src/config.rs`:

```rust
/// Get the user's home directory using the directories crate.
fn home_dir() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}
```

Replace with:

```rust
/// Get the user's home directory via the side-effects gate. Returns
/// `None` under `cfg(test)` so tests cannot reach the real `$HOME`.
fn home_dir() -> Option<PathBuf> {
    crate::side_effects::paths::home_dir()
}
```

- [ ] **Step 3.2: Replace the body of `config_path`**

Locate lines 249-253:

```rust
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let proj =
        directories::ProjectDirs::from("", "", "workbridge").ok_or(ConfigError::NoConfigDir)?;
    Ok(proj.config_dir().join("config.toml"))
}
```

Replace with:

```rust
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let proj = crate::side_effects::paths::project_dirs().ok_or(ConfigError::NoConfigDir)?;
    Ok(proj.config_dir().join("config.toml"))
}
```

- [ ] **Step 3.3: Verify**

Run: `cargo test --lib config::` and `cargo clippy --all-targets --all-features -- -D warnings`
Expected: all config tests green, no warnings.

- [ ] **Step 3.4: Commit**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
Route config.rs through side_effects::paths

Replace direct directories::UserDirs / directories::ProjectDirs calls
in home_dir() and config_path() with the cfg(test)-gated wrappers in
side_effects::paths. Under cfg(test) these return None, which maps to
the existing ConfigError::NoConfigDir branch that tests already exercise
via Config::for_test() / InMemoryConfigProvider.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Migrate `src/work_item_backend.rs::LocalFileBackend::new` to `side_effects::paths`

**Files:**
- Modify: `src/work_item_backend.rs:363-377`

- [ ] **Step 4.1: Edit `LocalFileBackend::new`**

Locate lines 363-377 of `src/work_item_backend.rs`. The current body reads:

```rust
pub fn new() -> Result<Self, BackendError> {
    let proj = directories::ProjectDirs::from("", "", "workbridge")
        .ok_or_else(|| BackendError::Io("could not determine data directory".into()))?;
    let data_dir = proj.data_dir().join("work-items");
    fs::create_dir_all(&data_dir).map_err(|e| {
        BackendError::Io(format!(
            "failed to create data dir {}: {e}",
            data_dir.display()
        ))
    })?;
    Ok(Self {
        data_dir,
        counter_lock: Mutex::new(()),
    })
}
```

Replace with:

```rust
pub fn new() -> Result<Self, BackendError> {
    let proj = crate::side_effects::paths::project_dirs()
        .ok_or_else(|| BackendError::Io("could not determine data directory".into()))?;
    let data_dir = proj.data_dir().join("work-items");
    fs::create_dir_all(&data_dir).map_err(|e| {
        BackendError::Io(format!(
            "failed to create data dir {}: {e}",
            data_dir.display()
        ))
    })?;
    Ok(Self {
        data_dir,
        counter_lock: Mutex::new(()),
    })
}
```

- [ ] **Step 4.2: Verify**

Run: `cargo test --lib work_item_backend::`
Expected: all backend tests green. Tests use `LocalFileBackend::with_dir(temp)` and never call `new()`, so the `cfg(test)` no-op path of `project_dirs()` is never hit.

Also run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 4.3: Commit**

```bash
git add src/work_item_backend.rs
git commit -m "$(cat <<'EOF'
Route LocalFileBackend::new through side_effects::paths

Replace directories::ProjectDirs::from with
side_effects::paths::project_dirs in the production constructor. Under
cfg(test) project_dirs() returns None, which maps to the existing
BackendError::Io("could not determine data directory") branch. Tests
use LocalFileBackend::with_dir(temp) and are unaffected.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Migrate `src/metrics.rs::default_data_dir` to `side_effects::paths`

**Files:**
- Modify: `src/metrics.rs:453-456`

- [ ] **Step 5.1: Edit `default_data_dir`**

Locate lines 453-456 of `src/metrics.rs`:

```rust
pub fn default_data_dir() -> Option<PathBuf> {
    let proj = directories::ProjectDirs::from("", "", "workbridge")?;
    Some(proj.data_dir().join("work-items"))
}
```

Replace with:

```rust
pub fn default_data_dir() -> Option<PathBuf> {
    let proj = crate::side_effects::paths::project_dirs()?;
    Some(proj.data_dir().join("work-items"))
}
```

- [ ] **Step 5.2: Verify**

Run: `cargo test --lib metrics::`
Expected: all metrics tests green. Tests call `aggregate_from_activity_logs(&dir)` with explicit temp dirs and never invoke `default_data_dir`.

Also run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 5.3: Commit**

```bash
git add src/metrics.rs
git commit -m "$(cat <<'EOF'
Route metrics::default_data_dir through side_effects::paths

Same migration pattern as LocalFileBackend::new: replace the direct
directories::ProjectDirs::from call with the cfg(test)-gated wrapper.
Tests call aggregate_from_activity_logs with explicit temp dirs and
do not exercise default_data_dir, so the None-under-test branch is
never hit by the test suite.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Tempdir migration in `src/config.rs` (7 sites)

**Files:**
- Modify: `src/config.rs` at lines 627, 650, 666, 684, 698, 711, 900.

All seven sites follow the same pattern. The replacement template is:

```rust
// BEFORE
let dir = std::env::temp_dir().join("workbridge-test-<suffix>");
let _ = std::fs::remove_dir_all(&dir);
// ... test body uses `dir` ...
// (optional trailing cleanup)
let _ = std::fs::remove_dir_all(&dir);

// AFTER
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
// ... test body uses `dir` ...
// no trailing cleanup: _tmp auto-removes on drop
```

Keep the binding name `dir` because existing test bodies reference it. The `_tmp` variable must live for the full test scope; name it `_tmp` (underscore prefix) to document "holds the tempdir alive" and avoid an unused-variable warning.

Do NOT convert Uuid-suffixed sites (`src/app.rs:15120, 19075, 19321`). Those are safe under parallel test threads and are out of scope per the spec.

- [ ] **Step 6.1: Edit `src/config.rs:627`**

Find the test at approximately line 627. The block currently opens with:

```rust
let dir = std::env::temp_dir().join("workbridge-test-config");
let _ = std::fs::remove_dir_all(&dir);
```

Replace with:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
```

Search for any `std::fs::remove_dir_all(&dir)` trailing the same test body and delete those lines too - the auto-cleanup on `_tmp` drop handles it.

- [ ] **Step 6.2: Edit `src/config.rs:650`**

Same pattern. Fixed-name suffix: `workbridge-test-discover`.

Before:

```rust
let dir = std::env::temp_dir().join("workbridge-test-discover");
let _ = std::fs::remove_dir_all(&dir);
```

After:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
```

Delete any trailing `std::fs::remove_dir_all(&dir)` in the same test.

- [ ] **Step 6.3: Edit `src/config.rs:666`**

Same pattern. Fixed-name suffix: `workbridge-test-empty`.

Before:

```rust
let dir = std::env::temp_dir().join("workbridge-test-empty");
let _ = std::fs::remove_dir_all(&dir);
```

After:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
```

- [ ] **Step 6.4: Edit `src/config.rs:684`**

Fixed-name suffix: `workbridge-test-add-repo`.

Before:

```rust
let dir = std::env::temp_dir().join("workbridge-test-add-repo");
let _ = std::fs::remove_dir_all(&dir);
```

After:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
```

- [ ] **Step 6.5: Edit `src/config.rs:698`**

Fixed-name suffix: `workbridge-test-add-fail`. Same before/after pattern.

- [ ] **Step 6.6: Edit `src/config.rs:711`**

Fixed-name suffix: `workbridge-test-add-base`. Same before/after pattern.

- [ ] **Step 6.7: Edit `src/config.rs:900`**

Fixed-name suffix: `workbridge-test-remove`. Same before/after pattern.

- [ ] **Step 6.8: Verify**

Run: `cargo test --lib config::`
Expected: all 7 migrated tests green. Because each test now gets its own unique tempdir, running with parallel threads cannot collide.

Extra sanity: `cargo test --lib config:: -- --test-threads=8` should also be green.

- [ ] **Step 6.9: Commit**

```bash
git add src/config.rs
git commit -m "$(cat <<'EOF'
Migrate config.rs tests to tempfile::tempdir

Replace seven fixed-name std::env::temp_dir().join(...) sites with
tempfile::tempdir() so parallel test threads cannot collide on
/tmp/workbridge-test-<fixed> directories and so the test tree stops
polluting /tmp with predictable directory names. The TempDir binding
auto-cleans on drop, removing the paired manual remove_dir_all calls.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Tempdir migration in `src/work_item_backend.rs` (1 site)

**Files:**
- Modify: `src/work_item_backend.rs:1010`

- [ ] **Step 7.1: Edit `src/work_item_backend.rs:1010`**

Same template as Task 6. The fixed-name suffix at this site is a `format!("workbridge-test-backend-{name}")` with a per-test `name`. Replacement:

Before:

```rust
let dir = std::env::temp_dir().join(format!("workbridge-test-backend-{name}"));
let _ = std::fs::remove_dir_all(&dir);
```

After:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
let _ = name; // retained in case the test referenced `name` only for the suffix
```

If `name` is still used downstream in the test body, drop the `let _ = name;` line. Keep existing references to `dir` unchanged.

Delete any trailing `std::fs::remove_dir_all(&dir)` paired with this test.

- [ ] **Step 7.2: Verify**

Run: `cargo test --lib work_item_backend::`
Expected: all backend tests green.

- [ ] **Step 7.3: Commit**

```bash
git add src/work_item_backend.rs
git commit -m "$(cat <<'EOF'
Migrate work_item_backend.rs test to tempfile::tempdir

Same mechanical conversion as src/config.rs: one fixed-name temp_dir
call becomes tempfile::tempdir() so the test is parallel-safe and
self-cleaning.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Tempdir migration in `src/metrics.rs` (1 site)

**Files:**
- Modify: `src/metrics.rs:488`

- [ ] **Step 8.1: Edit `src/metrics.rs:488`**

Before:

```rust
let dir = std::env::temp_dir().join(format!("workbridge-test-metrics-{name}"));
let _ = std::fs::remove_dir_all(&dir);
```

After:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
```

Delete any trailing `std::fs::remove_dir_all(&dir)` paired with this test.

- [ ] **Step 8.2: Verify**

Run: `cargo test --lib metrics::`
Expected: all metrics tests green.

- [ ] **Step 8.3: Commit**

```bash
git add src/metrics.rs
git commit -m "$(cat <<'EOF'
Migrate metrics.rs test to tempfile::tempdir

One fixed-name temp_dir call becomes tempfile::tempdir() so the test
is parallel-safe and self-cleaning.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Tempdir migration in `src/app.rs` (9 sites)

**Files:**
- Modify: `src/app.rs` at lines 13080, 13129, 13158, 13620, 14138, 18753, 18841, 20448, 24646.

All nine sites follow the Task 6 template. The fixed-name suffixes are (for reference; line numbers may drift as preceding edits land):

| Line  | Fixed-name suffix                               |
| ----- | ----------------------------------------------- |
| 13080 | `workbridge-test-f1-fetcher-flag`               |
| 13129 | `workbridge-test-f3-managed`                    |
| 13158 | `workbridge-test-r3-f1-root`                    |
| 13620 | `workbridge-test-r4-f1-canonical`               |
| 14138 | `workbridge-test-r5-f1-sorted`                  |
| 18753 | `workbridge-test-branchless-spawn`              |
| 18841 | `workbridge-test-branchless-spawn-msg`          |
| 20448 | `workbridge-test-backfill-collect`              |
| 24646 | `workbridge-test-orphan-activity-log`           |

Also `src/app.rs:18641` uses `format!("workbridge-test-branchless-{name}")` inside a helper and should be converted the same way. If it exists after preceding edits land, include it in this task (it's the helper `app_with_branchless_backlog_item`).

- [ ] **Step 9.1: Apply the template to each site**

For each line in the table above, locate the block:

```rust
let dir = std::env::temp_dir().join("<fixed-name>");
let _ = std::fs::remove_dir_all(&dir);
```

and replace with:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
```

Delete any trailing `let _ = std::fs::remove_dir_all(&dir);` paired with the same test.

If the test is a helper (like the line-18641 helper), the `_tmp` handle must be returned from the helper so the outer test keeps the tempdir alive. Return `(app, wi_id, _tmp)` from such helpers instead of `(app, wi_id, dir)` (the caller can still derive `dir = _tmp.path().to_path_buf()` or the helper can return both).

- [ ] **Step 9.2: Verify one more time across the app.rs test surface**

Run: `cargo test --lib app::` and `cargo clippy --all-targets --all-features -- -D warnings`
Expected: all tests green, no warnings.

Parallel-threads sanity: `cargo test --lib app:: -- --test-threads=8`.

- [ ] **Step 9.3: Commit**

```bash
git add src/app.rs
git commit -m "$(cat <<'EOF'
Migrate app.rs tests to tempfile::tempdir

Convert the nine remaining fixed-name std::env::temp_dir().join("workbridge-test-...")
sites in src/app.rs to tempfile::tempdir(). Each test now owns a
uniquely-named TempDir whose Drop removes the directory, so parallel
threads cannot collide and /tmp stops accumulating predictable
directory names between runs.

Uuid-suffixed sites (15120, 19075, 19321) are already collision-safe
and left alone per the spec's out-of-scope section.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Regression test - `copy()` is a no-op under `cfg(test)`

**Files:**
- Modify: `src/side_effects/clipboard.rs` (append to the existing `#[cfg(test)] mod tests` block).

This pins the contract so a future refactor that accidentally removes the `#[cfg(test)]` gate fails here rather than silently regressing.

- [ ] **Step 10.1: Append the regression test**

Inside `src/side_effects/clipboard.rs`, in the existing `#[cfg(test)] mod tests { ... }` block, append:

```rust
    /// Contract: under `#[cfg(test)]`, `copy` must be a pure no-op
    /// that returns `false`. This prevents a future refactor from
    /// re-introducing the pre-2026-04-20 leak where `arboard` ran
    /// during `cargo test` and clobbered the user's real clipboard
    /// with test-fixture strings. If this test fails, someone has
    /// removed the `#[cfg(test)]` early-return; do NOT "fix" the
    /// test - fix the gate.
    #[test]
    fn copy_is_noop_under_cfg_test() {
        let before_call = "workbridge-regression-probe";
        // Call must return false without touching any real backend.
        assert!(!copy(before_call));
        // Second call: still false, still no side effect.
        assert!(!copy("another-probe"));
    }
```

- [ ] **Step 10.2: Verify**

Run: `cargo test --lib side_effects::clipboard::`
Expected: all four tests green (three `osc52_sequence_*` tests from Task 2 plus the new `copy_is_noop_under_cfg_test`).

- [ ] **Step 10.3: Commit**

```bash
git add src/side_effects/clipboard.rs
git commit -m "$(cat <<'EOF'
Pin cfg(test) no-op contract for clipboard::copy

Add a regression test asserting that copy() returns false and has no
side effects under cfg(test). This guards against a future refactor
that accidentally removes the #[cfg(test)] early-return and silently
re-introduces the arboard leak that wrote "feat/my-branch" and
"workbridge" to the user's real system clipboard on every cargo test
run.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: Extend `hooks/pre-commit` with the side-effect gate

**Files:**
- Modify: `hooks/pre-commit`

Add a new block at the end of the existing file, following the same pattern as the `bare_git_files` and `bare_agent_files` blocks.

- [ ] **Step 11.1: Append the new block to `hooks/pre-commit`**

After the existing `echo "Agent spawn check OK."` line at the bottom of the hook, append:

```bash

echo "=== pre-commit: checking for bypasses of src/side_effects/ ==="
# src/side_effects/ is the ONLY place allowed to reach the real system
# clipboard, directories::ProjectDirs / BaseDirs / UserDirs,
# std::env::home_dir, or std::env::temp_dir. Every other file must
# route through side_effects:: wrappers (for production paths) or
# tempfile::tempdir() (for test scratch dirs).
#
# See docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md
# and the corresponding P0 rule in CLAUDE.md.
bare_sideeffect_files=""
for f in $(git diff --cached --name-only -- '*.rs'); do
    # side_effects/* is allowed to mention the gated symbols.
    case "$f" in
        src/side_effects/*) continue ;;
    esac
    if [ ! -f "$f" ]; then
        continue
    fi
    if grep -qE 'arboard::|directories::(ProjectDirs|BaseDirs|UserDirs)|std::env::home_dir|std::env::temp_dir' "$f"; then
        bare_sideeffect_files="$bare_sideeffect_files $f"
    fi
done
if [ -n "$bare_sideeffect_files" ]; then
    echo "ERROR: These files bypass src/side_effects/:"
    echo "$bare_sideeffect_files"
    echo ""
    echo "Host-visible APIs (arboard, directories::ProjectDirs/BaseDirs/UserDirs,"
    echo "std::env::home_dir, std::env::temp_dir) must be reached through"
    echo "crate::side_effects::{clipboard, paths} or tempfile::tempdir(). See:"
    echo "  - docs/TESTING.md 'Side-effect gating module'"
    echo "  - src/side_effects/mod.rs"
    echo "  - CLAUDE.md 'Severity overrides' (P0 rule)"
    exit 1
fi
echo "Side-effect gate check OK."
```

- [ ] **Step 11.2: Verify the hook fires on a synthetic violation**

Simulate a violation to confirm the check works end-to-end. Create a throwaway file that would bypass the gate, try to commit it, and confirm the hook rejects it.

```bash
printf 'use directories::ProjectDirs; fn x() { let _ = ProjectDirs::from("", "", "x"); }\n' > /tmp/synthetic.rs
cp /tmp/synthetic.rs src/synthetic_violation.rs
git add src/synthetic_violation.rs
git commit -m "should fail hook"
```

Expected: `ERROR: These files bypass src/side_effects/: src/synthetic_violation.rs` and exit code 1.

Clean up:

```bash
git reset HEAD src/synthetic_violation.rs
rm src/synthetic_violation.rs /tmp/synthetic.rs
```

- [ ] **Step 11.3: Verify the hook does NOT false-positive on the legitimate side-effects files**

Create an empty commit that re-stages an existing `side_effects/` file to confirm it is exempt.

```bash
touch src/side_effects/clipboard.rs  # no content change
git add -u src/side_effects/clipboard.rs
git diff --cached --name-only  # should list the file
```

Run the hook manually:

```bash
bash hooks/pre-commit
```

Expected: `Side-effect gate check OK.` (even though the file contains `arboard::`). If the hook reports a violation, the exemption logic is broken.

Reset:

```bash
git reset HEAD src/side_effects/clipboard.rs
```

- [ ] **Step 11.4: Commit**

```bash
git add hooks/pre-commit
git commit -m "$(cat <<'EOF'
Enforce side_effects/ boundary in hooks/pre-commit

Add a grep block that rejects staged .rs files outside src/side_effects/
which reference arboard::, directories::ProjectDirs/BaseDirs/UserDirs,
std::env::home_dir, or std::env::temp_dir. Files inside src/side_effects/
are exempt (that is the whole point of the module). Pattern follows the
existing bare_git_files and bare_agent_files blocks in the same hook.

This is the structural backstop for the test-side-effect invariant:
a future change that re-introduces a direct arboard or ProjectDirs call
outside the gate cannot land without an explicit session authorization
per the CLAUDE.md P0 rule added in a subsequent commit.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: Update `docs/TESTING.md`

**Files:**
- Modify: `docs/TESTING.md`

- [ ] **Step 12.1: Extend the "No host system side effects" list**

Locate the section starting at line 24:

```
## No host system side effects

Tests must not leave side effects on the host system. This includes:

- Writing to production config or data directories
- Creating persistent files outside of temp directories
- Modifying environment variables without restoring them
- Spawning processes that outlive the test
```

Append three bullets so the list reads:

```
## No host system side effects

Tests must not leave side effects on the host system. This includes:

- Writing to production config or data directories
- Creating persistent files outside of temp directories
- Modifying environment variables without restoring them
- Spawning processes that outlive the test
- Writing to the system clipboard (via `arboard`, OSC 52, `NSPasteboard`,
  or any other path)
- Writing raw terminal escape sequences to stdout / stderr outside
  `src/side_effects/`
- Using notification, audio, or visual system APIs
```

- [ ] **Step 12.2: Add the "Side-effect gating module" section**

Immediately after the "No host system side effects" section, before "Never use `git config` in tests", insert:

```
## Side-effect gating module

All code paths that reach the host system outside `std::env::temp_dir()`
live in `src/side_effects/`. That module is the ONLY place in the crate
allowed to call `arboard::`, `directories::ProjectDirs` / `BaseDirs` /
`UserDirs`, `std::env::home_dir`, or write raw terminal escape sequences
(such as OSC 52) to stdout.

Under `#[cfg(test)]` every wrapper in `side_effects::` returns a no-op
(`copy` returns `false`) or `None` (`paths::project_dirs`,
`paths::home_dir`). That maps cleanly to existing error branches
(`ConfigError::NoConfigDir`, `BackendError::Io("could not determine
data directory")`) that tests already exercise through
`InMemoryConfigProvider` and `LocalFileBackend::with_dir`.

The pre-commit hook (`hooks/pre-commit`) enforces the boundary
structurally: a staged `.rs` file outside `src/side_effects/` that
references any of the gated symbols is rejected at commit time. See
the P0 rule in `CLAUDE.md` "Severity overrides" for the review policy.

If you genuinely need a new host-visible side effect (for example, a
new notification API), the add path is:

1. Add the call inside `src/side_effects/` behind `#[cfg(not(test))]`,
   returning a no-op / `None` / `false` under `cfg(test)`.
2. Expose a narrow wrapper from `side_effects::` and route callers
   through it.
3. Update `docs/TESTING.md` (this file) and the pre-commit hook's
   grep pattern if the new API uses a new symbol name.
```

- [ ] **Step 12.3: Tighten the "Use temp directories for filesystem operations" section**

Locate the section (around line 18):

```
## Use temp directories for filesystem operations

Tests that need real directories on disk (e.g. to test git repo discovery)
must create them under `std::env::temp_dir()` and clean up after themselves.
Never use hard-coded paths that could collide with real user data.
```

Replace the body with:

```
## Use temp directories for filesystem operations

Tests that need real directories on disk must use `tempfile::tempdir()`.
The returned `TempDir` binds the directory to a unique path and removes
it on drop, so parallel test threads cannot collide and `/tmp` does not
accumulate predictable `workbridge-test-*` directories between runs.

Pattern:

```rust
let _tmp = tempfile::tempdir().expect("tempdir");
let dir = _tmp.path().to_path_buf();
// ... test body uses `dir` ...
// _tmp is dropped at end of scope and removes the directory
```

Do NOT use `std::env::temp_dir().join("fixed-name")`. The pre-commit
hook rejects bare `std::env::temp_dir` outside `src/side_effects/`.
UUID-suffixed names (`std::env::temp_dir().join(format!("...{}", Uuid::new_v4()))`)
are technically collision-safe but still pollute `/tmp`; prefer
`tempfile::tempdir()` for uniformity.
```

- [ ] **Step 12.4: Verify the file renders as intended**

Open `docs/TESTING.md` in a text viewer (or `cat docs/TESTING.md | head -80`) and confirm the sections appear in the right order:

1. No production side effects
2. Use InMemoryConfigProvider
3. Use temp directories for filesystem operations (updated)
4. No host system side effects (extended)
5. **Side-effect gating module** (new)
6. Never use `git config` in tests
7. Use `git_command()` for all git subprocesses
8. Integration tests
9. Sandbox-only verification

- [ ] **Step 12.5: Commit**

```bash
git add docs/TESTING.md
git commit -m "$(cat <<'EOF'
Document side_effects/ module and clipboard/escape forbidden channels

Extend docs/TESTING.md in three ways:

1. Add clipboard writes, raw terminal escapes, and notification / audio
   APIs to the forbidden-channel list.
2. Add a new "Side-effect gating module" section that documents
   src/side_effects/ as the single legal home for host-visible APIs and
   explains the cfg(test) no-op contract plus the pre-commit gate.
3. Tighten the tempdir guidance to recommend tempfile::tempdir()
   explicitly and call out the pre-commit rejection of bare
   std::env::temp_dir usage.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 13: Add the P0 review-policy bullet to `CLAUDE.md`

**Files:**
- Modify: `CLAUDE.md` "Severity overrides" section.

- [ ] **Step 13.1: Add the bullet**

Open `CLAUDE.md`. Locate the "Severity overrides" bullet list. Find the last bullet in the list (at the time of writing, the "Silent fallbacks to a default harness" `[ABSOLUTE]` rule). Append a new bullet AFTER it:

```markdown
- Tests that cause side effects on the host environment outside `std::env::temp_dir()` are P0 unless the specific side effect is covered by a session authorization naming the test and the rationale. Covered side-effect channels include: the system clipboard (via `arboard`, OSC 52, or any other path), `directories::ProjectDirs` / `BaseDirs` / `UserDirs` paths, `$HOME` writes, environment-variable mutations without deterministic restore, persistent files or sockets outside `std::env::temp_dir()`, spawned processes that outlive the test, notification / audio / visual APIs, and terminal escape sequences written to stdout/stderr. The reference anti-pattern was `arboard::Clipboard::new().set_text(...)` running during `cargo test` and clobbering the user's real clipboard with test-fixture strings (`"feat/my-branch"`, `"workbridge"`) that had been registered as copy targets in event-pipeline tests. All side-effect APIs must be reached only through `src/side_effects/`, which is `#[cfg(not(test))]`-gated and returns a no-op / `None` under test. The pre-commit grep check in `hooks/pre-commit` ("side-effect gate check") enforces bypass rejection at commit time - any staged `.rs` file outside `src/side_effects/` that references `arboard::`, `directories::ProjectDirs` / `BaseDirs` / `UserDirs`, `std::env::home_dir`, or `std::env::temp_dir` is rejected. Default interpretation is "any side effect visible outside the test process is a bug". This rule is default-overridable (not `[ABSOLUTE]`) because there are legitimate bounded exceptions - for example, `src/session.rs` spawns short-lived `sleep 60` / `sleep 0` processes to cover the PTY lifecycle, and `src/mcp.rs` binds a UUID-suffixed Unix socket under `/tmp` for the socket-server smoke test. Such exceptions must be documented in `docs/TESTING.md` and the authorization must name the specific test and why the gating helpers are insufficient.
```

- [ ] **Step 13.2: Verify**

Confirm the new bullet renders correctly in the "Severity overrides" list and that no existing bullet text is disturbed:

```bash
grep -n "Tests that cause side effects on the host environment" CLAUDE.md
```

Expected: exactly one match, on a line near the end of the "Severity overrides" bullet list.

- [ ] **Step 13.3: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'EOF'
Add P0 review-policy rule for test side effects

Treat any test that causes host-visible side effects outside
std::env::temp_dir() as a P0 violation unless a session authorization
covers the specific test and rationale. Enumerates the covered channels
(clipboard, ProjectDirs/BaseDirs/UserDirs, $HOME, env vars, persistent
files/sockets, process leaks, notifications, terminal escapes) and
points at src/side_effects/ as the single legal gate, enforced by the
side-effect-gate block in hooks/pre-commit.

Default-overridable rather than [ABSOLUTE] because legitimate bounded
exceptions exist (session.rs sleep-process tests, mcp.rs UUID socket
test); those are documented in docs/TESTING.md and must be named in
any authorization that invokes them.

docs/invariants.md is intentionally not modified; invariant 9 already
covers this semantically and is itself P0-to-edit per the CLAUDE.md
"Modifying docs/invariants.md" rule.

Part of docs/superpowers/specs/2026-04-18-test-side-effects-audit-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 14: Final verification pass

**Files:** none modified.

- [ ] **Step 14.1: Full unit-test run, single-threaded**

Run: `cargo test --lib`
Expected: all tests green.

- [ ] **Step 14.2: Full unit-test run, parallel**

Run: `cargo test --lib -- --test-threads=8`
Expected: all tests green (proves no fixed-name tempdir collisions remain).

- [ ] **Step 14.3: Clippy**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 14.4: Format**

Run: `cargo fmt -- --check`
Expected: no diffs.

- [ ] **Step 14.5: Full test run including integration tests**

Run: `cargo test --all-features`
Expected: all tests green.

- [ ] **Step 14.6: Clipboard behavioural smoke test (end-to-end)**

This re-runs the spec-level success criterion from Task 2.7, now with every migration landed.

```bash
printf 'final-sentinel' | pbcopy
cargo test --all-features
pbpaste
```

Expected: `pbpaste` prints exactly `final-sentinel`. If it prints anything else, a side-effect regression slipped through.

- [ ] **Step 14.7: Pre-commit hook self-check**

Confirm that running the hook against the current HEAD (nothing staged) exits cleanly:

```bash
bash hooks/pre-commit
```

Expected: all four OK lines (`Format OK.`, `Clippy OK.`, `Git env check OK.`, `Agent spawn check OK.`, `Side-effect gate check OK.`).

- [ ] **Step 14.8: Inventory check - no bare ProjectDirs / arboard / home_dir / temp_dir outside the gate**

```bash
grep -rnE 'arboard::|directories::(ProjectDirs|BaseDirs|UserDirs)|std::env::home_dir|std::env::temp_dir' src/ \
  --include='*.rs' \
  | grep -v '^src/side_effects/'
```

Expected: no output. Any line printed here is a gate bypass that must be fixed before declaring the task done.

- [ ] **Step 14.9: If all checks pass, hand the branch back for review**

No commit in this task. The verification pass is read-only. If any check fails, diagnose on the spot and amend the relevant task's commit - do NOT paper over a regression with a new "fix" commit unless the failure is unrelated to this plan.

---

## Spec coverage check

- [x] Create `src/side_effects/{mod.rs, clipboard.rs, paths.rs}` - Tasks 1, 2
- [x] Migrate call sites in `src/app.rs`, `src/event.rs` - Task 2
- [x] Migrate call sites in `src/config.rs`, `src/work_item_backend.rs`, `src/metrics.rs` - Tasks 3, 4, 5
- [x] Delete old `src/clipboard.rs` - Task 2
- [x] Migrate ~18 fixed-name tempdir sites - Tasks 6-9 (7 + 1 + 1 + 9 = 18)
- [x] Extend `hooks/pre-commit` with side-effect-gate block - Task 11
- [x] Add regression test (copy() no-op under cfg(test)) - Task 10
- [x] Update `docs/TESTING.md` - Task 12
- [x] Add P0 bullet to `CLAUDE.md` review policy - Task 13
- [x] Leave `docs/invariants.md` untouched - enforced by not listing it in any "Modify" file list
- [x] Final verification pass - Task 14
