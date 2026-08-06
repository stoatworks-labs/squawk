//! The receive buffer: turns an arriving RTP stream into the aligned blocks the mix
//! engine requires.
//!
//! # Why this exists
//!
//! The engine's mix-minus is exact because it subtracts the same buffer it added. That
//! holds only if every endpoint's audio has been placed on the server's timeline before
//! the engine sees it. Packets do not arrive on a timeline — they arrive late, early,
//! out of order, twice, or not at all. This module is the thing that reconciles those.
//!
//! # Indexed by timestamp, not by sequence number
//!
//! The obvious design keys the ring on the RTP sequence number. This one keys on the
//! *timestamp*, because the timestamp is the media clock and the sequence number is
//! merely a counter. A sender that restarts, or one whose sequence wraps at a different
//! moment from its timestamp, will place audio at the wrong instant in a
//! sequence-indexed buffer while a timestamp-indexed one puts it exactly where it
//! belongs. Timestamps are compared with wrapping arithmetic, so the 2^32 rollover
//! (every ~24.8 hours at 48 kHz) is a non-event rather than a nightly glitch.
//!
//! # On drift
//!
//! When both ends are locked to the same PTP grandmaster there is no drift, and that is
//! the entire point of AES67. Drift appears only when a sender is *not* locked — so
//! rather than resampling unconditionally, this buffer measures its own fill level and
//! reports it. A steadily rising or falling depth is the signal that a sender is free-
//! running, which is a fact worth showing an operator rather than papering over.
//! Correcting it properly needs asynchronous sample rate conversion, which is not here
//! yet; [`JitterBuffer::drift`] is what tells you whether you need it.

use std::collections::VecDeque;

/// What happened to a pushed packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// Placed in the buffer, ready to be played out.
    Accepted,
    /// This timestamp is already buffered. Duplicates are normal on redundant
    /// (SMPTE 2022-7 style) paths and must not be mixed in twice.
    Duplicate,
    /// Arrived after its playout instant had passed, by this many samples. Its audio is
    /// gone; the only useful response is to widen the buffer.
    Late(u32),
    /// So far ahead that the sender must have restarted or jumped. The buffer resynced
    /// around the new timestamp rather than stalling until it caught up.
    Resync,
    /// The timestamp is not a whole number of packets from the playout point, which
    /// means the sender's packet time is not the one we were told to expect.
    Misaligned,
}

/// What a pull produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    /// Real audio.
    Filled,
    /// Nothing had arrived for this instant, so the gap was concealed. This is loss.
    Concealed,
    /// The buffer is filling to its target depth and has not reached the first packet
    /// yet. Output is silent.
    ///
    /// Deliberately distinct from [`Pull::Concealed`]: the pre-roll is the buffer's
    /// designed latency, not a dropout, and counting it as loss would put a burst of
    /// phantom packet loss on every stream start and every resync — exactly the reading
    /// an operator would use to go hunting for a network fault that is not there.
    Priming,
    /// Nothing has ever arrived; the buffer is still waiting to start. Output is silent.
    Idle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u64,
    pub duplicates: u64,
    pub late: u64,
    pub lost: u64,
    pub resyncs: u64,
    pub misaligned: u64,
    /// Packets currently buffered ahead of the playout point.
    pub depth: usize,
}

struct Slot {
    timestamp: u32,
    filled: bool,
    samples: Vec<f32>,
}

pub struct JitterBuffer {
    /// Samples per packet, per channel.
    block: usize,
    /// How many packets of delay to hold before playing out. This is the whole
    /// latency-versus-robustness trade, in one number.
    target_depth: usize,
    slots: Vec<Slot>,
    /// Timestamp of the next block to be played out.
    playout: u32,
    /// Ring index holding `playout`.
    ///
    /// Tracked explicitly rather than derived from the timestamp. Deriving it as
    /// `(timestamp / block) % capacity` looks equivalent and is not: a sender's
    /// timestamps are not aligned to any absolute grid, and at the 2^32 rollover that
    /// division jumps discontinuously, scattering a few milliseconds of audio into the
    /// wrong slots once every ~24.8 hours. Advancing an index alongside the playout
    /// point makes the wrap arithmetically invisible.
    playout_slot: usize,
    started: bool,
    /// Set by the first [`Pull::Filled`] after a start or resync. Until then, an empty
    /// slot is pre-roll rather than loss.
    primed: bool,
    /// Last block actually played, kept for concealment.
    last_good: Vec<f32>,
    /// How many blocks in a row have been concealed, so concealment can decay.
    concealed_run: u32,
    stats: JitterStats,
    /// Recent depth readings, for the drift estimate.
    depth_history: VecDeque<usize>,
}

/// Signed distance from `b` to `a` in samples, correct across the 2^32 wrap.
fn ts_diff(a: u32, b: u32) -> i64 {
    a.wrapping_sub(b) as i32 as i64
}

impl JitterBuffer {
    /// `block` is samples per packet; `target_depth` is packets of delay.
    ///
    /// At the AES67 default of 1 ms packets, a target depth of 2 costs 2 ms of latency
    /// and absorbs 2 ms of network delay variation — comfortable on a quiet wired LAN
    /// and marginal on a busy one.
    pub fn new(block: usize, target_depth: usize) -> Self {
        let target_depth = target_depth.max(1);
        // Capacity is not the latency — `target_depth` is. This is headroom, and it is
        // deliberately generous: if the receiving thread is descheduled for a few tens
        // of milliseconds, every packet that arrived meanwhile is sitting in the socket
        // queue and `poll` hands them over in one burst. A ring smaller than that burst
        // resyncs and throws away audio it had already successfully received. At the
        // 1 ms default, 64 slots absorbs a 64 ms stall for about 10 kB per stream.
        let capacity = (target_depth * 4).max(64);
        Self {
            block,
            target_depth,
            slots: (0..capacity)
                .map(|_| Slot { timestamp: 0, filled: false, samples: vec![0.0; block] })
                .collect(),
            playout: 0,
            playout_slot: 0,
            started: false,
            primed: false,
            last_good: vec![0.0; block],
            concealed_run: 0,
            stats: JitterStats::default(),
            depth_history: VecDeque::with_capacity(64),
        }
    }

    pub fn block_size(&self) -> usize {
        self.block
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Mean buffer depth over recent history, in packets.
    ///
    /// Sitting near `target_depth` means the sender's clock and ours agree. Drifting
    /// away from it in one direction means the sender is not locked to our grandmaster,
    /// and audio will eventually break up no matter how large the buffer is.
    pub fn drift(&self) -> Option<f32> {
        if self.depth_history.len() < 16 {
            return None;
        }
        let sum: usize = self.depth_history.iter().sum();
        Some(sum as f32 / self.depth_history.len() as f32 - self.target_depth as f32)
    }

    fn start_at(&mut self, timestamp: u32) {
        // Play out from target_depth packets *behind* this one, so there is buffered
        // audio ahead of the playout point from the very first pull.
        self.playout = timestamp.wrapping_sub((self.target_depth * self.block) as u32);
        self.playout_slot = 0;
        self.started = true;
        self.primed = false;
        for slot in &mut self.slots {
            slot.filled = false;
        }
    }

    /// Offer a received packet's audio, tagged with its RTP timestamp.
    pub fn push(&mut self, timestamp: u32, samples: &[f32]) -> Push {
        debug_assert_eq!(samples.len(), self.block);
        self.stats.received += 1;

        if !self.started {
            self.start_at(timestamp);
        }

        let delta = ts_diff(timestamp, self.playout);
        let block = self.block as i64;
        let span = (self.slots.len() * self.block) as i64;

        // Range check comes first, before alignment. A restarted sender picks a fresh
        // random timestamp, which will almost never sit on our block grid — checking
        // alignment first would report every restart as a permanent misalignment and
        // the stream would never recover. Out of range in *either* direction is a
        // restart: far behind means the sender's clock went backwards.
        if !(-span..span).contains(&delta) {
            self.stats.resyncs += 1;
            self.start_at(timestamp);
            let idx = (self.playout_slot + self.target_depth) % self.slots.len();
            let slot = &mut self.slots[idx];
            slot.timestamp = timestamp;
            slot.filled = true;
            slot.samples.copy_from_slice(samples);
            return Push::Resync;
        }

        if delta.rem_euclid(block) != 0 {
            self.stats.misaligned += 1;
            return Push::Misaligned;
        }

        if delta < 0 {
            self.stats.late += 1;
            return Push::Late(delta.unsigned_abs() as u32);
        }

        let ahead = (delta / block) as usize;
        if ahead >= self.slots.len() {
            // In range by timestamp but past the end of the ring: we have stalled.
            self.stats.resyncs += 1;
            self.start_at(timestamp);
            let idx = (self.playout_slot + self.target_depth) % self.slots.len();
            let slot = &mut self.slots[idx];
            slot.timestamp = timestamp;
            slot.filled = true;
            slot.samples.copy_from_slice(samples);
            return Push::Resync;
        }

        let idx = (self.playout_slot + ahead) % self.slots.len();
        let slot = &mut self.slots[idx];
        if slot.filled && slot.timestamp == timestamp {
            self.stats.duplicates += 1;
            return Push::Duplicate;
        }
        slot.timestamp = timestamp;
        slot.filled = true;
        slot.samples.copy_from_slice(samples);
        Push::Accepted
    }

    /// Take the next block onto the server's timeline. `out` must be `block` long.
    pub fn pull(&mut self, out: &mut [f32]) -> Pull {
        debug_assert_eq!(out.len(), self.block);

        if !self.started {
            out.fill(0.0);
            return Pull::Idle;
        }

        let idx = self.playout_slot;
        let hit = {
            let slot = &self.slots[idx];
            slot.filled && slot.timestamp == self.playout
        };

        let result = if hit {
            let slot = &mut self.slots[idx];
            out.copy_from_slice(&slot.samples);
            slot.filled = false;
            self.last_good.copy_from_slice(out);
            self.concealed_run = 0;
            self.primed = true;
            Pull::Filled
        } else if !self.primed {
            out.fill(0.0);
            Pull::Priming
        } else {
            self.stats.lost += 1;
            self.conceal(out);
            Pull::Concealed
        };

        self.playout = self.playout.wrapping_add(self.block as u32);
        self.playout_slot = (self.playout_slot + 1) % self.slots.len();
        self.record_depth();
        result
    }

    /// Fill a gap by repeating the last good block at a decaying level.
    ///
    /// Plain silence would put a step discontinuity at both edges of the hole, which
    /// clicks; repeating at full level for long enough turns into an obvious buzz. A
    /// halving per lost packet keeps a single lost packet inaudible and fades a real
    /// dropout to nothing within a few milliseconds.
    fn conceal(&mut self, out: &mut [f32]) {
        self.concealed_run += 1;
        let gain = 0.5f32.powi(self.concealed_run as i32);
        if gain < 0.01 {
            out.fill(0.0);
            self.last_good.fill(0.0);
            return;
        }
        for (o, s) in out.iter_mut().zip(&self.last_good) {
            *o = s * gain;
        }
    }

    fn record_depth(&mut self) {
        let depth = self
            .slots
            .iter()
            .filter(|s| s.filled && ts_diff(s.timestamp, self.playout) >= 0)
            .count();
        self.stats.depth = depth;
        if self.depth_history.len() == 64 {
            self.depth_history.pop_front();
        }
        self.depth_history.push_back(depth);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: usize = 48;

    fn dc(value: f32) -> Vec<f32> {
        vec![value; BLOCK]
    }

    /// Push `count` packets in order starting at `ts`, one distinct DC level each.
    fn push_run(jb: &mut JitterBuffer, ts: u32, count: u32) {
        for i in 0..count {
            let t = ts.wrapping_add(i * BLOCK as u32);
            jb.push(t, &dc(i as f32 + 1.0));
        }
    }

    fn pull(jb: &mut JitterBuffer) -> (Pull, f32) {
        let mut out = vec![0.0; BLOCK];
        let r = jb.pull(&mut out);
        (r, out[0])
    }

    #[test]
    fn plays_out_in_order_after_filling_to_the_target_depth() {
        let mut jb = JitterBuffer::new(BLOCK, 2);
        push_run(&mut jb, 1000, 4);

        // Playout started 2 packets behind the first arrival, so the first two pulls
        // are the pre-roll that creates the buffer's delay.
        assert_eq!(pull(&mut jb).0, Pull::Priming);
        assert_eq!(pull(&mut jb).0, Pull::Priming);

        for expected in 1..=4 {
            let (res, v) = pull(&mut jb);
            assert_eq!(res, Pull::Filled, "packet {expected}");
            assert_eq!(v, expected as f32);
        }

        // The pre-roll is designed latency, not a dropout.
        assert_eq!(jb.stats().lost, 0, "priming must not register as packet loss");
    }

    #[test]
    fn reordered_packets_land_at_the_right_instant() {
        let mut jb = JitterBuffer::new(BLOCK, 3);
        let base = 10_000u32;
        // Arrive 1, 3, 2 — the classic single-swap reorder.
        jb.push(base, &dc(1.0));
        jb.push(base + 2 * BLOCK as u32, &dc(3.0));
        assert_eq!(jb.push(base + BLOCK as u32, &dc(2.0)), Push::Accepted);

        for _ in 0..3 {
            pull(&mut jb); // pre-roll
        }
        assert_eq!(pull(&mut jb), (Pull::Filled, 1.0));
        assert_eq!(pull(&mut jb), (Pull::Filled, 2.0), "the swap must be undone");
        assert_eq!(pull(&mut jb), (Pull::Filled, 3.0));
    }

    #[test]
    fn a_duplicate_is_recognised_and_not_mixed_in_twice() {
        let mut jb = JitterBuffer::new(BLOCK, 2);
        jb.push(500, &dc(1.0));
        assert_eq!(jb.push(500, &dc(1.0)), Push::Duplicate);
        assert_eq!(jb.stats().duplicates, 1);
    }

    #[test]
    fn a_packet_that_arrives_after_its_moment_is_reported_late() {
        let mut jb = JitterBuffer::new(BLOCK, 1);
        let base = 2_000u32;
        push_run(&mut jb, base, 3);
        for _ in 0..4 {
            pull(&mut jb);
        }
        // Playout has moved past `base`; that audio can no longer be placed.
        assert_eq!(jb.push(base, &dc(9.0)), Push::Late(BLOCK as u32 * 3));
        assert_eq!(jb.stats().late, 1);
    }

    #[test]
    fn a_lost_packet_is_concealed_by_a_decaying_repeat() {
        let mut jb = JitterBuffer::new(BLOCK, 1);
        let base = 3_000u32;
        jb.push(base, &dc(1.0));
        // base + 1 block is deliberately never sent.
        jb.push(base + 2 * BLOCK as u32, &dc(1.0));

        pull(&mut jb); // pre-roll
        assert_eq!(pull(&mut jb), (Pull::Filled, 1.0));

        let (res, v) = pull(&mut jb);
        assert_eq!(res, Pull::Concealed);
        assert_eq!(v, 0.5, "one lost packet should fade, not drop to silence");
        assert_eq!(pull(&mut jb), (Pull::Filled, 1.0), "recovers on the next packet");
        assert_eq!(jb.stats().lost, 1);
    }

    #[test]
    fn a_long_dropout_decays_to_silence_rather_than_buzzing() {
        let mut jb = JitterBuffer::new(BLOCK, 1);
        jb.push(4_000, &dc(1.0));
        pull(&mut jb);
        pull(&mut jb);

        let mut last = 1.0;
        for _ in 0..8 {
            let (res, v) = pull(&mut jb);
            assert_eq!(res, Pull::Concealed);
            assert!(v < last || v == 0.0, "concealment must decay: {v} after {last}");
            last = v;
        }
        assert_eq!(last, 0.0, "a sustained dropout should end in silence");
    }

    #[test]
    fn a_sender_restart_resyncs_instead_of_stalling() {
        let mut jb = JitterBuffer::new(BLOCK, 2);
        push_run(&mut jb, 1_000, 3);
        for _ in 0..3 {
            pull(&mut jb);
        }

        // A restarted sender picks a fresh random timestamp, far outside the ring.
        let restart = 900_000u32;
        assert_eq!(jb.push(restart, &dc(7.0)), Push::Resync);
        assert_eq!(jb.stats().resyncs, 1);

        for _ in 0..2 {
            pull(&mut jb); // new pre-roll
        }
        assert_eq!(pull(&mut jb), (Pull::Filled, 7.0), "audio flows again after resync");
    }

    #[test]
    fn a_sender_with_the_wrong_packet_time_is_reported_not_mangled() {
        let mut jb = JitterBuffer::new(BLOCK, 2);
        jb.push(0, &dc(1.0));
        // Half a block out — a sender using a packet time we were not told about.
        assert_eq!(jb.push(BLOCK as u32 / 2, &dc(2.0)), Push::Misaligned);
        assert_eq!(jb.stats().misaligned, 1);
    }

    #[test]
    fn the_timestamp_wrap_at_2_to_the_32_is_a_non_event() {
        // At 48 kHz this happens every ~24.8 hours, so it will happen mid-show.
        let mut jb = JitterBuffer::new(BLOCK, 2);
        // Start just under the wrap, on a block boundary.
        let base = u32::MAX - (BLOCK as u32 * 3) + 1;
        push_run(&mut jb, base, 6);

        for _ in 0..2 {
            pull(&mut jb);
        }
        for expected in 1..=6 {
            let (res, v) = pull(&mut jb);
            assert_eq!(res, Pull::Filled, "packet {expected} across the wrap");
            assert_eq!(v, expected as f32);
        }
        assert_eq!(jb.stats().resyncs, 0, "the wrap must not look like a restart");
        assert_eq!(jb.stats().lost, 0);
    }

    #[test]
    fn depth_settles_at_the_target_when_the_clocks_agree() {
        let mut jb = JitterBuffer::new(BLOCK, 3);
        let mut ts = 50_000u32;
        // One packet in, one block out — a locked sender in steady state.
        for _ in 0..64 {
            jb.push(ts, &dc(1.0));
            ts = ts.wrapping_add(BLOCK as u32);
            pull(&mut jb);
        }
        let drift = jb.drift().expect("enough history");
        assert!(drift.abs() < 0.5, "locked clocks should not drift, got {drift}");
    }

    #[test]
    fn a_fast_sender_shows_up_as_positive_drift() {
        // A free-running sender putting in more than we take out. This is the reading
        // that says "this device is not locked to our grandmaster".
        let mut jb = JitterBuffer::new(BLOCK, 2);
        let mut ts = 70_000u32;
        for i in 0..64 {
            jb.push(ts, &dc(1.0));
            ts = ts.wrapping_add(BLOCK as u32);
            // Every 8th iteration, push an extra packet without pulling.
            if i % 8 == 0 {
                jb.push(ts, &dc(1.0));
                ts = ts.wrapping_add(BLOCK as u32);
            }
            pull(&mut jb);
        }
        let drift = jb.drift().expect("enough history");
        assert!(drift > 0.5, "a fast sender should read as positive drift, got {drift}");
    }

    #[test]
    fn a_scheduling_stall_does_not_cost_audio_that_already_arrived() {
        // The receive thread misses its slot for 40 ms; the packets are safe in the
        // socket queue and `poll` hands all 40 over at once. A ring sized to the
        // target depth would resync here and discard audio it had successfully
        // received — the buffer must be deep enough to take the burst.
        let mut jb = JitterBuffer::new(BLOCK, 2);
        let base = 600_000u32;
        push_run(&mut jb, base, 40);

        assert_eq!(jb.stats().resyncs, 0, "a 40 ms burst must not force a resync");

        for _ in 0..2 {
            assert_eq!(pull(&mut jb).0, Pull::Priming);
        }
        for expected in 1..=40 {
            let (res, v) = pull(&mut jb);
            assert_eq!(res, Pull::Filled, "packet {expected} after the stall");
            assert_eq!(v, expected as f32);
        }
        assert_eq!(jb.stats().lost, 0);
    }

    #[test]
    fn an_idle_buffer_outputs_silence_rather_than_noise() {
        let mut jb = JitterBuffer::new(BLOCK, 2);
        let mut out = vec![9.9; BLOCK];
        assert_eq!(jb.pull(&mut out), Pull::Idle);
        assert!(out.iter().all(|s| *s == 0.0));
    }
}
