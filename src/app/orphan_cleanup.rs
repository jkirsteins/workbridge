//! `OrphanCleanup` subsystem - the completion-message channel for
//! background orphan-worktree cleanup threads.
//!
//! Stage 2.19 of the Phase 4 logical decomposition: `App` used to own
//! `orphan_cleanup_finished_tx` and `orphan_cleanup_finished_rx` as two
//! sibling fields (one always-open sender cloned into each background
//! thread, one receiver drained by the timer tick). Their ownership is
//! coupled - the pair is created together at `App::new`, never
//! replaced, and outlives every individual orphan-cleanup thread - so
//! grouping them in an owning struct drops the two sibling fields on
//! `App` to one.
//!
//! The heavy lifting (spawning the cleanup thread, accumulating
//! warnings, ending the status-bar activity) stays on `App` because it
//! reaches across `SharedServices`, `Activities`, and `Shell`. This
//! subsystem owns only the channel pair and the narrow `drain`
//! interface.

use super::OrphanCleanupFinished;

/// Owns the completion-message channel pair for background
/// `spawn_orphan_worktree_cleanup` threads. The sender is cloned into
/// every spawned closure; the receiver is drained by the timer tick.
#[derive(Debug)]
pub struct OrphanCleanup {
    /// Sender cloned into each background orphan-cleanup thread. The
    /// closure sends exactly one `OrphanCleanupFinished` when it
    /// finishes (success or failure).
    pub tx: crossbeam_channel::Sender<OrphanCleanupFinished>,
    /// Receiver paired with `tx`. Drained by `drain_pending` on every
    /// background-work tick.
    pub rx: crossbeam_channel::Receiver<OrphanCleanupFinished>,
}

impl OrphanCleanup {
    /// Construct a fresh channel pair. Called once from `App::new`
    /// and never replaced.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { tx, rx }
    }

    /// Drain all pending completion messages. Returns the list of
    /// `(ActivityId, warnings)` pairs the caller needs to apply:
    /// end each activity on the `Activities` subsystem, and collect
    /// any warnings for the status bar.
    ///
    /// Non-blocking. An empty channel returns an empty vector.
    pub fn drain_pending(&self) -> Vec<OrphanCleanupFinished> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            out.push(msg);
        }
        out
    }
}

impl Default for OrphanCleanup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ActivityId;

    fn fake_activity(id: u64) -> ActivityId {
        ActivityId(id)
    }

    #[test]
    fn new_channel_is_empty() {
        // Empty-state path: a freshly-built subsystem has no pending
        // messages.
        let oc = OrphanCleanup::new();
        assert!(oc.drain_pending().is_empty());
    }

    #[test]
    fn drain_pending_returns_queued_messages_in_order() {
        // Happy path: two background threads finish and each sends
        // one completion message. `drain_pending` must return both,
        // preserving send order so the status bar shows warnings in
        // a stable sequence.
        let oc = OrphanCleanup::new();
        oc.tx
            .send(OrphanCleanupFinished {
                activity: fake_activity(1),
                warnings: vec!["first".into()],
            })
            .unwrap();
        oc.tx
            .send(OrphanCleanupFinished {
                activity: fake_activity(2),
                warnings: vec!["second".into()],
            })
            .unwrap();
        let drained = oc.drain_pending();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].activity, fake_activity(1));
        assert_eq!(drained[1].activity, fake_activity(2));
        assert_eq!(drained[0].warnings, vec!["first"]);
        assert_eq!(drained[1].warnings, vec!["second"]);
    }

    #[test]
    fn drain_pending_is_idempotent_after_empty() {
        // Error / stable-state path: calling `drain_pending` twice in
        // a row when no new messages have arrived must return the
        // empty vector both times (no panic, no blocking).
        let oc = OrphanCleanup::new();
        assert!(oc.drain_pending().is_empty());
        assert!(oc.drain_pending().is_empty());
    }
}
