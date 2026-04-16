//! Click target registry for click-to-copy UI labels.
//!
//! Each frame, renderers that draw an interactive label push a
//! `ClickTarget` describing the absolute rect and the value that should
//! be copied when that rect is clicked. `handle_mouse` consults the
//! registry as a fallback after PTY classification: if a left-click
//! lands inside any registered rect, the associated value is copied to
//! the clipboard and a toast is shown.
//!
//! The registry is cleared at the top of `draw_to_buffer` so stale
//! targets from the previous frame never leak. See
//! `docs/UI.md` "Interactive labels" for the user-facing convention.

use ratatui_core::layout::Rect;

/// Which field a click target represents. Used to pick short-display
/// formatting for the toast and (in tests) to disambiguate which of
/// several equally sized rects was hit.
///
/// `Copy` cannot be derived because `WorkItemRow` carries a `usize`
/// payload - the pure-chrome variants above it would still be trivially
/// `Copy`, but the enum-level derive must agree on every variant. All
/// call sites pass `ClickKind` by value (via `.clone()`) or compare by
/// reference / value equality, so dropping `Copy` has no impact on
/// callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClickKind {
    /// The pull request URL value in the work item detail view.
    PrUrl,
    /// The branch name in the work item detail view.
    Branch,
    /// The repo path in the work item detail view.
    RepoPath,
    /// The work item title in the work item detail view.
    Title,
    /// A row in the left-panel work item list. Emitted once per
    /// visible row each frame, with `index` pointing into
    /// `App::display_list`. Left-click selects the row; the kind is
    /// dispatched separately from the chrome-copy targets above so
    /// `fire_chrome_copy` never sees it.
    WorkItemRow { index: usize },
}

/// A single registered click target: a rect in absolute frame
/// coordinates plus the untruncated value to copy when it is hit.
#[derive(Clone, Debug)]
pub struct ClickTarget {
    pub rect: Rect,
    pub value: String,
    pub kind: ClickKind,
}

/// Per-frame registry of click-to-copy targets. Populated during draw,
/// consumed (read-only) during `handle_mouse`. Cleared at the start of
/// every frame.
#[derive(Default)]
pub struct ClickRegistry {
    targets: Vec<ClickTarget>,
}

impl ClickRegistry {
    /// Discard all registered targets. Called at the top of
    /// `draw_to_buffer` before any render pushes happen.
    pub fn clear(&mut self) {
        self.targets.clear();
    }

    /// Register a new click target at the given rect with the given
    /// copy value. The rect must be in absolute frame coordinates - the
    /// same coordinate system `MouseEvent::column` / `row` uses.
    pub fn push(&mut self, rect: Rect, value: String, kind: ClickKind) {
        self.targets.push(ClickTarget { rect, value, kind });
    }

    /// Find the first registered target whose rect contains `(x, y)`.
    /// Linear scan - `N` is at most ~4 per frame in practice. Returns
    /// `None` if no target matches.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<&ClickTarget> {
        self.targets.iter().find(|t| rect_contains(t.rect, x, y))
    }
}

/// True iff the cell at `(x, y)` is inside `rect`. Half-open on the
/// right and bottom edges (matches ratatui's `Rect::right()` /
/// `Rect::bottom()` semantics: the cell at `rect.right()` is the first
/// cell *outside* the rect).
fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn hit_test_inside_outside_boundary() {
        let mut reg = ClickRegistry::default();
        // Two non-overlapping rects:
        //   A: cols 10..20 (width 10), row 5 (height 1)
        //   B: cols 30..40 (width 10), rows 8..10 (height 2)
        reg.push(rect(10, 5, 10, 1), "url".into(), ClickKind::PrUrl);
        reg.push(rect(30, 8, 10, 2), "branch".into(), ClickKind::Branch);

        // Inside A.
        let hit = reg.hit_test(15, 5).expect("inside A");
        assert_eq!(hit.kind, ClickKind::PrUrl);
        assert_eq!(hit.value, "url");

        // Left boundary of A is inclusive.
        let hit = reg.hit_test(10, 5).expect("left edge A");
        assert_eq!(hit.kind, ClickKind::PrUrl);

        // Right boundary of A is exclusive: col 20 is the first column
        // outside the rect.
        assert!(reg.hit_test(20, 5).is_none(), "right edge exclusive");

        // Top boundary of B inclusive, bottom exclusive.
        let hit = reg.hit_test(35, 8).expect("top edge B");
        assert_eq!(hit.kind, ClickKind::Branch);
        let hit = reg.hit_test(35, 9).expect("middle row B");
        assert_eq!(hit.kind, ClickKind::Branch);
        assert!(reg.hit_test(35, 10).is_none(), "bottom edge exclusive");

        // Between the two rects: no hit.
        assert!(reg.hit_test(25, 5).is_none());
        assert!(reg.hit_test(15, 8).is_none());

        // Far outside.
        assert!(reg.hit_test(0, 0).is_none());
        assert!(reg.hit_test(100, 100).is_none());
    }

    #[test]
    fn clear_drops_all_targets() {
        let mut reg = ClickRegistry::default();
        reg.push(rect(0, 0, 5, 1), "x".into(), ClickKind::Title);
        assert!(reg.hit_test(2, 0).is_some());
        reg.clear();
        assert!(reg.hit_test(2, 0).is_none());
    }
}
