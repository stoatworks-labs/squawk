//! Offset and delay measurement, the clock servo, and the media clock it disciplines.
//!
//! # What is being disciplined, and what is not
//!
//! **Not the system clock.** Slewing that needs root, and it would drag every other
//! process on the machine along with the audio network's idea of time. Instead this
//! keeps a mapping — an offset and a rate ratio — from the local monotonic clock to PTP
//! time, and derives RTP timestamps through it. Nothing outside squawk notices.
//!
//! # The assumption PTP cannot escape
//!
//! The offset calculation assumes the path is **symmetric**: that a packet takes as
//! long master-to-slave as slave-to-master. It has no way to measure the two separately
//! — with four timestamps and two unknowns (offset and one-way delay), symmetry is what
//! makes the system solvable at all.
//!
//! So any asymmetry lands directly in the offset, at exactly half its size, and no
//! amount of filtering removes it. This is why AES67 networks care about switch
//! configuration: a store-and-forward switch that queues one direction behind a burst
//! of audio and not the other injects an error PTP will faithfully track.

use crate::message::Timestamp;

/// A matched Sync (with its Follow_Up, if two-step) and the local time it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncSample {
    /// Master's transmit time: the Sync's originTimestamp for a one-step clock, or the
    /// Follow_Up's preciseOriginTimestamp for a two-step one.
    pub t1: Timestamp,
    /// Local receive time.
    pub t2: Timestamp,
    /// Correction field accumulated by transparent clocks, in nanoseconds.
    pub correction_nanos: i64,
}

/// A Delay_Req we sent and the Delay_Resp that came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelaySample {
    /// Local transmit time of the Delay_Req.
    pub t3: Timestamp,
    /// Master's receive time, from the Delay_Resp.
    pub t4: Timestamp,
    pub correction_nanos: i64,
}

/// The result of combining a Sync and a Delay exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// How far the local clock is ahead of the master, in nanoseconds.
    pub offset_nanos: i64,
    /// Estimated one-way delay, in nanoseconds.
    pub delay_nanos: i64,
}

/// Compute offset and mean path delay from the four timestamps.
///
/// ```text
/// mean delay = ((t2 - t1 - c_sync) + (t4 - t3 - c_resp)) / 2
/// offset     =  (t2 - t1 - c_sync) - mean delay
/// ```
pub fn measure(sync: SyncSample, delay: DelaySample) -> Measurement {
    let master_to_slave = sync.t2.diff_nanos(sync.t1) - sync.correction_nanos;
    let slave_to_master = delay.t4.diff_nanos(delay.t3) - delay.correction_nanos;

    let delay_nanos = (master_to_slave + slave_to_master) / 2;
    Measurement { offset_nanos: master_to_slave - delay_nanos, delay_nanos }
}

/// How well locked the servo is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockState {
    /// No usable measurements yet.
    Unlocked,
    /// Following, but the offset is still outside the locked threshold.
    Locking,
    /// Offset has stayed within threshold for long enough to trust.
    Locked,
}

/// A PI controller disciplining the local-to-PTP mapping.
pub struct Servo {
    /// Local-to-PTP offset in nanoseconds.
    offset_nanos: i64,
    /// Rate correction in parts per billion. Positive means the local clock runs slow
    /// and its intervals are being stretched to match the master.
    freq_ppb: f64,
    integral: f64,
    kp: f64,
    ki: f64,
    /// Offsets above this are stepped, not slewed.
    step_threshold_nanos: i64,
    /// Offsets within this count towards lock.
    lock_threshold_nanos: i64,
    good_samples: u32,
    required_good: u32,
    state: LockState,
    last_offset: i64,
    samples: u64,
    steps: u64,
}

impl Default for Servo {
    fn default() -> Self {
        Self::new()
    }
}

impl Servo {
    pub fn new() -> Self {
        Self {
            offset_nanos: 0,
            freq_ppb: 0.0,
            integral: 0.0,
            // Deliberately soft. A stiff servo tracks the *jitter* of software
            // timestamping rather than the master's actual time, and a media clock that
            // chases packet jitter sounds worse than one that lags a genuine change.
            kp: 0.3,
            ki: 0.05,
            // Beyond a millisecond, slewing would take minutes of audible drift.
            step_threshold_nanos: 1_000_000,
            lock_threshold_nanos: 100_000,
            good_samples: 0,
            required_good: 8,
            state: LockState::Unlocked,
            last_offset: 0,
            samples: 0,
            steps: 0,
        }
    }

    pub fn state(&self) -> LockState {
        self.state
    }
    /// The most recent *measured* offset — what the last exchange said the error was.
    ///
    /// Distinct from [`Servo::local_to_ptp_offset`], which is the correction currently
    /// being applied. Once locked the first is near zero and the second is not.
    pub fn last_offset_nanos(&self) -> i64 {
        self.last_offset
    }
    pub fn freq_ppb(&self) -> f64 {
        self.freq_ppb
    }
    pub fn samples(&self) -> u64 {
        self.samples
    }
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Feed a measurement. Returns the resulting lock state.
    pub fn update(&mut self, m: Measurement) -> LockState {
        self.samples += 1;
        self.last_offset = m.offset_nanos;

        if m.offset_nanos.abs() > self.step_threshold_nanos {
            // Step. Slewing a large offset means minutes of the media clock being
            // knowingly wrong, which is worse than one discontinuity — and at startup
            // the offset is arbitrary, so there is nothing to preserve by slewing.
            self.offset_nanos -= m.offset_nanos;
            self.integral = 0.0;
            self.freq_ppb = 0.0;
            self.good_samples = 0;
            self.steps += 1;
            self.state = LockState::Locking;
            return self.state;
        }

        let error = m.offset_nanos as f64;
        self.integral += error * self.ki;
        // Bound the integral: an unbounded one winds up during a network outage and
        // then drives the clock hard the wrong way for as long as it took to wind up.
        self.integral = self.integral.clamp(-1_000_000.0, 1_000_000.0);

        let correction = error * self.kp + self.integral;
        self.offset_nanos -= correction as i64;
        self.freq_ppb = -self.integral;

        // Hysteresis. Resetting the counter on a single bad sample makes lock a
        // coin-toss under software timestamping, where the occasional outlier is
        // guaranteed — the servo reports Locked, then Locking, then Locked again, and
        // anything downstream that gates on lock flaps with it. Credit one good sample,
        // debit several bad ones, and allow the count to run above the threshold so a
        // lone outlier costs margin rather than the lock itself.
        if m.offset_nanos.abs() <= self.lock_threshold_nanos {
            self.good_samples = (self.good_samples + 1).min(self.required_good * 2);
        } else {
            self.good_samples = self.good_samples.saturating_sub(4);
        }

        self.state = if self.good_samples >= self.required_good {
            LockState::Locked
        } else {
            LockState::Locking
        };
        self.state
    }

    /// Current local-to-PTP offset in nanoseconds.
    pub fn local_to_ptp_offset(&self) -> i64 {
        self.offset_nanos
    }
}

/// Turns PTP time into AES67 RTP timestamps.
///
/// AES67's `a=mediaclk:direct=0` means the RTP timestamp *is* the media clock with no
/// offset: it is PTP time counted in samples. Two senders locked to the same
/// grandmaster therefore produce identical timestamps for the same instant, which is
/// what lets a receiver line up streams from different devices without negotiating
/// anything.
#[derive(Debug, Clone, Copy)]
pub struct MediaClock {
    sample_rate: u32,
}

impl MediaClock {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate }
    }

    /// RTP timestamp for a PTP instant.
    pub fn rtp_timestamp(&self, ptp: Timestamp) -> u32 {
        let rate = self.sample_rate as u64;
        // Seconds first, then the sub-second part, so neither term overflows: seconds
        // are ~1.7e9 and the product with 48000 is ~8e13, comfortably inside u64.
        let from_seconds = ptp.seconds.wrapping_mul(rate);
        let from_nanos = (ptp.nanos as u64 * rate) / 1_000_000_000;
        from_seconds.wrapping_add(from_nanos) as u32
    }

    /// Samples per packet for a packet time in milliseconds.
    pub fn samples_per_ptime(&self, ptime_ms: f32) -> usize {
        ((self.sample_rate as f32) * ptime_ms / 1000.0).round() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: u64, nanos: u32) -> Timestamp {
        Timestamp::new(secs, nanos)
    }

    #[test]
    fn a_symmetric_path_gives_the_exact_offset_and_delay() {
        // One-way delay 5 us, slave clock 1 ms fast.
        //   t1 = 100.000000000  master transmits
        //   t2 = 100.001005000  slave receives: +5 us transit, +1 ms clock error
        //   t3 = 100.501005000  slave transmits (slave's own clock)
        //   t4 = 100.500010000  master receives: t3 less the 1 ms error, plus 5 us
        let sync = SyncSample {
            t1: ts(100, 0),
            t2: ts(100, 1_005_000),
            correction_nanos: 0,
        };
        let delay = DelaySample {
            t3: ts(100, 501_005_000),
            t4: ts(100, 500_010_000),
            correction_nanos: 0,
        };
        let m = measure(sync, delay);
        assert_eq!(m.delay_nanos, 5_000, "one-way delay");
        assert_eq!(m.offset_nanos, 1_000_000, "slave is 1 ms fast");
    }

    #[test]
    fn asymmetry_lands_in_the_offset_at_half_its_size() {
        // The fundamental limit: four timestamps, two unknowns, so symmetry is assumed.
        // Here master-to-slave takes 10 us and slave-to-master 2 us, with the clocks
        // actually in perfect agreement. PTP cannot see that and reports 4 us of error.
        let sync = SyncSample { t1: ts(0, 0), t2: ts(0, 10_000), correction_nanos: 0 };
        let delay = DelaySample { t3: ts(0, 100_000), t4: ts(0, 102_000), correction_nanos: 0 };
        let m = measure(sync, delay);

        assert_eq!(m.delay_nanos, 6_000, "mean of 10 us and 2 us");
        assert_eq!(
            m.offset_nanos, 4_000,
            "half the 8 us asymmetry, reported as clock error that is not there"
        );
    }

    #[test]
    fn the_correction_field_is_removed_from_both_directions() {
        // Transparent clocks add their residence time. A slave that ignores it reads an
        // offset inflated by however long the packet sat inside the switches.
        let sync = SyncSample { t1: ts(0, 0), t2: ts(0, 25_000), correction_nanos: 20_000 };
        let delay = DelaySample { t3: ts(0, 50_000), t4: ts(0, 75_000), correction_nanos: 20_000 };
        let m = measure(sync, delay);
        assert_eq!(m.delay_nanos, 5_000, "residence time should not look like path delay");
        assert_eq!(m.offset_nanos, 0);
    }

    #[test]
    fn a_large_initial_offset_is_stepped_not_slewed() {
        let mut servo = Servo::new();
        let state = servo.update(Measurement { offset_nanos: 500_000_000, delay_nanos: 5_000 });
        assert_eq!(state, LockState::Locking);
        assert_eq!(servo.steps(), 1);
        // The whole offset is taken out at once rather than crawled towards.
        assert_eq!(servo.local_to_ptp_offset(), -500_000_000);
    }

    #[test]
    fn the_servo_converges_on_a_steady_offset() {
        let mut servo = Servo::new();
        // A simulated clock 20 us fast, with the servo's correction fed back in.
        let mut true_offset = 20_000i64;
        for _ in 0..200 {
            let applied = servo.local_to_ptp_offset();
            let measured = true_offset + applied;
            servo.update(Measurement { offset_nanos: measured, delay_nanos: 5_000 });
            true_offset = 20_000;
        }
        assert_eq!(servo.state(), LockState::Locked);
        assert!(
            servo.last_offset_nanos().abs() < 1_000,
            "should have converged, residual {} ns",
            servo.last_offset_nanos()
        );
    }

    #[test]
    fn the_servo_rejects_timestamp_jitter_rather_than_chasing_it() {
        // Software timestamping is noisy. A stiff servo would track the noise and make
        // the media clock jump around more than the master ever does.
        let mut servo = Servo::new();
        let mut seed = 12345u64;
        let mut worst_after_settle = 0i64;

        for i in 0..400 {
            // Deterministic pseudo-random jitter of +/-15 us around a true offset of 0.
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let jitter = ((seed >> 33) % 30_001) as i64 - 15_000;
            let measured = servo.local_to_ptp_offset() + jitter;
            servo.update(Measurement { offset_nanos: measured, delay_nanos: 5_000 });
            if i > 200 {
                worst_after_settle = worst_after_settle.max(servo.local_to_ptp_offset().abs());
            }
        }
        assert!(
            worst_after_settle < 15_000,
            "servo amplified the input jitter to {worst_after_settle} ns"
        );
    }

    #[test]
    fn a_single_outlier_does_not_drop_lock() {
        // Software timestamping guarantees the occasional outlier. Without hysteresis
        // the servo flaps between Locked and Locking, and anything gating on lock
        // flaps with it.
        let mut servo = Servo::new();
        for _ in 0..40 {
            servo.update(Measurement { offset_nanos: 0, delay_nanos: 5_000 });
        }
        assert_eq!(servo.state(), LockState::Locked);

        servo.update(Measurement { offset_nanos: 400_000, delay_nanos: 5_000 });
        assert_eq!(servo.state(), LockState::Locked, "one outlier should cost margin, not lock");
    }

    #[test]
    fn sustained_bad_samples_do_drop_lock() {
        let mut servo = Servo::new();
        for _ in 0..40 {
            servo.update(Measurement { offset_nanos: 0, delay_nanos: 5_000 });
        }
        assert_eq!(servo.state(), LockState::Locked);

        for _ in 0..6 {
            servo.update(Measurement { offset_nanos: 400_000, delay_nanos: 5_000 });
        }
        assert_eq!(servo.state(), LockState::Locking, "a real loss of lock must be reported");
    }

    #[test]
    fn the_integral_is_bounded_so_an_outage_does_not_wind_it_up() {
        let mut servo = Servo::new();
        // A long run of consistent error, as if the master vanished mid-correction.
        for _ in 0..10_000 {
            servo.update(Measurement { offset_nanos: 90_000, delay_nanos: 5_000 });
        }
        assert!(
            servo.freq_ppb().abs() <= 1_000_000.0,
            "integral wound up to {}",
            servo.freq_ppb()
        );
    }

    #[test]
    fn rtp_timestamps_are_ptp_time_counted_in_samples() {
        let mc = MediaClock::new(48_000);
        // One second later is exactly 48000 samples later, whatever the absolute value.
        let a = mc.rtp_timestamp(ts(1_700_000_000, 0));
        let b = mc.rtp_timestamp(ts(1_700_000_001, 0));
        assert_eq!(b.wrapping_sub(a), 48_000);

        // And 1 ms later is exactly one packet.
        let c = mc.rtp_timestamp(ts(1_700_000_000, 1_000_000));
        assert_eq!(c.wrapping_sub(a), 48);
    }

    #[test]
    fn two_senders_on_the_same_grandmaster_agree_exactly() {
        // The property that lets a receiver align streams from different devices
        // without negotiating anything.
        let a = MediaClock::new(48_000);
        let b = MediaClock::new(48_000);
        let instant = ts(1_700_123_456, 789_000_000);
        assert_eq!(a.rtp_timestamp(instant), b.rtp_timestamp(instant));
    }

    #[test]
    fn the_rtp_timestamp_wraps_without_a_discontinuity_in_spacing() {
        let mc = MediaClock::new(48_000);
        // Find a PTP second whose sample count sits just below the 2^32 wrap.
        let secs = (u32::MAX as u64) / 48_000;
        let before = mc.rtp_timestamp(ts(secs, 0));
        let after = mc.rtp_timestamp(ts(secs + 1, 0));
        assert_eq!(
            after.wrapping_sub(before),
            48_000,
            "spacing must survive the wrap"
        );
    }

    #[test]
    fn packet_geometry_matches_the_aes67_default() {
        let mc = MediaClock::new(48_000);
        assert_eq!(mc.samples_per_ptime(1.0), 48);
        assert_eq!(mc.samples_per_ptime(0.125), 6);
        assert_eq!(mc.samples_per_ptime(4.0), 192);
    }
}
