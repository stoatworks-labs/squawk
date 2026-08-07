//! The slave-side protocol logic, with no sockets in it.
//!
//! Kept separate from [`crate::port`] so the message exchange can be driven
//! synthetically in tests. The sequence-matching rules below are the sort of thing that
//! appears to work against one vendor's grandmaster and quietly fails against another,
//! which is not something to discover on a network you cannot single-step.

use std::collections::BTreeMap;

use crate::bmca::{BestMaster, MasterDataset};
use crate::message::{Body, Message, MessageType, PortIdentity, Timestamp};
use crate::servo::{measure, DelaySample, LockState, Measurement, Servo, SyncSample};

use std::time::Instant;

/// Syncs held waiting for their Follow_Ups. At the AES67 profile's 8 Sync/s this is a
/// second of backlog, which is far more than a healthy system ever needs.
const MAX_PENDING_SYNCS: usize = 8;

/// Something the caller needs to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The grandmaster changed. The caller should expect a step and, if it cares,
    /// tell the operator.
    MasterChanged,
    /// A complete measurement landed and the servo moved.
    Measured(LockState),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SlaveStats {
    pub announces: u64,
    pub syncs: u64,
    pub follow_ups: u64,
    pub delay_resps: u64,
    /// Messages from a domain we are not on.
    pub wrong_domain: u64,
    /// Sync or Delay_Resp that arrived without a partner to match.
    pub unmatched: u64,
}

pub struct SlaveState {
    identity: PortIdentity,
    domain: u8,
    bmca: BestMaster,
    servo: Servo,
    master: Option<MasterDataset>,

    /// Receive times of Syncs still waiting for their Follow_Ups, by sequence id.
    ///
    /// A map rather than a single slot, because `poll` drains the event port (Syncs)
    /// completely before the general port (Follow_Ups) — it has to, or the general
    /// port's processing time lands in the Syncs' timestamps. With one slot, any
    /// backlog leaves only the newest Sync pending and every earlier Follow_Up is
    /// discarded as unmatched, so a moment of scheduling pressure costs most of the
    /// measurements rather than one.
    pending_sync: BTreeMap<u16, Timestamp>,
    /// The most recent completed Sync pairing.
    last_sync: Option<SyncSample>,
    /// Sequence and local transmit time of the Delay_Req in flight.
    pending_delay: Option<(u16, Timestamp)>,

    last_measurement: Option<Measurement>,
    stats: SlaveStats,
}

impl SlaveState {
    pub fn new(identity: PortIdentity, domain: u8) -> Self {
        Self {
            identity,
            domain,
            bmca: BestMaster::new(),
            servo: Servo::new(),
            master: None,
            pending_sync: BTreeMap::new(),
            last_sync: None,
            pending_delay: None,
            last_measurement: None,
            stats: SlaveStats::default(),
        }
    }

    pub fn servo(&self) -> &Servo {
        &self.servo
    }
    pub fn master(&self) -> Option<MasterDataset> {
        self.master
    }
    pub fn stats(&self) -> SlaveStats {
        self.stats
    }
    pub fn last_measurement(&self) -> Option<Measurement> {
        self.last_measurement
    }
    pub fn identity(&self) -> PortIdentity {
        self.identity
    }
    pub fn masters_seen(&self) -> usize {
        self.bmca.known_masters()
    }

    /// Whether a Delay_Req would be useful yet.
    ///
    /// A delay measurement can only be combined with a completed Sync pairing, so
    /// asking before the first Sync has landed produces a Delay_Resp that has to be
    /// discarded — and it is discarded as "unmatched", which is the counter an operator
    /// would reasonably read as a fault.
    pub fn ready_for_delay_req(&self) -> bool {
        self.master.is_some() && self.last_sync.is_some()
    }

    /// Handle a received message. `local_rx` is when it arrived by our clock.
    pub fn on_message(&mut self, msg: &Message, local_rx: Timestamp, now: Instant) -> Option<Event> {
        // A domain is an independent PTP network sharing one wire. Acting on another
        // domain's Sync would discipline our clock to a master we are not following.
        if msg.header.domain != self.domain {
            self.stats.wrong_domain += 1;
            return None;
        }

        match (&msg.body, msg.header.message_type) {
            (Body::Announce(a), _) => {
                self.stats.announces += 1;
                let dataset = MasterDataset::from_announce(&msg.header, a);
                self.bmca
                    .observe(dataset, msg.header.log_message_interval, now);
                self.reselect(now)
            }

            (Body::Sync { origin }, MessageType::Sync) => {
                if !self.is_from_master(msg) {
                    return None;
                }
                self.stats.syncs += 1;
                if msg.header.flags.two_step {
                    // The originTimestamp of a two-step Sync is not the transmit time;
                    // it is a placeholder. Using it would build the whole measurement
                    // on a number the master explicitly said to ignore.
                    self.pending_sync.insert(msg.header.sequence_id, local_rx);
                    // Bounded: a master whose Follow_Ups never arrive must not grow this
                    // without limit.
                    while self.pending_sync.len() > MAX_PENDING_SYNCS {
                        let oldest = *self.pending_sync.keys().next().expect("non-empty");
                        self.pending_sync.remove(&oldest);
                        self.stats.unmatched += 1;
                    }
                } else {
                    self.last_sync = Some(SyncSample {
                        t1: *origin,
                        t2: local_rx,
                        correction_nanos: msg.header.correction_nanos(),
                    });
                    self.pending_sync.clear();
                }
                None
            }

            (Body::FollowUp { precise_origin }, _) => {
                if !self.is_from_master(msg) {
                    return None;
                }
                self.stats.follow_ups += 1;
                // Match on sequence id, not on arrival order. Follow_Ups can be
                // reordered against the next Sync on a busy network, and pairing by
                // arrival then attributes one Sync's receive time to another's transmit
                // time — an error the size of the Sync interval.
                match self.pending_sync.remove(&msg.header.sequence_id) {
                    Some(t2) => {
                        self.last_sync = Some(SyncSample {
                            t1: *precise_origin,
                            t2,
                            correction_nanos: msg.header.correction_nanos(),
                        });
                        // Anything older will never be paired now.
                        self.pending_sync.retain(|seq, _| {
                            msg.header.sequence_id.wrapping_sub(*seq) > u16::MAX / 2
                        });
                    }
                    None => {
                        self.stats.unmatched += 1;
                    }
                }
                None
            }

            (Body::DelayResp { receive, requesting }, _) => {
                if !self.is_from_master(msg) {
                    return None;
                }
                // Delay_Resp is multicast, so every slave on the domain sees every
                // other slave's responses. Without this check we would take another
                // device's measurement as our own.
                if *requesting != self.identity {
                    return None;
                }
                self.stats.delay_resps += 1;

                let Some((seq, t3)) = self.pending_delay else {
                    self.stats.unmatched += 1;
                    return None;
                };
                if seq != msg.header.sequence_id {
                    self.stats.unmatched += 1;
                    return None;
                }
                self.pending_delay = None;

                let Some(sync) = self.last_sync else {
                    self.stats.unmatched += 1;
                    return None;
                };

                let m = measure(
                    sync,
                    DelaySample {
                        t3,
                        t4: *receive,
                        correction_nanos: msg.header.correction_nanos(),
                    },
                );
                self.last_measurement = Some(m);
                Some(Event::Measured(self.servo.update(m)))
            }

            _ => None,
        }
    }

    /// Record that a Delay_Req went out, with the local time it left.
    pub fn on_delay_req_sent(&mut self, sequence_id: u16, t3: Timestamp) {
        self.pending_delay = Some((sequence_id, t3));
    }

    /// Drop masters that have gone quiet and re-run selection.
    pub fn tick(&mut self, now: Instant) -> Option<Event> {
        if self.bmca.expire(now) > 0 {
            return self.reselect(now);
        }
        None
    }

    fn reselect(&mut self, _now: Instant) -> Option<Event> {
        let best = self.bmca.best();
        let changed = match (&self.master, &best) {
            (Some(a), Some(b)) => a.grandmaster != b.grandmaster || a.sender != b.sender,
            (None, Some(_)) | (Some(_), None) => true,
            (None, None) => false,
        };
        self.master = best;
        if changed {
            // Everything in flight belonged to the old master's timeline.
            self.pending_sync.clear();
            self.last_sync = None;
            self.pending_delay = None;
            Some(Event::MasterChanged)
        } else {
            None
        }
    }

    fn is_from_master(&self, msg: &Message) -> bool {
        self.master
            .map(|m| m.sender == msg.header.source)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        Announce, ClockIdentity, ClockQuality, Flags, Header, HEADER_LEN, TIMESTAMP_LEN,
    };

    fn ts(s: u64, n: u32) -> Timestamp {
        Timestamp::new(s, n)
    }

    fn master_port() -> PortIdentity {
        PortIdentity { clock: ClockIdentity([1, 0, 0, 0xFF, 0xFE, 0, 0, 1]), port: 1 }
    }

    fn our_port() -> PortIdentity {
        PortIdentity { clock: ClockIdentity([9, 9, 9, 0xFF, 0xFE, 9, 9, 9]), port: 1 }
    }

    fn header(kind: MessageType, seq: u16, two_step: bool, source: PortIdentity) -> Header {
        Header {
            message_type: kind,
            domain: 0,
            flags: Flags { two_step, ..Default::default() },
            correction_subnanos: 0,
            source,
            sequence_id: seq,
            log_message_interval: 0,
        }
    }

    fn announce_msg(source: PortIdentity, priority1: u8) -> Message {
        Message {
            header: header(MessageType::Announce, 1, false, source),
            body: Body::Announce(Box::new(Announce {
                current_utc_offset: 37,
                grandmaster_priority1: priority1,
                grandmaster_quality: ClockQuality {
                    class: 6,
                    accuracy: 0x21,
                    offset_scaled_log_variance: 0x436A,
                },
                grandmaster_priority2: 128,
                grandmaster_identity: source.clock,
                steps_removed: 0,
                time_source: 0x20,
            })),
        }
    }

    /// Drive a full two-step exchange. Returns the resulting measurement.
    fn full_exchange(
        state: &mut SlaveState,
        seq: u16,
        t1: Timestamp,
        t2: Timestamp,
        t3: Timestamp,
        t4: Timestamp,
    ) -> Option<Measurement> {
        let now = Instant::now();
        state.on_message(
            &Message {
                header: header(MessageType::Sync, seq, true, master_port()),
                body: Body::Sync { origin: Timestamp::default() },
            },
            t2,
            now,
        );
        state.on_message(
            &Message {
                header: header(MessageType::FollowUp, seq, false, master_port()),
                body: Body::FollowUp { precise_origin: t1 },
            },
            ts(0, 0),
            now,
        );
        state.on_delay_req_sent(seq, t3);
        state.on_message(
            &Message {
                header: header(MessageType::DelayResp, seq, false, master_port()),
                body: Body::DelayResp { receive: t4, requesting: state.identity() },
            },
            ts(0, 0),
            now,
        );
        state.last_measurement()
    }

    fn locked_on_master() -> SlaveState {
        let mut s = SlaveState::new(our_port(), 0);
        s.on_message(&announce_msg(master_port(), 128), ts(0, 0), Instant::now());
        s
    }

    #[test]
    fn a_full_two_step_exchange_produces_the_right_offset() {
        let mut s = locked_on_master();
        let m = full_exchange(
            &mut s,
            1,
            ts(100, 0),
            ts(100, 1_005_000),
            ts(100, 501_005_000),
            ts(100, 500_010_000),
        )
        .expect("measurement");
        assert_eq!(m.delay_nanos, 5_000);
        assert_eq!(m.offset_nanos, 1_000_000);
        assert_eq!(s.stats().syncs, 1);
        assert_eq!(s.stats().follow_ups, 1);
        assert_eq!(s.stats().delay_resps, 1);
    }

    #[test]
    fn a_one_step_sync_needs_no_follow_up() {
        let mut s = locked_on_master();
        let now = Instant::now();
        s.on_message(
            &Message {
                header: header(MessageType::Sync, 1, false, master_port()),
                body: Body::Sync { origin: ts(100, 0) },
            },
            ts(100, 1_005_000),
            now,
        );
        s.on_delay_req_sent(1, ts(100, 501_005_000));
        s.on_message(
            &Message {
                header: header(MessageType::DelayResp, 1, false, master_port()),
                body: Body::DelayResp { receive: ts(100, 500_010_000), requesting: our_port() },
            },
            ts(0, 0),
            now,
        );
        assert_eq!(s.last_measurement().unwrap().offset_nanos, 1_000_000);
    }

    #[test]
    fn a_follow_up_for_a_different_sync_is_not_paired_with_this_one() {
        // Pairing by arrival order rather than sequence id attributes one Sync's
        // receive time to another's transmit time — an error the size of the Sync
        // interval, which at 8/s is 125 ms.
        let mut s = locked_on_master();
        let now = Instant::now();
        s.on_message(
            &Message {
                header: header(MessageType::Sync, 5, true, master_port()),
                body: Body::Sync { origin: Timestamp::default() },
            },
            ts(100, 1_005_000),
            now,
        );
        s.on_message(
            &Message {
                header: header(MessageType::FollowUp, 6, false, master_port()),
                body: Body::FollowUp { precise_origin: ts(100, 0) },
            },
            ts(0, 0),
            now,
        );
        assert_eq!(s.stats().unmatched, 1, "mismatched sequence should not pair");

        // And with nothing paired, a Delay_Resp cannot complete a measurement.
        s.on_delay_req_sent(5, ts(100, 501_005_000));
        s.on_message(
            &Message {
                header: header(MessageType::DelayResp, 5, false, master_port()),
                body: Body::DelayResp { receive: ts(100, 500_010_000), requesting: our_port() },
            },
            ts(0, 0),
            now,
        );
        assert!(s.last_measurement().is_none());
    }

    #[test]
    fn another_slaves_delay_resp_is_ignored() {
        // Delay_Resp is multicast: every slave on the domain sees every other's.
        // Taking one as our own would apply somebody else's path delay to our clock.
        let mut s = locked_on_master();
        let now = Instant::now();
        s.on_message(
            &Message {
                header: header(MessageType::Sync, 1, false, master_port()),
                body: Body::Sync { origin: ts(100, 0) },
            },
            ts(100, 1_005_000),
            now,
        );
        s.on_delay_req_sent(1, ts(100, 501_005_000));

        let someone_else = PortIdentity { clock: ClockIdentity([7; 8]), port: 1 };
        s.on_message(
            &Message {
                header: header(MessageType::DelayResp, 1, false, master_port()),
                body: Body::DelayResp { receive: ts(100, 400_000_000), requesting: someone_else },
            },
            ts(0, 0),
            now,
        );
        assert!(s.last_measurement().is_none());
        assert_eq!(s.stats().delay_resps, 0);
    }

    #[test]
    fn traffic_from_another_domain_is_counted_and_discarded() {
        let mut s = locked_on_master();
        let mut msg = announce_msg(master_port(), 1);
        msg.header.domain = 127;
        s.on_message(&msg, ts(0, 0), Instant::now());
        assert_eq!(s.stats().wrong_domain, 1);
        // The priority-1 announce on the wrong domain must not have become our master.
        assert_eq!(s.master().unwrap().priority1, 128);
    }

    #[test]
    fn sync_from_a_clock_that_is_not_our_master_is_ignored() {
        let mut s = locked_on_master();
        let other = PortIdentity { clock: ClockIdentity([2; 8]), port: 1 };
        s.on_message(
            &Message {
                header: header(MessageType::Sync, 1, false, other),
                body: Body::Sync { origin: ts(100, 0) },
            },
            ts(100, 1_005_000),
            Instant::now(),
        );
        assert_eq!(s.stats().syncs, 0);
    }

    #[test]
    fn a_better_grandmaster_takes_over_and_discards_work_in_flight() {
        let mut s = locked_on_master();
        let now = Instant::now();

        // A Sync from the old master is half-processed when a better master appears.
        s.on_message(
            &Message {
                header: header(MessageType::Sync, 1, true, master_port()),
                body: Body::Sync { origin: Timestamp::default() },
            },
            ts(100, 1_005_000),
            now,
        );

        let better = PortIdentity { clock: ClockIdentity([0, 0, 0, 0xFF, 0xFE, 0, 0, 1]), port: 1 };
        let ev = s.on_message(&announce_msg(better, 50), ts(0, 0), now);
        assert_eq!(ev, Some(Event::MasterChanged));
        assert_eq!(s.master().unwrap().sender, better);

        // The old master's Follow_Up must not complete against the new timeline.
        s.on_message(
            &Message {
                header: header(MessageType::FollowUp, 1, false, master_port()),
                body: Body::FollowUp { precise_origin: ts(100, 0) },
            },
            ts(0, 0),
            now,
        );
        assert!(s.last_measurement().is_none());
    }

    #[test]
    fn repeated_exchanges_drive_the_servo_to_lock() {
        let mut s = locked_on_master();
        // A steady 30 us clock error with 5 us of path delay, with the servo's own
        // correction fed back in so it is closing a real loop rather than being handed
        // a shrinking number.
        //
        // t1 sits a millisecond into the second so the error can go negative during
        // overshoot without the timestamps underflowing.
        for seq in 0..80u16 {
            let err = 30_000i64 + s.servo().local_to_ptp_offset();
            let base = 200u64 + seq as u64;
            let t1 = 1_000_000i64;
            let t2 = 1_005_000 + err;
            let t3 = 500_000_000 + err;
            let t4 = 500_005_000i64;
            assert!(t2 > 0 && t3 > 0, "test timestamps went negative: {t2}, {t3}");
            full_exchange(
                &mut s,
                seq,
                ts(base, t1 as u32),
                ts(base, t2 as u32),
                ts(base, t3 as u32),
                ts(base, t4 as u32),
            );
        }
        assert_eq!(s.servo().state(), LockState::Locked);
        assert!(
            s.servo().last_offset_nanos().abs() < 2_000,
            "residual offset {} ns",
            s.servo().last_offset_nanos()
        );
    }

    #[test]
    fn the_wire_format_round_trips_through_the_state_machine() {
        // Guards the seam between the codec and the logic: a message built as bytes,
        // parsed, and acted on.
        let mut s = locked_on_master();
        let mut buf = [0u8; 44];
        header(MessageType::Sync, 3, false, master_port()).write(44, 0x00, &mut buf);
        ts(500, 250_000).write(&mut buf[HEADER_LEN..]);
        let msg = Message::parse(&buf).unwrap();

        s.on_message(&msg, ts(500, 255_000), Instant::now());
        assert_eq!(s.stats().syncs, 1);

        let mut resp = [0u8; 54];
        header(MessageType::DelayResp, 3, false, master_port()).write(54, 0x03, &mut resp);
        ts(500, 750_000).write(&mut resp[HEADER_LEN..]);
        our_port().write(&mut resp[HEADER_LEN + TIMESTAMP_LEN..]);

        s.on_delay_req_sent(3, ts(500, 745_000));
        s.on_message(&Message::parse(&resp).unwrap(), ts(0, 0), Instant::now());

        let m = s.last_measurement().expect("measurement from parsed bytes");
        assert_eq!(m.delay_nanos, 5_000);
        assert_eq!(m.offset_nanos, 0);
    }
}
