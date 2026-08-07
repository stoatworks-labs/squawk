//! A synthetic grandmaster, for testing a slave without a real one.
//!
//! Public rather than confined to this crate's own tests, because `squawk-server` needs
//! it too: verifying that RTP timestamps track PTP means having a PTP domain to track.
//! It is a test aid and nothing more — it does no BMCA, never yields to a better clock,
//! and answers every Delay_Req it sees.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use socket2::{Domain, Protocol, Socket, Type};

use crate::message::{
    ClockIdentity, Flags, Header, Message, MessageType, PortIdentity, Timestamp, HEADER_LEN,
    TIMESTAMP_LEN,
};
use crate::{PORT_EVENT, PORT_GENERAL, PTP_PRIMARY};

/// A running synthetic grandmaster. Dropping it stops the thread.
pub struct Grandmaster {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    offset_nanos: i64,
    identity: PortIdentity,
}

impl Grandmaster {
    /// Start announcing on `iface`, with a clock `offset_nanos` from local time.
    ///
    /// A non-zero offset is what makes a test meaningful: with zero, a slave that
    /// ignored every message would still appear to be perfectly locked.
    pub fn spawn(iface: Ipv4Addr, domain: u8, offset_nanos: i64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let identity = PortIdentity {
            clock: ClockIdentity::from_mac([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE]),
            port: 1,
        };
        let handle = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("test-grandmaster".into())
                .spawn(move || run(iface, domain, offset_nanos, identity, stop))
                .expect("spawn grandmaster")
        };
        Self { stop, handle: Some(handle), offset_nanos, identity }
    }

    pub fn identity(&self) -> PortIdentity {
        self.identity
    }

    pub fn offset_nanos(&self) -> i64 {
        self.offset_nanos
    }

    /// This master's current time, so a test can compare against it directly.
    pub fn now(&self) -> Timestamp {
        now_with_offset(self.offset_nanos)
    }
}

impl Drop for Grandmaster {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn now_with_offset(offset_nanos: i64) -> Timestamp {
    let total = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let total = (total as i128 + offset_nanos as i128).max(0) as u128;
    Timestamp {
        seconds: (total / 1_000_000_000) as u64,
        nanos: (total % 1_000_000_000) as u32,
    }
}

fn join(iface: Ipv4Addr, port: u16) -> UdpSocket {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("socket");
    s.set_reuse_address(true).expect("reuse addr");
    #[cfg(unix)]
    s.set_reuse_port(true).expect("reuse port");
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
        .expect("bind");
    s.join_multicast_v4(&PTP_PRIMARY, &iface).expect("join");
    s.set_multicast_if_v4(&iface).expect("mcast if");
    s.set_multicast_ttl_v4(1).expect("ttl");
    s.set_nonblocking(true).expect("nonblocking");
    s.into()
}

fn header(identity: PortIdentity, domain: u8, kind: MessageType, seq: u16, two_step: bool) -> Header {
    Header {
        message_type: kind,
        domain,
        flags: Flags { two_step, ptp_timescale: true, ..Default::default() },
        correction_subnanos: 0,
        source: identity,
        sequence_id: seq,
        log_message_interval: -4,
    }
}

fn run(iface: Ipv4Addr, domain: u8, offset: i64, identity: PortIdentity, stop: Arc<AtomicBool>) {
    let event = join(iface, PORT_EVENT);
    let general = join(iface, PORT_GENERAL);
    let event_dest = SocketAddrV4::new(PTP_PRIMARY, PORT_EVENT);
    let general_dest = SocketAddrV4::new(PTP_PRIMARY, PORT_GENERAL);

    let mut seq = 0u16;
    let mut last_sync = Instant::now();
    let mut last_announce = Instant::now() - Duration::from_secs(1);
    let mut buf = [0u8; 1500];

    while !stop.load(Ordering::Relaxed) {
        if last_announce.elapsed() >= Duration::from_millis(100) {
            let mut pkt = [0u8; 64];
            header(identity, domain, MessageType::Announce, seq, false).write(64, 0x05, &mut pkt);
            let body = &mut pkt[HEADER_LEN..];
            body[10..12].copy_from_slice(&37i16.to_be_bytes());
            body[13] = 128; // priority1
            body[14] = 6; // clockClass: locked to a primary reference
            body[15] = 0x21;
            body[16..18].copy_from_slice(&0x436Au16.to_be_bytes());
            body[18] = 128; // priority2
            body[19..27].copy_from_slice(&identity.clock.0);
            body[29] = 0x20; // timeSource: GPS
            let _ = general.send_to(&pkt, general_dest);
            last_announce = Instant::now();
        }

        if last_sync.elapsed() >= Duration::from_millis(20) {
            seq = seq.wrapping_add(1);
            let mut pkt = [0u8; 44];
            header(identity, domain, MessageType::Sync, seq, true).write(44, 0x00, &mut pkt);
            Timestamp::default().write(&mut pkt[HEADER_LEN..]);
            let _ = event.send_to(&pkt, event_dest);
            // Two-step: the transmit time is captured after the send and follows on.
            let t1 = now_with_offset(offset);

            let mut fu = [0u8; 44];
            header(identity, domain, MessageType::FollowUp, seq, false).write(44, 0x02, &mut fu);
            t1.write(&mut fu[HEADER_LEN..]);
            let _ = general.send_to(&fu, general_dest);
            last_sync = Instant::now();
        }

        // Everything on this group loops back, our own Syncs included.
        while let Ok(len) = event.recv(&mut buf) {
            let t4 = now_with_offset(offset);
            let Ok(msg) = Message::parse(&buf[..len]) else { continue };
            if msg.header.message_type != MessageType::DelayReq || msg.header.domain != domain {
                continue;
            }
            let mut resp = [0u8; 54];
            header(identity, domain, MessageType::DelayResp, msg.header.sequence_id, false)
                .write(54, 0x03, &mut resp);
            t4.write(&mut resp[HEADER_LEN..]);
            msg.header.source.write(&mut resp[HEADER_LEN + TIMESTAMP_LEN..]);
            let _ = general.send_to(&resp, general_dest);
        }

        // Drain promptly: every millisecond a Delay_Req waits here lands in t4, where
        // it is indistinguishable from path delay. A synthetic master has to be a
        // better timestamper than the thing it is testing.
        std::thread::sleep(Duration::from_micros(200));
    }
}
