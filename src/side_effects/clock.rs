#[cfg(test)]
use std::time::UNIX_EPOCH;
use std::time::{Duration, Instant, SystemTime};

// Mock-clock scope note (tests only)
// ================================================================
// Both the per-thread safety counter (`MOCK_SLEEP_CALLS`) and the
// per-thread synthetic-time offset (`MOCK_OFFSET_NANOS`) live in
// `thread_local!` storage. `cargo test`'s default libtest harness
// spawns a fresh OS thread per test, so each test sees its own
// mock clock starting at zero offset with a zero sleep counter -
// two parallel tests cannot advance each other's clock and cannot
// trip each other's safety cap. The thread-local values persist
// for the lifetime of their thread; if a future test harness
// reuses OS threads across tests, each test will still observe a
// monotonic clock but offsets will accumulate. That matches the
// libtest default today and is the scope documented in
// `docs/TESTING.md` "Wall-clock in tests".

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
    // without cross-test contamination. The mock-time offset is also
    // thread-local for the same reason - see the module-level scope
    // note at the top of this file.
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
use std::cell::Cell;
#[cfg(test)]
use std::sync::LazyLock;

#[cfg(test)]
const MOCK_SYSTEM_BASE_SECS: u64 = 1_700_000_000;
#[cfg(test)]
const MOCK_SLEEP_SAFETY_CAP: u64 = 100_000;

#[cfg(test)]
static MOCK_INSTANT_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

#[cfg(test)]
thread_local! {
    /// Per-thread safety counter for the mock `sleep`. Resets when
    /// the thread exits. `cargo test` spawns a fresh thread per test
    /// in its default harness, so each test gets its own counter.
    static MOCK_SLEEP_CALLS: Cell<u64> = const { Cell::new(0) };

    /// Per-thread synthetic-time offset driving the mock `instant_now`
    /// and `system_now`. Must be thread-local so parallel tests in
    /// the default libtest harness (which spawns a fresh OS thread
    /// per test) see fully independent clocks. A process-global
    /// counter would let concurrent tests observe each other's time
    /// advances and break the "deterministic mock clock" contract
    /// documented in `docs/TESTING.md`. See also the module-level
    /// scope note above.
    static MOCK_OFFSET_NANOS: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
fn mock_offset_nanos() -> u64 {
    MOCK_OFFSET_NANOS.with(|c| c.get())
}

#[cfg(test)]
fn add_duration_to_mock_offset(dur: Duration) {
    let nanos = u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX);
    MOCK_OFFSET_NANOS.with(|c| {
        c.set(c.get().saturating_add(nanos));
    });
}

// Wall-clock-free bounded-receive helper for tests.
// ================================================================
// `bounded_recv` is the canonical replacement for the stdlib
// bounded-wait receive APIs (`Receiver::recv_timeout`,
// `Receiver::recv_deadline`, `Condvar::wait_timeout(_while)`,
// `Thread::park_timeout`), which internally read the real monotonic
// clock via `Condvar::wait_timeout` and are therefore forbidden by
// the wall-clock gate (see `docs/TESTING.md` "Wall-clock in tests"
// and the `hooks/pre-commit` side-effect check).
//
// The helper is channel-type-agnostic via the `PollableReceiver`
// trait; impls are provided for `std::sync::mpsc::Receiver` and
// `crossbeam_channel::Receiver`. Adding a new channel kind only
// requires another trait impl - the polling body stays here.
//
// Historical note: this module previously shipped without the
// helper, and three near-identical bounded-poll loops accreted in
// `src/fetcher.rs`, `src/app.rs::tests`, and `src/mcp.rs`. They
// drifted on iteration budget (1000 vs 6000) and on panic-message
// phrasing before being consolidated here.

#[cfg(test)]
pub(crate) enum TryRecv<T> {
    Ready(T),
    Empty,
    Disconnected,
}

#[cfg(test)]
pub(crate) trait PollableReceiver {
    type Item;
    fn poll_once(&self) -> TryRecv<Self::Item>;
}

#[cfg(test)]
impl<T> PollableReceiver for std::sync::mpsc::Receiver<T> {
    type Item = T;
    fn poll_once(&self) -> TryRecv<T> {
        match self.try_recv() {
            Ok(v) => TryRecv::Ready(v),
            Err(std::sync::mpsc::TryRecvError::Empty) => TryRecv::Empty,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => TryRecv::Disconnected,
        }
    }
}

#[cfg(test)]
impl<T> PollableReceiver for crossbeam_channel::Receiver<T> {
    type Item = T;
    fn poll_once(&self) -> TryRecv<T> {
        match self.try_recv() {
            Ok(v) => TryRecv::Ready(v),
            Err(crossbeam_channel::TryRecvError::Empty) => TryRecv::Empty,
            Err(crossbeam_channel::TryRecvError::Disconnected) => TryRecv::Disconnected,
        }
    }
}

/// Bounded `try_recv` polling loop for tests. Wall-clock-free
/// replacement for the stdlib `recv_timeout` family.
///
/// Polls `rx` up to `BUDGET` times with a 1ms mock-clock `sleep`
/// between polls. The mock-clock `sleep` is pure `yield_now` in
/// tests, so producer threads get ample real-time opportunity to
/// deliver a message while the mock clock advances deterministically.
/// A true livelock still trips the per-thread safety cap inside
/// `sleep` itself.
///
/// Works with both `std::sync::mpsc::Receiver` and
/// `crossbeam_channel::Receiver` via the `PollableReceiver` trait.
///
/// Panics with the provided `context` string if the channel
/// disconnects before delivering a message, or if the iteration
/// budget is exhausted.
#[cfg(test)]
pub(crate) fn bounded_recv<R: PollableReceiver>(rx: &R, context: &str) -> R::Item {
    const BUDGET: usize = 6_000;
    for _ in 0..BUDGET {
        match rx.poll_once() {
            TryRecv::Ready(v) => return v,
            TryRecv::Empty => sleep(Duration::from_millis(1)),
            TryRecv::Disconnected => {
                panic!("{context}: channel disconnected before delivering a message");
            }
        }
    }
    panic!("{context}: channel did not deliver a message within {BUDGET} mock-clock iterations");
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

    #[test]
    fn bounded_recv_returns_value_on_ready_channel() {
        // Sanity: if the channel already has a value, the helper
        // returns it on the first poll without burning the budget.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(42_u32).unwrap();
        assert_eq!(super::bounded_recv(&rx, "ready-mpsc"), 42);

        let (ctx, crx) = crossbeam_channel::unbounded();
        ctx.send("hi".to_string()).unwrap();
        assert_eq!(
            super::bounded_recv(&crx, "ready-crossbeam"),
            "hi".to_string()
        );
    }

    #[test]
    #[should_panic(expected = "disconnected")]
    fn bounded_recv_panics_on_disconnect() {
        // A dropped sender produces Disconnected on the next
        // try_recv; the helper must panic immediately rather than
        // spin to the budget.
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        drop(tx);
        super::bounded_recv(&rx, "disconnected-mpsc");
    }

    #[test]
    fn mock_clock_is_per_thread_independent() {
        // Regression test for R1-F-1: MOCK_OFFSET_NANOS used to be a
        // process-global AtomicU64, so every parallel test in the
        // cargo-test harness shared one counter. Two concurrent
        // tests advancing their mock clocks would observe each
        // other's advances, breaking the "deterministic mock clock"
        // contract documented in docs/TESTING.md. The fix makes the
        // offset a `thread_local! { Cell<u64> }` which each OS thread
        // initializes to zero. This test spawns two OS threads,
        // advances each by a distinct amount, and verifies the main
        // thread's own clock is untouched by either spawn and each
        // spawn saw only its own advance.
        use std::sync::{Arc, Barrier};

        // Pin the main-thread baseline BEFORE spawning so we can
        // assert it is unchanged after the children run.
        let main_before = super::instant_now();

        // Use a barrier so both child threads advance concurrently
        // rather than sequentially - sequential advances would mask
        // a process-global counter behind last-writer-wins timing.
        let barrier = Arc::new(Barrier::new(2));

        let b1 = Arc::clone(&barrier);
        let t1 = std::thread::spawn(move || {
            b1.wait();
            let start = super::instant_now();
            super::advance_mock_clock(Duration::from_secs(3));
            super::elapsed_since(start)
        });

        let b2 = Arc::clone(&barrier);
        let t2 = std::thread::spawn(move || {
            b2.wait();
            let start = super::instant_now();
            super::advance_mock_clock(Duration::from_secs(7));
            super::elapsed_since(start)
        });

        let elapsed_t1 = t1.join().expect("t1 must not panic");
        let elapsed_t2 = t2.join().expect("t2 must not panic");

        // Each child thread must see exactly its own advance, not
        // its own plus the other's. The upper bound is the strict
        // part of the test: a shared counter would deliver 10s to
        // whichever thread observed after the other's write.
        assert_eq!(
            elapsed_t1,
            Duration::from_secs(3),
            "thread 1 must observe only its own 3s advance - a shared \
             counter would bleed thread 2's 7s advance into this value"
        );
        assert_eq!(
            elapsed_t2,
            Duration::from_secs(7),
            "thread 2 must observe only its own 7s advance - a shared \
             counter would bleed thread 1's 3s advance into this value"
        );

        // Main-thread clock must not have been advanced by either
        // child. A shared counter would show 10s elapsed here.
        let main_elapsed = super::elapsed_since(main_before);
        assert_eq!(
            main_elapsed,
            Duration::ZERO,
            "main-thread mock clock must be untouched by child-thread \
             advances - saw {:?}, expected zero",
            main_elapsed
        );
    }
}
