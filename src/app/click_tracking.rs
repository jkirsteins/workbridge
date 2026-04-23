//! Click-tracking subsystem - per-frame click target registry and
//! pending-click gesture state for the click-to-copy flow.
//!
//! `App` previously held `click_registry: RefCell<ClickRegistry>` and
//! `pending_chrome_click: Option<...>` as two sibling fields, with
//! `fire_chrome_copy` implemented directly on `impl App`. Copy
//! effects (clipboard + confirmation toast) straddled the boundary
//! between click tracking and the toast queue, which meant every
//! call site needed a `&mut App` borrow even when only the two
//! subsystems were involved.
//!
//! This module owns the two fields as a single `ClickTracking`
//! struct and exposes `fire_copy(&mut self, &mut Toasts, ..)` - the
//! cross-subsystem call takes an explicit `&mut Toasts` borrow,
//! making the field-borrow split at the call site visible in the
//! signature.

use std::cell::RefCell;

use super::{Toasts, short_display};
use crate::click_targets::{ClickKind, ClickRegistry};

/// Owns the per-frame click registry (populated during render,
/// consumed by `handle_mouse`) and the pending click gesture that
/// tracks a `Down(Left)`/`Up(Left)` pair for click-to-copy.
#[derive(Default)]
pub struct ClickTracking {
    /// Per-frame click-to-copy target registry. Populated during
    /// draw (via `&App`, which is why this is a `RefCell`), consumed
    /// by `handle_mouse`. Cleared at the top of every frame.
    pub registry: RefCell<ClickRegistry>,
    /// Tracks a pending click-to-copy gesture between `Down(Left)`
    /// and `Up(Left)`. A drag or an `Up` outside the original target
    /// cancels the gesture. Stored as `(col, row, kind, value)` in
    /// absolute frame coordinates.
    pub pending: Option<(u16, u16, ClickKind, String)>,
}

impl ClickTracking {
    /// Construct an empty click-tracking subsystem.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fire a click-to-copy action: write `value` to the clipboard
    /// via the OSC 52 + arboard backend and push a confirmation
    /// toast. The toast shows a short form of `value` based on
    /// `kind` so long URLs and file paths do not overflow the frame.
    ///
    /// Branches on the clipboard backend's return value so the toast
    /// truthfully reflects success vs. failure. See the "user-facing
    /// claim ignores available signal" rule in `CLAUDE.md`.
    ///
    /// Takes `&mut Toasts` explicitly rather than reaching through
    /// `App`, making the cross-subsystem borrow structural rather
    /// than a hidden dependency.
    pub fn fire_copy(toasts: &mut Toasts, value: &str, kind: ClickKind) {
        let ok = crate::side_effects::clipboard::copy(value);
        let short = short_display(value, kind);
        let text = if ok {
            format!("Copied: {short}")
        } else {
            format!("Copy failed: {short}")
        };
        toasts.push(text);
    }
}

#[cfg(test)]
mod tests {
    use super::{ClickKind, ClickTracking, Toasts};

    #[test]
    fn new_click_tracking_has_no_pending_gesture() {
        let ct = ClickTracking::new();
        assert!(ct.pending.is_none());
        // Registry starts empty-by-default: a hit test on any
        // coordinate returns None before anything is pushed.
        assert!(ct.registry.borrow().hit_test(0, 0).is_none());
    }

    #[test]
    fn pending_gesture_roundtrip() {
        // Happy path: arm a pending click, read it back, then clear
        // it. Matches the `Down(Left)` -> `Up(Left)` lifecycle in
        // `event::mouse::handle_mouse`.
        let mut ct = ClickTracking::new();
        ct.pending = Some((10, 5, ClickKind::Branch, "main".into()));
        assert_eq!(ct.pending.as_ref().unwrap().3, "main");
        ct.pending = None;
        assert!(ct.pending.is_none());
    }

    #[test]
    fn fire_copy_pushes_toast() {
        // Cross-subsystem invariant: fire_copy must reach the Toasts
        // subsystem so the user sees either "Copied: ..." or "Copy
        // failed: ...". The test doesn't assert which branch because
        // the clipboard backend under `cfg(test)` is a no-op; it
        // only checks that a toast landed at all.
        let _ct = ClickTracking::new();
        let mut toasts = Toasts::new();
        ClickTracking::fire_copy(&mut toasts, "feat/foo", ClickKind::Branch);
        assert_eq!(toasts.entries.len(), 1);
        let text = &toasts.entries[0].text;
        assert!(
            text.contains("feat/foo"),
            "toast should include the copied value, got {text:?}",
        );
    }
}
