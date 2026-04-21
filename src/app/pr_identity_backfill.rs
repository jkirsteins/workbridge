//! `PrIdentityBackfill` subsystem - receiver + status-bar activity for
//! the one-time PR identity backfill thread spawned at startup.
//!
//! Stage 2.18a of the Phase 4 logical decomposition: `App` used to own
//! `pr_identity_backfill_rx` and `pr_identity_backfill_activity` as two
//! sibling `Option<_>` fields. Their lifetimes are coupled (the rx is
//! set the moment the background thread is spawned, alongside the
//! activity; both are cleared together on disconnect) so grouping
//! them in a small owning struct closes the "two separate Options
//! that must stay in lockstep" anti-pattern the structural-ownership
//! rule targets.
//!
//! The actual drain logic - which mutates `SharedServices::backend`
//! and `Shell::status_message` on success / error - stays on `App`
//! because it reaches across subsystems. This subsystem owns only the
//! pair + install / clear / take helpers.

use super::ActivityId;
use super::PrIdentityBackfillResult;

/// Owns the receiver and status-bar activity for the one-time
/// startup PR identity backfill.
///
/// Invariant: `rx.is_some() == activity.is_some()`. The two
/// `Option`s must flip together; this is the structural-ownership
/// reason the pair is a single subsystem.
#[derive(Debug, Default)]
pub struct PrIdentityBackfill {
    /// Receiver for background PR identity backfill results. `None`
    /// before the backfill thread has been spawned, `None` again
    /// after the thread exits and the receiver is drained.
    pub rx: Option<crossbeam_channel::Receiver<Result<PrIdentityBackfillResult, String>>>,
    /// Status-bar activity ID for the backfill. Kept so
    /// `drain_pending` can end it when the background thread
    /// finishes.
    pub activity: Option<ActivityId>,
}

impl PrIdentityBackfill {
    /// Construct an empty subsystem. No receiver installed, no
    /// activity pending.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rx: None,
            activity: None,
        }
    }

    /// Install the receiver + activity pair atomically. Called once
    /// from `salsa::app_init` after the background backfill thread
    /// has been spawned and the spinner activity has been started.
    pub fn install(
        &mut self,
        rx: crossbeam_channel::Receiver<Result<PrIdentityBackfillResult, String>>,
        activity: ActivityId,
    ) {
        self.rx = Some(rx);
        self.activity = Some(activity);
    }

    /// Called when the background thread has disconnected (closed
    /// its end of the channel). Clears the receiver and returns the
    /// stored activity id so the caller can end the matching
    /// status-bar spinner on its `Activities` subsystem.
    pub fn take_activity_on_disconnect(&mut self) -> Option<ActivityId> {
        self.rx = None;
        self.activity.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;

    #[test]
    fn new_is_inactive() {
        let b = PrIdentityBackfill::new();
        assert!(b.rx.is_none());
        assert!(b.activity.is_none());
    }

    #[test]
    fn install_sets_both_fields_atomically() {
        // Happy path: after `install`, both Options are Some. This
        // is the invariant the subsystem exists to enforce.
        let mut b = PrIdentityBackfill::new();
        let (_, rx) = unbounded();
        b.install(rx, ActivityId(42));
        assert!(b.rx.is_some());
        assert_eq!(b.activity, Some(ActivityId(42)));
    }

    #[test]
    fn take_activity_on_disconnect_clears_both_and_returns_id() {
        // Error path: the background thread exited. The caller
        // receives the activity ID to end on `Activities`, and
        // subsequent polls must see both Options clear so they
        // become no-ops.
        let mut b = PrIdentityBackfill::new();
        let (_, rx) = unbounded();
        b.install(rx, ActivityId(7));
        let got = b.take_activity_on_disconnect();
        assert_eq!(got, Some(ActivityId(7)));
        assert!(b.rx.is_none());
        assert!(b.activity.is_none());
    }

    #[test]
    fn take_activity_on_disconnect_is_idempotent() {
        // Empty-state / idempotency path: calling disconnect when
        // the subsystem was never installed must return None and
        // leave the state clean.
        let mut b = PrIdentityBackfill::new();
        assert!(b.take_activity_on_disconnect().is_none());
        assert!(b.rx.is_none());
        assert!(b.activity.is_none());
    }
}
