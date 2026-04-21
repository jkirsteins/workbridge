//! `Metrics` subsystem - owns the latest aggregated metrics snapshot
//! and the receiver that feeds it from the background aggregator thread.
//!
//! `App` used to own `metrics_snapshot` and `metrics_rx` as two
//! sibling fields. Their
//! ownership is coupled (the snapshot is written in response to the
//! receiver producing a value, and a disconnect clears the receiver
//! while leaving the last snapshot untouched) so a small struct with
//! a `poll` method is a natural fit.
//!
//! The background aggregator thread lives in `crate::metrics`; it
//! produces a fresh `MetricsSnapshot` every ~60s. The UI consumes
//! whatever `Metrics::snapshot` holds; the Dashboard shows a
//! "computing..." placeholder when it is `None`.

use crate::metrics::MetricsSnapshot;

/// Owns the metrics snapshot cache and the background aggregator
/// receiver. A disconnect on the receiver drops it without touching
/// the cached snapshot so the Dashboard keeps rendering the last
/// known state.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Latest metrics snapshot produced by the background aggregator.
    /// `None` on startup until the first aggregation completes. The
    /// Dashboard renders a "computing..." placeholder while `None`.
    pub snapshot: Option<MetricsSnapshot>,
    /// Receiver for fresh `MetricsSnapshot` values from the background
    /// metrics aggregator thread. Polled (non-blocking `try_recv`)
    /// from the UI timer tick. See `docs/UI.md` "Blocking I/O
    /// Prohibition".
    ///
    /// `None` both before `set_rx` is called (pre-aggregator boot) and
    /// after the aggregator disconnects. Either way, `poll` becomes a
    /// no-op.
    pub rx: Option<crossbeam_channel::Receiver<MetricsSnapshot>>,
}

impl Metrics {
    /// Construct an empty metrics subsystem with no snapshot and no
    /// aggregator receiver. `main` installs the receiver via `set_rx`
    /// once the background aggregator thread has been spawned.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            snapshot: None,
            rx: None,
        }
    }

    /// Install the background aggregator's receiver. Called from
    /// `main` after `spawn_metrics_aggregator` returns.
    pub fn set_rx(&mut self, rx: crossbeam_channel::Receiver<MetricsSnapshot>) {
        self.rx = Some(rx);
    }

    /// Drain the aggregator channel, keeping only the latest snapshot.
    /// Called from the salsa timer tick. Non-blocking; never touches
    /// disk. The background thread produces a fresh snapshot every
    /// ~60s, so multiple pending values are rare but the drain-to-
    /// latest pattern keeps the dashboard truthful even if the
    /// consumer briefly lags.
    ///
    /// If the aggregator has disconnected (thread exited), the
    /// receiver is dropped so subsequent polls return immediately.
    /// The cached snapshot is deliberately left untouched so the
    /// dashboard keeps rendering the last value we know to be good.
    pub fn poll(&mut self) {
        let Some(rx) = self.rx.as_ref() else {
            return;
        };
        let mut latest: Option<MetricsSnapshot> = None;
        loop {
            match rx.try_recv() {
                Ok(snap) => latest = Some(snap),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.rx = None;
                    break;
                }
            }
        }
        if let Some(snap) = latest {
            self.snapshot = Some(snap);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::unbounded;

    use super::*;

    fn empty_snapshot() -> MetricsSnapshot {
        MetricsSnapshot::default()
    }

    #[test]
    fn new_metrics_has_no_snapshot_and_no_rx() {
        let m = Metrics::new();
        assert!(m.snapshot.is_none());
        assert!(m.rx.is_none());
    }

    #[test]
    fn poll_without_rx_is_noop() {
        // Empty-state path: until the aggregator has been booted,
        // `poll` must do nothing (no panic, no change to snapshot).
        let mut m = Metrics::new();
        m.poll();
        assert!(m.snapshot.is_none());
    }

    #[test]
    fn poll_drains_to_latest_snapshot() {
        // Happy-path: two pending snapshots get drained and the LAST
        // one wins. This matches the drain-to-latest contract so a
        // brief consumer lag cannot produce a stale dashboard.
        let mut m = Metrics::new();
        let (tx, rx) = unbounded();
        m.set_rx(rx);
        tx.send(empty_snapshot()).unwrap();
        tx.send(empty_snapshot()).unwrap();
        m.poll();
        assert!(m.snapshot.is_some());
    }

    #[test]
    fn poll_on_disconnect_drops_rx_but_keeps_snapshot() {
        // Error path: the aggregator exited. The receiver must be
        // dropped (so later polls return fast) but the cached
        // snapshot from a previous cycle stays so the dashboard
        // keeps rendering the last known state.
        let mut m = Metrics::new();
        let (tx, rx) = unbounded();
        m.set_rx(rx);
        tx.send(empty_snapshot()).unwrap();
        m.poll();
        assert!(m.snapshot.is_some());
        assert!(m.rx.is_some());

        drop(tx);
        m.poll();
        assert!(m.rx.is_none());
        assert!(m.snapshot.is_some(), "snapshot survives disconnect");
    }
}
