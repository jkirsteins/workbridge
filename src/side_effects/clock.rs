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
    let calls = MOCK_SLEEP_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    if calls > MOCK_SLEEP_SAFETY_CAP {
        panic!("mock clock safety cap hit - polling loop likely livelocked");
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
const MOCK_SYSTEM_BASE_SECS: u64 = 1_700_000_000;
#[cfg(test)]
const MOCK_SLEEP_SAFETY_CAP: u64 = 100_000;

#[cfg(test)]
static MOCK_INSTANT_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);
#[cfg(test)]
static MOCK_OFFSET_NANOS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static MOCK_SLEEP_CALLS: AtomicU64 = AtomicU64::new(0);

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
}
