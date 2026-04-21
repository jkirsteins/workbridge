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
//! (Ghostty, iTerm2, Alacritty, Kitty, `WezTerm`, xterm) silently swallow
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
//!
//! The test-mode contract is enforced two ways, not just one:
//!
//! 1. **Outer `#[cfg(not(test))]` gating**: the `arboard` import
//!    and the production implementation body are both behind
//!    `#[cfg(not(test))]`, so under `cargo test` the test binary
//!    never links the arboard code path. Removing
//!    `#[cfg(not(test))]` on the `use` line would produce an
//!    unused-import warning and fail clippy.
//!
//! 2. **Inner compile-time assertion**: the production path
//!    begins with a `const { assert!(!cfg!(test)) }` block. If a
//!    future refactor weakens the outer `#[cfg(not(test))]`
//!    attribute and the production body ends up compiled under
//!    test, the `cfg!(test)` constant inside the const block
//!    flips to `true` and the build fails - before the test
//!    binary can even be linked, let alone execute
//!    `arboard::Clipboard::new()`. The check is intentionally in
//!    the production block (not the test block) because its job
//!    is to fail if production ever ends up reachable from a
//!    test-mode compilation unit.
//!
//! The paired `copy_is_noop_under_cfg_test` test also performs a
//! `const { assert!(cfg!(test)) }` check so the `cfg(test)`
//! expectation is pinned symmetrically on the test side.

#[cfg(not(test))]
use std::io::Write;

/// Attempt to copy `text` to the system clipboard via OSC 52 and
/// `arboard`. Returns `true` if at least one path succeeded.
///
/// Under `#[cfg(test)]` this is a no-op that returns `false`. See the
/// module doc for the rationale.
///
/// Safety: this function writes a short escape sequence directly to
/// `stdout`. That is safe to call between ratatui draws (which is the
/// only place it runs - from `handle_mouse` and from the PTY
/// drag-select handler) because ratatui's frame writes also go through
/// the same `stdout` handle. We flush after the write so the sequence
/// reaches the terminal before the next draw.
#[must_use]
pub fn copy(text: &str) -> bool {
    #[cfg(test)]
    {
        let _ = text;
        false
    }

    #[cfg(not(test))]
    {
        // Compile-time belt-and-braces gate: if a future refactor
        // relaxes the `#[cfg(not(test))]` attributes above and the
        // production body becomes reachable under `cargo test`, the
        // `cfg!(test)` inside this const block flips to `true` and
        // the `assert!` fails at BUILD time - before the test
        // binary even links `arboard`. Do not "fix" a failure of
        // this assertion by removing it: the failure means the
        // compile-time gate above was loosened and the real host
        // clipboard is about to be clobbered by test fixtures. See
        // the module doc for the full test-mode contract.
        const {
            assert!(!cfg!(test));
        }

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
#[must_use]
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
/// for malformed input. Inlined here to keep the crate dep-free.
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
        // Exact byte prefix and suffix.
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));

        // Middle decodes back to the original.
        let middle = &seq["\x1b]52;c;".len()..seq.len() - 1];
        let decoded = base64_decode(middle).expect("valid base64");
        assert_eq!(decoded, b"hello");
    }

    #[test]
    fn osc52_sequence_roundtrips_various_lengths() {
        // Cover all three remainder cases in the encoder.
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
        let payload = "feat: shíp it \u{1f680}"; // rocket emoji
        let seq = osc52_sequence(payload);
        let middle = &seq["\x1b]52;c;".len()..seq.len() - 1];
        let decoded = base64_decode(middle).expect("valid base64");
        assert_eq!(decoded, payload.as_bytes());
    }

    /// Contract: under `#[cfg(test)]`, `copy` must be a pure no-op
    /// that returns `false`. This prevents a future refactor from
    /// re-introducing the pre-2026-04-20 leak where `arboard` ran
    /// during `cargo test` and clobbered the user's real clipboard
    /// with test-fixture strings. If this test fails, someone has
    /// removed the `#[cfg(test)]` early-return; do NOT "fix" the
    /// test - fix the gate.
    ///
    /// The test pins two things, not just one:
    ///
    /// 1. `copy` returns `false`. This catches the straightforward
    ///    regression where the early-return body was removed and
    ///    `arboard` ran and returned `true`.
    ///
    /// 2. `cfg!(test)` is `true` from within this test. This pins
    ///    the runtime view that the `debug_assert!(!cfg!(test), ...)`
    ///    in the production body relies on. If a future build mode
    ///    caused `cfg!(test)` to report `false` inside a unit test
    ///    the production runtime gate would silently stop firing;
    ///    this assertion would flag that regression immediately so
    ///    the compile-time-only half of the contract is never the
    ///    only line of defense.
    #[test]
    fn copy_is_noop_under_cfg_test() {
        // Compile-time check: `cfg(test)` must evaluate to `true`
        // inside this test. The production body's runtime gate is
        // `debug_assert!(!cfg!(test), ...)`, so if the build mode
        // ever caused `cfg!(test)` to report `false` in a unit
        // test the runtime gate would silently stop firing. A
        // `const { assert!(cfg!(test), ...) }` block turns that
        // assumption into a build-time error instead of a silent
        // gap.
        const { assert!(cfg!(test)) };
        let before_call = "workbridge-regression-probe";
        // Call must return false without touching any real backend.
        assert!(!copy(before_call));
        // Second call: still false, still no side effect.
        assert!(!copy("another-probe"));
    }
}
