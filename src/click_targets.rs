//! Click target registry for click-to-copy UI labels and left-panel
//! row selection.
//!
//! Each frame, renderers that draw an interactive chrome label push a
//! `ClickTarget::Copy` describing the absolute rect and the value to
//! copy when that rect is clicked. The work item list renderer pushes
//! `ClickTarget::WorkItemRow` once per visible selectable row.
//! `handle_mouse` consults the registry as a priority check before
//! falling back to geometric PTY classification: a `Copy` hit copies
//! the value and shows a toast; a `WorkItemRow` hit selects that row.
//!
//! The registry is cleared at the top of `draw_to_buffer` so stale
//! targets from the previous frame never leak. See
//! `docs/UI.md` "Interactive labels" and "Mouse Events" for the
//! user-facing conventions.

use ratatui_core::layout::Rect;

/// Which chrome field a copy click target represents. Used to pick
/// short-display formatting for the toast and (in tests) to
/// disambiguate which of several equally sized rects was hit.
///
/// This enum is deliberately chrome-copy-only. The structural "row
/// click" kind lives as a separate variant on `ClickTarget` so the
/// type system prevents a row-click payload from reaching code paths
/// (like `short_display` / `fire_chrome_copy`) that only make sense
/// for copyable labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickKind {
    /// The pull request URL value in the work item detail view.
    PrUrl,
    /// The branch name in the work item detail view.
    Branch,
    /// The repo path in the work item detail view.
    RepoPath,
    /// The work item title in the work item detail view.
    Title,
}

/// A single registered click target in absolute frame coordinates.
///
/// Two shapes: a copyable chrome label (rect + kind + value) and a
/// selectable work item list row (rect + display-list index). Keeping
/// them in separate variants lets the mouse handler dispatch on the
/// variant directly - no defensive `unreachable!` branches, no empty
/// placeholder `value` strings for row targets.
#[derive(Clone, Debug)]
pub enum ClickTarget {
    /// A chrome label that copies `value` to the clipboard when
    /// clicked (down-up on the same target).
    Copy {
        rect: Rect,
        kind: ClickKind,
        value: String,
    },
    /// A row in the left-panel work item list. `index` points into
    /// `App::display_list`. A left-click releases the selection onto
    /// this index.
    WorkItemRow { rect: Rect, index: usize },
}

impl ClickTarget {
    /// Rect in absolute frame coordinates, regardless of variant.
    pub const fn rect(&self) -> Rect {
        match self {
            Self::Copy { rect, .. } | Self::WorkItemRow { rect, .. } => *rect,
        }
    }
}

/// Per-frame registry of click targets. Populated during draw,
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

    /// Register a copyable chrome label. `rect` must be in absolute
    /// frame coordinates.
    pub fn push_copy(&mut self, rect: Rect, kind: ClickKind, value: String) {
        self.targets.push(ClickTarget::Copy { rect, kind, value });
    }

    /// Register a selectable work item list row. `rect` must be in
    /// absolute frame coordinates; `index` is a display-list index.
    pub fn push_work_item_row(&mut self, rect: Rect, index: usize) {
        self.targets.push(ClickTarget::WorkItemRow { rect, index });
    }

    /// Find the first registered target whose rect contains `(x, y)`.
    /// Linear scan - `N` is at most the visible row count plus a
    /// handful of chrome labels. Returns `None` if no target matches.
    pub fn hit_test(&self, x: u16, y: u16) -> Option<&ClickTarget> {
        self.targets.iter().find(|t| rect_contains(t.rect(), x, y))
    }
}

/// True iff the cell at `(x, y)` is inside `rect`. Half-open on the
/// right and bottom edges (matches ratatui's `Rect::right()` /
/// `Rect::bottom()` semantics: the cell at `rect.right()` is the first
/// cell *outside* the rect).
const fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
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
        reg.push_copy(rect(10, 5, 10, 1), ClickKind::PrUrl, "url".into());
        reg.push_copy(rect(30, 8, 10, 2), ClickKind::Branch, "branch".into());

        // Inside A.
        let hit = reg.hit_test(15, 5).expect("inside A");
        match hit {
            ClickTarget::Copy { kind, value, .. } => {
                assert_eq!(*kind, ClickKind::PrUrl);
                assert_eq!(value, "url");
            }
            ClickTarget::WorkItemRow { .. } => panic!("expected Copy, got WorkItemRow"),
        }

        // Left boundary of A is inclusive.
        let hit = reg.hit_test(10, 5).expect("left edge A");
        assert!(matches!(
            hit,
            ClickTarget::Copy {
                kind: ClickKind::PrUrl,
                ..
            }
        ));

        // Right boundary of A is exclusive: col 20 is the first column
        // outside the rect.
        assert!(reg.hit_test(20, 5).is_none(), "right edge exclusive");

        // Top boundary of B inclusive, bottom exclusive.
        let hit = reg.hit_test(35, 8).expect("top edge B");
        assert!(matches!(
            hit,
            ClickTarget::Copy {
                kind: ClickKind::Branch,
                ..
            }
        ));
        let hit = reg.hit_test(35, 9).expect("middle row B");
        assert!(matches!(
            hit,
            ClickTarget::Copy {
                kind: ClickKind::Branch,
                ..
            }
        ));
        assert!(reg.hit_test(35, 10).is_none(), "bottom edge exclusive");

        // Between the two rects: no hit.
        assert!(reg.hit_test(25, 5).is_none());
        assert!(reg.hit_test(15, 8).is_none());

        // Far outside.
        assert!(reg.hit_test(0, 0).is_none());
        assert!(reg.hit_test(100, 100).is_none());
    }

    #[test]
    fn work_item_row_hit_test() {
        let mut reg = ClickRegistry::default();
        reg.push_work_item_row(rect(0, 5, 30, 1), 7);
        let hit = reg.hit_test(15, 5).expect("inside row rect");
        match hit {
            ClickTarget::WorkItemRow { index, .. } => assert_eq!(*index, 7),
            ClickTarget::Copy { .. } => panic!("expected WorkItemRow, got Copy"),
        }
        assert!(reg.hit_test(15, 6).is_none(), "row is height 1");
    }

    #[test]
    fn clear_drops_all_targets() {
        let mut reg = ClickRegistry::default();
        reg.push_copy(rect(0, 0, 5, 1), ClickKind::Title, "x".into());
        reg.push_work_item_row(rect(0, 1, 5, 1), 0);
        assert!(reg.hit_test(2, 0).is_some());
        assert!(reg.hit_test(2, 1).is_some());
        reg.clear();
        assert!(reg.hit_test(2, 0).is_none());
        assert!(reg.hit_test(2, 1).is_none());
    }
}
