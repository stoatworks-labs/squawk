//! The disciplined clock, shared between the PTP thread and whoever needs the time.
//!
//! # Why PTP cannot live on the audio thread
//!
//! The obvious arrangement is to drain the PTP sockets once per audio tick. It works,
//! and it costs an order of magnitude of accuracy.
//!
//! Sync messages are timestamped when they are read, so polling every 1 ms quantises
//! every receive timestamp to the tick — on average half a tick late. The slave's own
//! Delay_Reqs go out the instant they are asked for, so *that* direction has no such
//! delay. PTP assumes the path is symmetric, cannot tell the difference, and reports
//! half the asymmetry as clock offset. Measured on loopback against a grandmaster
//! draining every 200 us, that was about 200 us of offset that was not there — enough
//! to sit permanently outside the lock threshold.
//!
//! So PTP runs on its own thread, polling far faster than audio rate, and publishes the
//! result here. The audio thread reads an atomic.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use crate::message::Timestamp;

/// A local monotonic timeline plus the correction that maps it onto PTP time.
///
/// Cheap to read from any thread and safe to read from an audio thread: two relaxed
/// atomic loads and no allocation, locking or syscall.
pub struct SharedClock {
    epoch_instant: Instant,
    epoch_nanos: u128,
    offset_nanos: AtomicI64,
    steps: AtomicU64,
    measurements: AtomicU64,
}

impl SharedClock {
    pub fn new(epoch_nanos: u128) -> Self {
        Self {
            epoch_instant: Instant::now(),
            epoch_nanos,
            offset_nanos: AtomicI64::new(0),
            steps: AtomicU64::new(0),
            measurements: AtomicU64::new(0),
        }
    }

    /// Undisciplined local time.
    pub fn local_now(&self) -> Timestamp {
        let total = self.epoch_nanos + self.epoch_instant.elapsed().as_nanos();
        Timestamp {
            seconds: (total / 1_000_000_000) as u64,
            nanos: (total % 1_000_000_000) as u32,
        }
    }

    /// PTP time as currently believed.
    pub fn now(&self) -> Timestamp {
        let total = self.epoch_nanos + self.epoch_instant.elapsed().as_nanos();
        let corrected =
            (total as i128 + self.offset_nanos.load(Ordering::Relaxed) as i128).max(0) as u128;
        Timestamp {
            seconds: (corrected / 1_000_000_000) as u64,
            nanos: (corrected % 1_000_000_000) as u32,
        }
    }

    /// PTP time as a flat nanosecond count, for arithmetic.
    pub fn now_nanos(&self) -> u128 {
        let total = self.epoch_nanos + self.epoch_instant.elapsed().as_nanos();
        (total as i128 + self.offset_nanos.load(Ordering::Relaxed) as i128).max(0) as u128
    }

    pub fn offset_nanos(&self) -> i64 {
        self.offset_nanos.load(Ordering::Relaxed)
    }

    /// Number of clock steps. A change means a timestamp discontinuity is legitimate.
    pub fn steps(&self) -> u64 {
        self.steps.load(Ordering::Relaxed)
    }

    /// Completed measurements, so a caller can tell "no grandmaster" from "not yet".
    pub fn measurements(&self) -> u64 {
        self.measurements.load(Ordering::Relaxed)
    }

    pub(crate) fn publish(&self, offset_nanos: i64, steps: u64, measurements: u64) {
        self.offset_nanos.store(offset_nanos, Ordering::Relaxed);
        self.steps.store(steps, Ordering::Relaxed);
        self.measurements.store(measurements, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn an_undisciplined_clock_reads_as_local_time() {
        let c = SharedClock::new(1_700_000_000_000_000_000);
        assert_eq!(c.offset_nanos(), 0);
        let a = c.now();
        std::thread::sleep(Duration::from_millis(20));
        let moved = c.now().diff_nanos(a);
        assert!((15_000_000..100_000_000).contains(&moved), "moved {moved} ns");
    }

    #[test]
    fn publishing_an_offset_shifts_the_time_it_reports() {
        let c = SharedClock::new(1_700_000_000_000_000_000);
        let before = c.now();
        c.publish(50_000_000, 1, 1);
        let after = c.now();
        let shift = after.diff_nanos(before);
        assert!(
            (49_000_000..52_000_000).contains(&shift),
            "expected a ~50 ms shift, got {shift} ns"
        );
        assert_eq!(c.steps(), 1);
        assert_eq!(c.measurements(), 1);
    }

    #[test]
    fn it_is_readable_from_another_thread() {
        use std::sync::Arc;
        let c = Arc::new(SharedClock::new(1_700_000_000_000_000_000));
        let reader = Arc::clone(&c);
        let t = std::thread::spawn(move || {
            let mut last = 0u128;
            for _ in 0..1000 {
                let n = reader.now_nanos();
                assert!(n >= last, "time went backwards");
                last = n;
            }
            reader.offset_nanos()
        });
        c.publish(-1_000_000, 0, 5);
        assert!(t.join().unwrap().abs() <= 1_000_000);
    }
}
