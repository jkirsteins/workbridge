//! Toasts subsystem - transient top-right notifications.
//!
//! `App` used to own `toasts: Vec<Toast>` directly and carry
//! `push_toast` / `prune_toasts`
//! on `impl App`. This file owns both the data and the narrow API that
//! mutates it, so the rest of `App` never touches the vector itself.
//!
//! `App` holds `toasts: Toasts` and sibling subsystems talk to it
//! through `&mut Toasts`. Field-borrow splitting at the tick / event
//! dispatcher lets `Toasts` be borrowed disjointly from the rest of the
//! app state so, for example, `ClickTracking::fire_chrome_copy` can
//! both inspect the click registry and push a toast in the same call.

use std::time::Duration;

use super::Toast;

/// Owns the transient top-right notification queue.
///
/// Pruning is cheap and runs every timer tick so the vector never
/// grows unbounded. Newest toasts sit at the end of `entries`; render
/// code reads the vector in order.
#[derive(Debug, Default)]
pub struct Toasts {
    /// Active toasts. Newest at the end.
    pub entries: Vec<Toast>,
}

impl Toasts {
    /// Construct an empty toasts subsystem.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Show a transient top-right toast for ~2 seconds. Newest toasts
    /// stack at the top of the column. Multiple calls in quick
    /// succession produce a visible stack; each auto-dismisses
    /// independently. Called from `ClickTracking::fire_chrome_copy`
    /// and any other handler that wants to surface a short
    /// confirmation without hijacking the status bar.
    pub fn push(&mut self, text: String) {
        self.entries.push(Toast {
            text,
            expires_at: crate::side_effects::clock::instant_now() + Duration::from_secs(2),
        });
    }

    /// Drop any toasts whose deadline has passed. Cheap - called from
    /// the per-tick hook in `salsa::app_event`. Keeps the vector from
    /// growing unbounded and is the only thing that removes toasts.
    pub fn prune(&mut self) {
        let now = crate::side_effects::clock::instant_now();
        self.entries.retain(|t| t.expires_at > now);
    }

    /// True when no toasts are active. Used by the renderer to
    /// short-circuit the layout math when there is nothing to draw.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate active toasts in insertion order (oldest first).
    pub fn iter(&self) -> std::slice::Iter<'_, Toast> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::{Duration, Toasts};

    #[test]
    fn new_toasts_is_empty() {
        let toasts = Toasts::new();
        assert!(toasts.is_empty());
        assert_eq!(toasts.entries.len(), 0);
    }

    #[test]
    fn push_grows_then_prune_expired_empties_vector() {
        // Drive the mock clock forward past the 2s TTL. In test
        // builds `instant_now` reads the per-thread mock offset, and
        // `advance_mock_clock` moves that offset - so the toast TTL
        // expires deterministically without any wall-clock `sleep`.
        let mut toasts = Toasts::new();
        toasts.push("hello".into());
        toasts.push("world".into());
        assert_eq!(toasts.entries.len(), 2);

        // Before TTL elapses, prune is a no-op.
        toasts.prune();
        assert_eq!(toasts.entries.len(), 2);

        // Advance past the 2s TTL and prune drops both.
        crate::side_effects::clock::advance_mock_clock(Duration::from_secs(3));
        toasts.prune();
        assert!(toasts.is_empty());
    }

    #[test]
    fn iter_yields_entries_in_insertion_order() {
        let mut toasts = Toasts::new();
        toasts.push("first".into());
        toasts.push("second".into());
        let texts: Vec<&str> = toasts.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second"]);
    }

    #[test]
    fn push_on_empty_then_single_entry() {
        // Happy-path: the subsystem starts empty, a single push lands
        // a single entry, and the render vector surfaces it.
        let mut toasts = Toasts::new();
        toasts.push("only".into());
        assert_eq!(toasts.entries.len(), 1);
        let entry = toasts.iter().next().expect("one entry");
        assert_eq!(entry.text, "only");
    }
}
