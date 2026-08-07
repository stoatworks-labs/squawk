//! PTPv2 message coding, per IEEE 1588-2008 clause 13.
//!
//! Only the messages AES67 actually needs: Announce, Sync, Follow_Up, Delay_Req and
//! Delay_Resp — the delay request-response mechanism. The peer-delay messages
//! (Pdelay_*) are recognised so they can be counted and ignored rather than logged as
//! garbage, because they will be present on any network that also carries a peer-delay
//! profile.

use thiserror::Error;

/// Fixed PTP header length.
pub const HEADER_LEN: usize = 34;

/// Length of a `Timestamp` on the wire: 48-bit seconds, 32-bit nanoseconds.
pub const TIMESTAMP_LEN: usize = 10;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PtpError {
    #[error("packet is {0} bytes, shorter than the {HEADER_LEN}-byte PTP header")]
    TooShort(usize),
    #[error("PTP version is {0}, expected 2")]
    BadVersion(u8),
    #[error("message type {0:#x} carries a {1}-byte body, which is too short")]
    TruncatedBody(u8, usize),
    #[error("nanoseconds field is {0}, which is not below one second")]
    BadTimestamp(u32),
}

/// PTP message types (IEEE 1588-2008 table 19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Sync,
    DelayReq,
    PdelayReq,
    PdelayResp,
    FollowUp,
    DelayResp,
    PdelayRespFollowUp,
    Announce,
    Signaling,
    Management,
    Unknown(u8),
}

impl MessageType {
    pub fn from_bits(v: u8) -> Self {
        match v & 0x0f {
            0x0 => MessageType::Sync,
            0x1 => MessageType::DelayReq,
            0x2 => MessageType::PdelayReq,
            0x3 => MessageType::PdelayResp,
            0x8 => MessageType::FollowUp,
            0x9 => MessageType::DelayResp,
            0xA => MessageType::PdelayRespFollowUp,
            0xB => MessageType::Announce,
            0xC => MessageType::Signaling,
            0xD => MessageType::Management,
            other => MessageType::Unknown(other),
        }
    }

    pub fn to_bits(self) -> u8 {
        match self {
            MessageType::Sync => 0x0,
            MessageType::DelayReq => 0x1,
            MessageType::PdelayReq => 0x2,
            MessageType::PdelayResp => 0x3,
            MessageType::FollowUp => 0x8,
            MessageType::DelayResp => 0x9,
            MessageType::PdelayRespFollowUp => 0xA,
            MessageType::Announce => 0xB,
            MessageType::Signaling => 0xC,
            MessageType::Management => 0xD,
            MessageType::Unknown(v) => v & 0x0f,
        }
    }

    /// Event messages are the ones that must be timestamped on the wire, and are sent
    /// to port 319. Everything else is a general message on port 320.
    pub fn is_event(self) -> bool {
        matches!(
            self,
            MessageType::Sync | MessageType::DelayReq | MessageType::PdelayReq | MessageType::PdelayResp
        )
    }
}

/// A PTP timestamp: 48-bit seconds since the epoch, plus nanoseconds.
///
/// Held as separate fields rather than as a single integer of nanoseconds because the
/// full range does not fit in 64 bits — 2^48 seconds is about 9 million years, and
/// nanoseconds of that overflows u64 by five orders of magnitude. Code that flattens
/// PTP timestamps to `u64` nanoseconds works perfectly against every real grandmaster
/// (which report time near the Unix epoch) and overflows on a synthetic one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub struct Timestamp {
    pub seconds: u64,
    pub nanos: u32,
}

impl Timestamp {
    pub fn new(seconds: u64, nanos: u32) -> Self {
        Self { seconds, nanos }
    }

    /// Difference in nanoseconds. Saturates rather than wrapping — a difference that
    /// does not fit in an i64 means one of the two is nonsense, and a saturated value
    /// is a reading the servo will reject instead of a wrapped one it will believe.
    pub fn diff_nanos(self, earlier: Self) -> i64 {
        let secs = self.seconds as i128 - earlier.seconds as i128;
        let nanos = self.nanos as i128 - earlier.nanos as i128;
        let total = secs * 1_000_000_000 + nanos;
        total.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    pub fn write(&self, out: &mut [u8]) {
        debug_assert!(out.len() >= TIMESTAMP_LEN);
        let s = self.seconds.to_be_bytes();
        out[0..6].copy_from_slice(&s[2..8]);
        out[6..10].copy_from_slice(&self.nanos.to_be_bytes());
    }

    pub fn parse(buf: &[u8]) -> Result<Self, PtpError> {
        if buf.len() < TIMESTAMP_LEN {
            return Err(PtpError::TooShort(buf.len()));
        }
        let seconds = u64::from_be_bytes([0, 0, buf[0], buf[1], buf[2], buf[3], buf[4], buf[5]]);
        let nanos = u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]);
        if nanos >= 1_000_000_000 {
            return Err(PtpError::BadTimestamp(nanos));
        }
        Ok(Self { seconds, nanos })
    }
}

/// The 8-byte identity of a clock, usually derived from a MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ClockIdentity(pub [u8; 8]);

impl ClockIdentity {
    /// Build the conventional identity from a MAC: `xx xx xx FF FE xx xx xx`.
    pub fn from_mac(mac: [u8; 6]) -> Self {
        Self([mac[0], mac[1], mac[2], 0xFF, 0xFE, mac[3], mac[4], mac[5]])
    }
}

impl std::fmt::Display for ClockIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            write!(f, "{b:02X}")?;
        }
        Ok(())
    }
}

/// Which port of which clock a message came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PortIdentity {
    pub clock: ClockIdentity,
    pub port: u16,
}

impl PortIdentity {
    pub fn write(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.clock.0);
        out[8..10].copy_from_slice(&self.port.to_be_bytes());
    }

    pub fn parse(buf: &[u8]) -> Result<Self, PtpError> {
        if buf.len() < 10 {
            return Err(PtpError::TooShort(buf.len()));
        }
        let mut clock = [0u8; 8];
        clock.copy_from_slice(&buf[0..8]);
        Ok(Self { clock: ClockIdentity(clock), port: u16::from_be_bytes([buf[8], buf[9]]) })
    }
}

/// Header flags that matter to a slave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    /// The sender is a two-step clock: the real transmit time arrives in a Follow_Up,
    /// and the Sync's own originTimestamp must be ignored.
    pub two_step: bool,
    pub unicast: bool,
    pub leap61: bool,
    pub leap59: bool,
    pub utc_offset_valid: bool,
    pub ptp_timescale: bool,
    pub time_traceable: bool,
    pub frequency_traceable: bool,
}

impl Flags {
    fn parse(bytes: [u8; 2]) -> Self {
        Self {
            two_step: bytes[0] & 0x02 != 0,
            unicast: bytes[0] & 0x04 != 0,
            leap61: bytes[1] & 0x01 != 0,
            leap59: bytes[1] & 0x02 != 0,
            utc_offset_valid: bytes[1] & 0x04 != 0,
            ptp_timescale: bytes[1] & 0x08 != 0,
            time_traceable: bytes[1] & 0x10 != 0,
            frequency_traceable: bytes[1] & 0x20 != 0,
        }
    }

    fn write(self) -> [u8; 2] {
        let mut b = [0u8; 2];
        if self.two_step {
            b[0] |= 0x02;
        }
        if self.unicast {
            b[0] |= 0x04;
        }
        if self.leap61 {
            b[1] |= 0x01;
        }
        if self.leap59 {
            b[1] |= 0x02;
        }
        if self.utc_offset_valid {
            b[1] |= 0x04;
        }
        if self.ptp_timescale {
            b[1] |= 0x08;
        }
        if self.time_traceable {
            b[1] |= 0x10;
        }
        if self.frequency_traceable {
            b[1] |= 0x20;
        }
        b
    }
}

/// The common header on every PTP message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub message_type: MessageType,
    pub domain: u8,
    pub flags: Flags,
    /// Accumulated residence and asymmetry correction, in nanoseconds scaled by 2^16.
    ///
    /// Every transparent clock a message passes through adds its residence time here.
    /// A slave that ignores this field reads an offset inflated by however long the
    /// packet sat inside the switches — which on a busy network is exactly the error
    /// PTP exists to remove.
    pub correction_subnanos: i64,
    pub source: PortIdentity,
    pub sequence_id: u16,
    pub log_message_interval: i8,
}

impl Header {
    /// Correction field in whole nanoseconds.
    pub fn correction_nanos(&self) -> i64 {
        self.correction_subnanos >> 16
    }

    pub fn parse(buf: &[u8]) -> Result<Self, PtpError> {
        if buf.len() < HEADER_LEN {
            return Err(PtpError::TooShort(buf.len()));
        }
        let version = buf[1] & 0x0f;
        if version != 2 {
            return Err(PtpError::BadVersion(version));
        }
        Ok(Self {
            message_type: MessageType::from_bits(buf[0]),
            domain: buf[4],
            flags: Flags::parse([buf[6], buf[7]]),
            correction_subnanos: i64::from_be_bytes([
                buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
            ]),
            source: PortIdentity::parse(&buf[20..30])?,
            sequence_id: u16::from_be_bytes([buf[30], buf[31]]),
            log_message_interval: buf[33] as i8,
        })
    }

    pub fn write(&self, length: u16, control: u8, out: &mut [u8]) {
        debug_assert!(out.len() >= HEADER_LEN);
        out[..HEADER_LEN].fill(0);
        out[0] = self.message_type.to_bits();
        out[1] = 0x02; // versionPTP = 2
        out[2..4].copy_from_slice(&length.to_be_bytes());
        out[4] = self.domain;
        let f = self.flags.write();
        out[6] = f[0];
        out[7] = f[1];
        out[8..16].copy_from_slice(&self.correction_subnanos.to_be_bytes());
        self.source.write(&mut out[20..30]);
        out[30..32].copy_from_slice(&self.sequence_id.to_be_bytes());
        out[32] = control;
        out[33] = self.log_message_interval as u8;
    }
}

/// How good a clock says it is (IEEE 1588-2008 clause 8.6.2.2-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockQuality {
    pub class: u8,
    pub accuracy: u8,
    pub offset_scaled_log_variance: u16,
}

/// The contents of an Announce, which is what the BMCA runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announce {
    pub current_utc_offset: i16,
    pub grandmaster_priority1: u8,
    pub grandmaster_quality: ClockQuality,
    pub grandmaster_priority2: u8,
    pub grandmaster_identity: ClockIdentity,
    pub steps_removed: u16,
    pub time_source: u8,
}

/// A parsed PTP message.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// Sync and Delay_Req both carry only an origin timestamp.
    Sync { origin: Timestamp },
    DelayReq { origin: Timestamp },
    FollowUp { precise_origin: Timestamp },
    DelayResp { receive: Timestamp, requesting: PortIdentity },
    Announce(Box<Announce>),
    /// Recognised but not acted on.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub header: Header,
    pub body: Body,
}

impl Message {
    pub fn parse(buf: &[u8]) -> Result<Self, PtpError> {
        let header = Header::parse(buf)?;
        let rest = &buf[HEADER_LEN..];
        let need = |n: usize| -> Result<(), PtpError> {
            if rest.len() < n {
                Err(PtpError::TruncatedBody(header.message_type.to_bits(), rest.len()))
            } else {
                Ok(())
            }
        };

        let body = match header.message_type {
            MessageType::Sync => {
                need(TIMESTAMP_LEN)?;
                Body::Sync { origin: Timestamp::parse(rest)? }
            }
            MessageType::DelayReq => {
                need(TIMESTAMP_LEN)?;
                Body::DelayReq { origin: Timestamp::parse(rest)? }
            }
            MessageType::FollowUp => {
                need(TIMESTAMP_LEN)?;
                Body::FollowUp { precise_origin: Timestamp::parse(rest)? }
            }
            MessageType::DelayResp => {
                need(TIMESTAMP_LEN + 10)?;
                Body::DelayResp {
                    receive: Timestamp::parse(rest)?,
                    requesting: PortIdentity::parse(&rest[TIMESTAMP_LEN..])?,
                }
            }
            MessageType::Announce => {
                need(30)?;
                Body::Announce(Box::new(Announce {
                    current_utc_offset: i16::from_be_bytes([rest[10], rest[11]]),
                    grandmaster_priority1: rest[13],
                    grandmaster_quality: ClockQuality {
                        class: rest[14],
                        accuracy: rest[15],
                        offset_scaled_log_variance: u16::from_be_bytes([rest[16], rest[17]]),
                    },
                    grandmaster_priority2: rest[18],
                    grandmaster_identity: {
                        let mut id = [0u8; 8];
                        id.copy_from_slice(&rest[19..27]);
                        ClockIdentity(id)
                    },
                    steps_removed: u16::from_be_bytes([rest[27], rest[28]]),
                    time_source: rest[29],
                }))
            }
            _ => Body::Other,
        };

        Ok(Self { header, body })
    }
}

/// Build a Delay_Req. The only message a slave-only implementation has to transmit.
///
/// Its originTimestamp is deliberately zero: the value that matters is t3, the instant
/// it actually left, which the sender records locally. Filling this field in with an
/// estimate would be writing down a time the packet did not leave.
pub fn write_delay_req(source: PortIdentity, domain: u8, sequence_id: u16, out: &mut [u8; 44]) {
    let header = Header {
        message_type: MessageType::DelayReq,
        domain,
        flags: Flags::default(),
        correction_subnanos: 0,
        source,
        sequence_id,
        log_message_interval: 0x7f, // per spec for Delay_Req
    };
    header.write(44, 0x01, out);
    Timestamp::default().write(&mut out[HEADER_LEN..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_source() -> PortIdentity {
        PortIdentity {
            clock: ClockIdentity::from_mac([0x00, 0x1D, 0xC1, 0x12, 0x34, 0x56]),
            port: 1,
        }
    }

    #[test]
    fn clock_identity_follows_the_mac_convention() {
        let id = ClockIdentity::from_mac([0x00, 0x1D, 0xC1, 0x12, 0x34, 0x56]);
        assert_eq!(id.0, [0x00, 0x1D, 0xC1, 0xFF, 0xFE, 0x12, 0x34, 0x56]);
        assert_eq!(id.to_string(), "00-1D-C1-FF-FE-12-34-56");
    }

    #[test]
    fn timestamps_round_trip_across_the_48_bit_seconds_field() {
        let t = Timestamp::new(0x0000_1234_5678_9ABC & 0xFFFF_FFFF_FFFF, 999_999_999);
        let mut buf = [0u8; TIMESTAMP_LEN];
        t.write(&mut buf);
        assert_eq!(Timestamp::parse(&buf).unwrap(), t);
    }

    #[test]
    fn a_nanoseconds_field_of_a_second_or_more_is_rejected() {
        let mut buf = [0u8; TIMESTAMP_LEN];
        buf[6..10].copy_from_slice(&1_000_000_000u32.to_be_bytes());
        assert_eq!(Timestamp::parse(&buf), Err(PtpError::BadTimestamp(1_000_000_000)));
    }

    #[test]
    fn timestamp_differences_do_not_overflow_at_the_top_of_the_range() {
        // A u64-of-nanoseconds representation overflows here. Real grandmasters sit
        // near the Unix epoch, so this only ever bites on a synthetic or faulty one —
        // which is exactly when you least want a wrapped number believed.
        let far = Timestamp::new(0xFFFF_FFFF_FFFF, 0);
        let near = Timestamp::new(0, 0);
        assert_eq!(far.diff_nanos(near), i64::MAX);
        assert_eq!(near.diff_nanos(far), i64::MIN);

        // And ordinary differences are exact.
        let a = Timestamp::new(1_700_000_010, 500_000_000);
        let b = Timestamp::new(1_700_000_009, 250_000_000);
        assert_eq!(a.diff_nanos(b), 1_250_000_000);
        assert_eq!(b.diff_nanos(a), -1_250_000_000);
    }

    #[test]
    fn a_header_round_trips() {
        let h = Header {
            message_type: MessageType::Sync,
            domain: 127,
            flags: Flags { two_step: true, ptp_timescale: true, ..Default::default() },
            correction_subnanos: 12_345 << 16,
            source: sample_source(),
            sequence_id: 0xBEEF,
            log_message_interval: -3,
        };
        let mut buf = [0u8; 44];
        h.write(44, 0x00, &mut buf);

        assert_eq!(buf[1] & 0x0f, 2, "versionPTP must be 2");
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 44);

        let parsed = Header::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(parsed.correction_nanos(), 12_345);
        assert!(parsed.flags.two_step);
    }

    #[test]
    fn the_correction_field_is_scaled_nanoseconds() {
        // Residence time from transparent clocks lives in the low 16 bits as a
        // fraction. Reading the raw i64 as nanoseconds inflates it 65536-fold.
        let mut buf = [0u8; 44];
        let h = Header {
            message_type: MessageType::Sync,
            domain: 0,
            flags: Flags::default(),
            correction_subnanos: (2_500i64 << 16) | 0x8000, // 2500.5 ns
            source: sample_source(),
            sequence_id: 1,
            log_message_interval: 0,
        };
        h.write(44, 0, &mut buf);
        assert_eq!(Header::parse(&buf).unwrap().correction_nanos(), 2_500);
    }

    #[test]
    fn a_version_1_packet_is_rejected() {
        let mut buf = [0u8; HEADER_LEN];
        buf[1] = 0x01;
        assert_eq!(Header::parse(&buf), Err(PtpError::BadVersion(1)));
    }

    #[test]
    fn event_messages_are_the_ones_that_need_timestamping() {
        assert!(MessageType::Sync.is_event());
        assert!(MessageType::DelayReq.is_event());
        assert!(!MessageType::FollowUp.is_event());
        assert!(!MessageType::DelayResp.is_event());
        assert!(!MessageType::Announce.is_event());
    }

    #[test]
    fn parses_a_two_step_sync() {
        let mut buf = [0u8; 44];
        Header {
            message_type: MessageType::Sync,
            domain: 0,
            flags: Flags { two_step: true, ..Default::default() },
            correction_subnanos: 0,
            source: sample_source(),
            sequence_id: 7,
            log_message_interval: -3,
        }
        .write(44, 0x00, &mut buf);
        Timestamp::new(1_700_000_000, 123).write(&mut buf[HEADER_LEN..]);

        let msg = Message::parse(&buf).unwrap();
        assert!(msg.header.flags.two_step);
        // The origin timestamp of a two-step Sync is meaningless — the real transmit
        // time arrives in the Follow_Up — but it must still parse.
        assert_eq!(msg.body, Body::Sync { origin: Timestamp::new(1_700_000_000, 123) });
    }

    #[test]
    fn parses_a_delay_resp_and_its_requesting_port() {
        let mut buf = [0u8; 54];
        Header {
            message_type: MessageType::DelayResp,
            domain: 0,
            flags: Flags::default(),
            correction_subnanos: 0,
            source: sample_source(),
            sequence_id: 42,
            log_message_interval: 0,
        }
        .write(54, 0x03, &mut buf);
        Timestamp::new(1_700_000_001, 500).write(&mut buf[HEADER_LEN..]);
        let requester = PortIdentity {
            clock: ClockIdentity::from_mac([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
            port: 3,
        };
        requester.write(&mut buf[HEADER_LEN + TIMESTAMP_LEN..]);

        let msg = Message::parse(&buf).unwrap();
        assert_eq!(
            msg.body,
            Body::DelayResp { receive: Timestamp::new(1_700_000_001, 500), requesting: requester }
        );
    }

    #[test]
    fn parses_an_announce() {
        let mut buf = [0u8; 64];
        Header {
            message_type: MessageType::Announce,
            domain: 0,
            flags: Flags { ptp_timescale: true, time_traceable: true, ..Default::default() },
            correction_subnanos: 0,
            source: sample_source(),
            sequence_id: 9,
            log_message_interval: 1,
        }
        .write(64, 0x05, &mut buf);

        let body = &mut buf[HEADER_LEN..];
        body[10..12].copy_from_slice(&37i16.to_be_bytes()); // currentUtcOffset
        body[13] = 128; // priority1
        body[14] = 6; // clockClass — a GPS-locked grandmaster
        body[15] = 0x21; // accuracy, within 100 ns
        body[16..18].copy_from_slice(&0x436Au16.to_be_bytes());
        body[18] = 128; // priority2
        let gm = ClockIdentity::from_mac([0x00, 0x1D, 0xC1, 0x12, 0x34, 0x56]);
        body[19..27].copy_from_slice(&gm.0);
        body[27..29].copy_from_slice(&0u16.to_be_bytes()); // stepsRemoved
        body[29] = 0x20; // timeSource: GPS

        let msg = Message::parse(&buf).unwrap();
        let Body::Announce(a) = msg.body else { panic!("not an announce") };
        assert_eq!(a.current_utc_offset, 37);
        assert_eq!(a.grandmaster_priority1, 128);
        assert_eq!(a.grandmaster_quality.class, 6);
        assert_eq!(a.grandmaster_identity, gm);
        assert_eq!(a.steps_removed, 0);
        assert_eq!(a.time_source, 0x20);
    }

    #[test]
    fn a_truncated_body_is_rejected_rather_than_read_past() {
        let mut buf = [0u8; HEADER_LEN + 4];
        Header {
            message_type: MessageType::Announce,
            domain: 0,
            flags: Flags::default(),
            correction_subnanos: 0,
            source: sample_source(),
            sequence_id: 1,
            log_message_interval: 0,
        }
        .write(HEADER_LEN as u16 + 4, 0x05, &mut buf);
        assert_eq!(Message::parse(&buf), Err(PtpError::TruncatedBody(0xB, 4)));
    }

    #[test]
    fn peer_delay_messages_parse_as_other_rather_than_failing() {
        // They will be on any network also running a peer-delay profile, and should be
        // ignored quietly rather than logged as corruption.
        let mut buf = [0u8; 54];
        Header {
            message_type: MessageType::PdelayReq,
            domain: 0,
            flags: Flags::default(),
            correction_subnanos: 0,
            source: sample_source(),
            sequence_id: 1,
            log_message_interval: 0,
        }
        .write(54, 0x05, &mut buf);
        assert_eq!(Message::parse(&buf).unwrap().body, Body::Other);
    }

    #[test]
    fn a_built_delay_req_parses_back() {
        let mut buf = [0u8; 44];
        write_delay_req(sample_source(), 0, 1234, &mut buf);
        let msg = Message::parse(&buf).unwrap();
        assert_eq!(msg.header.message_type, MessageType::DelayReq);
        assert_eq!(msg.header.sequence_id, 1234);
        assert_eq!(msg.header.source, sample_source());
        // Zero on purpose: t3 is what the sender recorded locally, not what it wrote.
        assert_eq!(msg.body, Body::DelayReq { origin: Timestamp::default() });
    }
}
