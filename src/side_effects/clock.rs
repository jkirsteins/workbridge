use std::time::{Duration, Instant, SystemTime};

#[cfg(test)]
use std::time::UNIX_EPOCH;

#[cfg(not(test))]
pub fn instant_now() -> Instant {
    Instant::now()
}

#[cfg(test)]
pub fn instant_now() -> Instant {
    *MOCK_INSTANT_BASE + Duration::from_nanos(mock_offset_nanos())
}

/// Returns the duration that has passed between `start` and the current
/// value of `instant_now()`.
///
/// Production builds delegate to `Instant::duration_since` against the
/// real monotonic clock (equivalent to `start.elapsed()`). Test builds
/// route through the mock clock so watchdog loops see the synthetic
/// time that `sleep` / `advance_mock_clock` have accumulated.
///
/// Bare `Instant::elapsed()` must NOT be used in test-reachable code -
/// it always reads `Instant::now()` (the real clock), which for a
/// `start` captured via `instant_now()` produces `Duration::ZERO` as
/// soon as `mock_offset_nanos()` exceeds the real elapsed time since
/// `MOCK_INSTANT_BASE` was initialized. That regression silently
/// disables CI watchdogs that guard against livelocks.
pub fn elapsed_since(start: Instant) -> Duration {
    instant_now().saturating_duration_since(start)
}

#[cfg(not(test))]
pub fn system_now() -> SystemTime {
    SystemTime::now()
}

#[cfg(test)]
pub fn system_now() -> SystemTime {
    UNIX_EPOCH
        + Duration::from_secs(MOCK_SYSTEM_BASE_SECS)
        + Duration::from_nanos(mock_offset_nanos())
}

#[cfg(not(test))]
pub fn sleep(dur: Duration) {
    std::thread::sleep(dur);
}

#[cfg(test)]
pub fn sleep(dur: Duration) {
    // Per-thread safety cap. `cargo test` runs tests in parallel
    // threads within a single process, so a process-global counter
    // would accumulate across unrelated tests and eventually trip
    // the cap with a misleading "livelock" panic on a test that was
    // actually fine. A thread-local counter keeps the intent
    // (catch a single test's polling loop that never converges)
    // without cross-test contamination.
    let calls = MOCK_SLEEP_CALLS.with(|c| {
        let next = c.get() + 1;
        c.set(next);
        next
    });
    if calls > MOCK_SLEEP_SAFETY_CAP {
        let current = std::thread::current();
        let name = current.name().unwrap_or("<unnamed>");
        panic!(
            "mock clock safety cap hit on thread '{}' - polling loop likely livelocked",
            name
        );
    }
    advance_mock_clock(dur);
    std::thread::yield_now();
}

#[cfg(test)]
pub fn advance_mock_clock(dur: Duration) {
    add_duration_to_mock_offset(dur);
}

#[cfg(test)]
use std::sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
const MOCK_SYSTEM_BASE_SECS: u64 = 1_700_000_000;
#[cfg(test)]
const MOCK_SLEEP_SAFETY_CAP: u64 = 100_000;

#[cfg(test)]
static MOCK_INSTANT_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);
#[cfg(test)]
static MOCK_OFFSET_NANOS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Per-thread safety counter for the mock `sleep`. Resets when
    /// the thread exits. `cargo test` spawns a fresh thread per test
    /// in its default harness, so each test gets its own counter.
    static MOCK_SLEEP_CALLS: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
fn mock_offset_nanos() -> u64 {
    MOCK_OFFSET_NANOS.load(Ordering::Relaxed)
}

#[cfg(test)]
fn add_duration_to_mock_offset(dur: Duration) {
    let nanos = u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX);
    let _ = MOCK_OFFSET_NANOS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(nanos))
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn mock_sleep_advances_instant_and_system_time() {
        let instant_before = super::instant_now();
        let system_before = super::system_now();

        super::sleep(Duration::from_millis(25));

        assert!(
            super::instant_now().duration_since(instant_before) >= Duration::from_millis(25),
            "sleep should advance the mock instant clock by at least the requested duration"
        );
        assert!(
            super::system_now()
                .duration_since(system_before)
                .expect("mock system clock should be monotonic")
                >= Duration::from_millis(25),
            "sleep should advance the mock system clock by at least the requested duration"
        );
    }

    #[test]
    fn manual_advance_moves_mock_clock_without_sleeping() {
        let instant_before = super::instant_now();
        let system_before = super::system_now();

        super::advance_mock_clock(Duration::from_secs(2));

        assert!(
            super::instant_now().duration_since(instant_before) >= Duration::from_secs(2),
            "manual advance should move the mock instant clock by at least the requested duration"
        );
        assert!(
            super::system_now()
                .duration_since(system_before)
                .expect("mock system clock should be monotonic")
                >= Duration::from_secs(2),
            "manual advance should move the mock system clock by at least the requested duration"
        );
    }

    #[test]
    fn elapsed_since_tracks_mock_clock_advance() {
        // Regression test for R2-F-1: watchdogs that capture a
        // `start = instant_now()` and then check `start.elapsed()`
        // against a real-time deadline would see 0 once the mock
        // offset exceeds real elapsed time (the normal case, because
        // mock advances synthetically). `elapsed_since` routes the
        // diff through the mock clock so the watchdog actually fires.
        let start = super::instant_now();
        super::advance_mock_clock(Duration::from_secs(5));
        assert!(
            super::elapsed_since(start) >= Duration::from_secs(5),
            "elapsed_since must reflect mock-clock advance, not real-time elapsed"
        );
    }
}
