//! Deriving RTP timestamps and the audio loop's cadence from PTP.
//!
//! # Why both, and not just the timestamps
//!
//! It is tempting to stamp packets from PTP and keep pacing the loop off the local
//! clock. That does not work, and the reason is worth stating because the failure is
//! slow enough to look like something else.
//!
//! If timestamps advance by exactly one block per tick and the ticks come from a local
//! crystal that is, say, 20 ppm fast, then the stream emits 48001 samples' worth of
//! timestamp per PTP second. The error accumulates at about one sample per second, so
//! after a minute and a half the timestamps are a couple of packets ahead of the time
//! they claim, and the sender has to jump them back. On a show that is a glitch every
//! couple of minutes, on every stream, with nothing in the logs to connect it to the
//! crystal.
//!
//! Pacing from PTP removes the accumulation at the source: the loop runs at the
//! grandmaster's rate, so the timestamps stay where they were put.

use std::time::{Duration, Instant};

use squawk_ptp::{MediaClock, Timestamp};

/// Produces the RTP timestamp for each block.
pub struct MediaTimeline {
    clock: MediaClock,
    block: u32,
    current: u32,
    started: bool,
    /// How far the counter may wander from PTP before it is snapped back. A last
    /// resort, not the normal correction path.
    max_drift_samples: u32,
    realignments: u64,
}

impl MediaTimeline {
    pub fn new(sample_rate: u32, block: u32) -> Self {
        Self {
            clock: MediaClock::new(sample_rate),
            block,
            current: 0,
            started: false,
            // A quarter of a second, which is enormous — deliberately.
            //
            // Snapping the timeline is a timestamp discontinuity, and receivers treat
            // those as a stream restart. The servo makes small corrections continuously
            // as it settles, and each one moves PTP time by a millisecond or two; a
            // tight threshold turns every one of them into a glitch on every stream.
            // Ordinary corrections are absorbed by the pacer instead, which simply ticks
            // fractionally faster or slower. This bound only catches the case where
            // something has gone properly wrong.
            max_drift_samples: sample_rate / 4,
            realignments: 0,
        }
    }

    pub fn realignments(&self) -> u64 {
        self.realignments
    }

    /// The timestamp for the block about to be sent.
    ///
    /// Advances by exactly one block per call, so packet spacing is uniform — receivers
    /// depend on that far more than on the absolute value. `ptp` re-anchors it; without
    /// it the counter free-runs and agrees with nobody but itself.
    ///
    /// `stepped` says the clock has just jumped, which is the one moment a timestamp
    /// discontinuity is correct: the old timeline referred to a different notion of
    /// time and there is nothing to preserve.
    pub fn next(&mut self, ptp: Option<Timestamp>, stepped: bool) -> u32 {
        let Some(ptp) = ptp else {
            if self.started {
                self.current = self.current.wrapping_add(self.block);
            } else {
                self.started = true;
            }
            return self.current;
        };

        let derived = self.clock.rtp_timestamp(ptp);
        if !self.started || stepped {
            self.current = derived;
            self.started = true;
            return self.current;
        }

        self.current = self.current.wrapping_add(self.block);

        // Signed distance, correct across the 2^32 wrap.
        let drift = self.current.wrapping_sub(derived) as i32;
        if drift.unsigned_abs() > self.max_drift_samples {
            tracing::warn!(
                drift_samples = drift,
                "media timeline realigned to PTP — the audio loop is not running at the \
                 grandmaster's rate"
            );
            self.current = derived;
            self.realignments += 1;
        }
        self.current
    }
}

/// Paces the audio loop off PTP time when it is available, and off the local monotonic
/// clock when it is not.
pub struct MediaPacer {
    period: Duration,
    period_nanos: u128,
    /// Next deadline in PTP nanoseconds. Unused in local mode.
    target_nanos: u128,
    started: bool,
    /// Local-clock fallback deadline.
    local_next: Instant,
    spin: bool,
    resyncs: u64,
}

/// How long before the deadline to stop sleeping and spin. `thread::sleep` overshoots a
/// 1 ms period by a large and variable fraction.
const SPIN_MARGIN: Duration = Duration::from_micros(400);

impl MediaPacer {
    pub fn new(period: Duration, spin: bool) -> Self {
        Self {
            period,
            period_nanos: period.as_nanos(),
            target_nanos: 0,
            started: false,
            local_next: Instant::now(),
            spin,
            resyncs: 0,
        }
    }

    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Wait for the next tick. `ptp_nanos` returns current PTP time, or `None` when
    /// there is no disciplined clock. Returns true if the deadline had already passed.
    pub fn wait<F>(&mut self, mut ptp_nanos: F) -> bool
    where
        F: FnMut() -> Option<u128>,
    {
        let Some(now) = ptp_nanos() else {
            return self.wait_local();
        };

        if !self.started {
            self.target_nanos = now + self.period_nanos;
            self.started = true;
        } else {
            self.target_nanos += self.period_nanos;
        }

        // A PTP step moves the whole timeline underneath us. Chasing the old deadline
        // would either spin for however long the step was, or fire thousands of ticks
        // back to back trying to catch up.
        let ahead = self.target_nanos as i128 - now as i128;
        if !(0..=1_000_000_000i128).contains(&ahead) {
            self.target_nanos = now + self.period_nanos;
            self.resyncs += 1;
            return true;
        }

        let remaining = Duration::from_nanos((ahead as u128).min(u64::MAX as u128) as u64);
        let margin = if self.spin { SPIN_MARGIN } else { Duration::ZERO };
        if remaining > margin {
            std::thread::sleep(remaining - margin);
        }
        if self.spin {
            loop {
                match ptp_nanos() {
                    Some(n) if n >= self.target_nanos => break,
                    // The clock vanishing mid-spin means PTP went away; stop waiting on
                    // a deadline that will never arrive.
                    None => break,
                    Some(_) => std::hint::spin_loop(),
                }
            }
        }
        false
    }

    fn wait_local(&mut self) -> bool {
        self.local_next += self.period;
        let now = Instant::now();
        if self.local_next <= now {
            // Give up the lost time rather than sprinting: sprinting turns one late
            // tick into a burst, which is worse for every receiver than the gap.
            self.local_next = now;
            return true;
        }
        let remaining = self.local_next - now;
        let margin = if self.spin { SPIN_MARGIN } else { Duration::ZERO };
        if remaining > margin {
            std::thread::sleep(remaining - margin);
        }
        while self.spin && Instant::now() < self.local_next {
            std::hint::spin_loop();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: u32 = 48;
    const RATE: u32 = 48_000;

    fn ptp(secs: u64, nanos: u32) -> Timestamp {
        Timestamp::new(secs, nanos)
    }

    #[test]
    fn without_ptp_the_timeline_free_runs_at_exactly_one_block_a_tick() {
        let mut tl = MediaTimeline::new(RATE, BLOCK);
        let first = tl.next(None, false);
        for i in 1..100u32 {
            assert_eq!(tl.next(None, false), first.wrapping_add(i * BLOCK));
        }
        assert_eq!(tl.realignments(), 0);
    }

    #[test]
    fn the_first_ptp_sample_anchors_the_timeline() {
        let mut tl = MediaTimeline::new(RATE, BLOCK);
        let anchor = ptp(1_700_000_000, 0);
        let expected = MediaClock::new(RATE).rtp_timestamp(anchor);
        assert_eq!(tl.next(Some(anchor), false), expected);
    }

    #[test]
    fn spacing_stays_uniform_while_ptp_agrees() {
        // Receivers care far more about even spacing than about the absolute value, so
        // ordinary agreement must never produce a correction.
        let mut tl = MediaTimeline::new(RATE, BLOCK);
        let mut nanos = 1_700_000_000u128 * 1_000_000_000;
        let mut previous = tl.next(Some(ptp(1_700_000_000, 0)), false);

        for _ in 0..5_000 {
            nanos += 1_000_000; // exactly one block at 1 ms
            let t = ptp((nanos / 1_000_000_000) as u64, (nanos % 1_000_000_000) as u32);
            let next = tl.next(Some(t), false);
            assert_eq!(next.wrapping_sub(previous), BLOCK, "spacing must not vary");
            previous = next;
        }
        assert_eq!(tl.realignments(), 0, "an agreeing clock must not need realigning");
    }

    #[test]
    fn a_loop_running_at_the_wrong_rate_is_realigned_and_reported() {
        // This is the failure that pacing from PTP exists to prevent: a local clock
        // running fast pushes the timeline ahead of the time it claims.
        let mut tl = MediaTimeline::new(RATE, BLOCK);
        let mut nanos = 1_700_000_000u128 * 1_000_000_000;
        tl.next(Some(ptp(1_700_000_000, 0)), false);

        // Ticks arrive every 1 ms by our reckoning but PTP only advances 0.9 ms.
        for _ in 0..40_000 {
            nanos += 900_000;
            let t = ptp((nanos / 1_000_000_000) as u64, (nanos % 1_000_000_000) as u32);
            tl.next(Some(t), false);
        }
        assert!(
            tl.realignments() > 0,
            "a sustained 10% rate error should eventually trip the safety bound"
        );
    }

    #[test]
    fn the_timeline_survives_the_rtp_timestamp_wrap() {
        let mut tl = MediaTimeline::new(RATE, BLOCK);
        // A PTP second whose sample count sits just below 2^32.
        let secs = (u32::MAX as u64) / RATE as u64;
        let mut nanos = secs as u128 * 1_000_000_000;
        let mut previous = tl.next(Some(ptp(secs, 0)), false);

        for _ in 0..3_000 {
            nanos += 1_000_000;
            let t = ptp((nanos / 1_000_000_000) as u64, (nanos % 1_000_000_000) as u32);
            let next = tl.next(Some(t), false);
            assert_eq!(next.wrapping_sub(previous), BLOCK, "spacing must survive the wrap");
            previous = next;
        }
        assert_eq!(tl.realignments(), 0, "the wrap must not look like drift");
    }

    #[test]
    fn the_pacer_falls_back_to_the_local_clock_without_ptp() {
        let mut pacer = MediaPacer::new(Duration::from_millis(2), false);
        let start = Instant::now();
        for _ in 0..10 {
            pacer.wait(|| None);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(15) && elapsed < Duration::from_millis(200),
            "10 ticks of 2 ms took {elapsed:?}"
        );
    }

    #[test]
    fn the_pacer_follows_ptp_time_rather_than_the_local_clock() {
        // A simulated PTP clock running at half real speed: ten 2 ms ticks should take
        // about 40 ms of wall time, not 20.
        let mut pacer = MediaPacer::new(Duration::from_millis(2), false);
        let origin = Instant::now();
        let base = 1_700_000_000u128 * 1_000_000_000;
        let start = Instant::now();
        for _ in 0..10 {
            pacer.wait(|| Some(base + origin.elapsed().as_nanos() / 2));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(30),
            "a half-speed clock should have stretched the ticks, took {elapsed:?}"
        );
    }

    #[test]
    fn a_ptp_step_resyncs_the_pacer_instead_of_stalling_or_sprinting() {
        let mut pacer = MediaPacer::new(Duration::from_millis(1), false);
        let base = 1_700_000_000u128 * 1_000_000_000;
        pacer.wait(|| Some(base));

        // The grandmaster steps us an hour forward. Chasing the old deadline would
        // fire millions of ticks back to back.
        let jumped = base + 3_600 * 1_000_000_000u128;
        let start = Instant::now();
        let late = pacer.wait(|| Some(jumped));
        assert!(late, "a step should be reported as a missed deadline");
        assert_eq!(pacer.resyncs(), 1);
        assert!(start.elapsed() < Duration::from_millis(50), "it must not have slept the step");
    }
}
