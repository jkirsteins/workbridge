//! Activities subsystem - status-bar spinners and in-flight indicators.
//!
//! `App` previously held `activity_counter`, `activities`,
//! `spinner_tick`, `structural_fetch_activity`, and
//! `pending_fetch_count` as separate fields, with `start_activity` /
//! `end_activity` / `current_activity` implemented directly on `impl
//! App`. That scatter made the spinner ownership story hard to reason
//! about: the invariant "exactly one spinner at a time (either the
//! user-action guard or the structural fetch) owns the indicator" is
//! enforced by code that had to reach across multiple App fields.
//!
//! This module makes the subsystem a single struct (`Activities`)
//! owning all five fields and exposing a narrow API. `App` now holds
//! `activities: Activities` and the individual fields are only
//! reachable through that owner.

/// A running status-bar activity: the spinner + short message shown
/// while a background task is in flight. Replicated here from the
/// legacy `types_01` layout so the new `Activities` struct can hold
/// a `Vec<Activity>` directly.
pub use super::Activity;
use super::ActivityId;

/// Owns the status-bar spinner state, the activity queue, and the
/// fetcher-spinner bookkeeping. `App` field-borrow splits this struct
/// out of `&mut self` so sibling subsystems can drive activity
/// lifecycle without holding a borrow on the rest of the app.
#[derive(Debug, Default)]
pub struct Activities {
    /// Monotonic counter for generating unique `ActivityId` values.
    pub counter: u64,
    /// Currently running activities. The last entry is displayed in the
    /// status bar. When empty, the normal `status_message` shows through.
    pub entries: Vec<Activity>,
    /// Spinner frame index, advanced on each 200ms timer tick when
    /// activities are present.
    pub spinner_tick: usize,
    /// Activity ID for an in-flight GitHub fetch that was NOT initiated
    /// via the `GithubRefresh` user-action guard - i.e. a structural
    /// fetcher restart. Either this field or the
    /// `UserActionKey::GithubRefresh` entry owns the spinner, never
    /// both. See `docs/UI.md` "Activity indicator placement".
    pub structural_fetch: Option<ActivityId>,
    /// Number of repos currently fetching. The activity spinner is
    /// shown while this is > 0 and cleared when it returns to 0.
    pub pending_fetch_count: usize,
}

impl Activities {
    /// Construct an empty activities subsystem.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counter: 0,
            entries: Vec::new(),
            spinner_tick: 0,
            structural_fetch: None,
            pending_fetch_count: 0,
        }
    }

    /// Start a new activity. Returns its ID for later removal.
    /// The most recently started activity is displayed in the status bar.
    pub fn start(&mut self, message: impl Into<String>) -> ActivityId {
        self.counter += 1;
        let id = ActivityId(self.counter);
        self.entries.push(Activity {
            id,
            message: message.into(),
        });
        id
    }

    /// End an activity by its ID. No-op if already ended.
    pub fn end(&mut self, id: ActivityId) {
        self.entries.retain(|a| a.id != id);
    }

    /// Returns the activity message to display, or None if idle.
    #[must_use]
    pub fn current(&self) -> Option<&str> {
        self.entries.last().map(|a| a.message.as_str())
    }

    /// True while at least one activity is in flight.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Advance the spinner frame index. Called every 200ms from the
    /// timer tick.
    pub const fn advance_spinner(&mut self) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
    }

    /// Number of active activities. Used by the header renderer
    /// to show a trailing count when more than one is in flight.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{Activities, ActivityId};

    #[test]
    fn new_activities_is_empty() {
        let a = Activities::new();
        assert!(a.is_empty());
        assert_eq!(a.counter, 0);
        assert_eq!(a.pending_fetch_count, 0);
        assert!(a.structural_fetch.is_none());
    }

    #[test]
    fn start_then_current_then_end_drops_entry() {
        let mut a = Activities::new();
        let id = a.start("fetching");
        assert_eq!(a.current(), Some("fetching"));
        assert!(!a.is_empty());
        a.end(id);
        assert!(a.is_empty());
        assert_eq!(a.current(), None);
    }

    #[test]
    fn end_unknown_id_is_noop() {
        // Error path: ending an id that never existed (or was already
        // ended) must not panic and must not affect other entries.
        let mut a = Activities::new();
        let keep = a.start("keep");
        a.end(ActivityId(9999));
        assert_eq!(a.entries.len(), 1);
        assert_eq!(a.current(), Some("keep"));
        a.end(keep);
        assert!(a.is_empty());
    }

    #[test]
    fn advance_spinner_wraps() {
        let mut a = Activities::new();
        a.spinner_tick = usize::MAX;
        a.advance_spinner();
        assert_eq!(a.spinner_tick, 0);
    }
}
