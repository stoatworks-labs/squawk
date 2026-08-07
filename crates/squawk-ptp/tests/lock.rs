//! Lock a real `PtpPort` to a synthetic grandmaster over real sockets.
//!
//! There is no grandmaster on the network this was developed on, so the alternative to
//! this test is shipping a PTP slave that has never once locked to anything. This runs
//! a minimal but honest master on loopback — Announce, two-step Sync with Follow_Up,
//! and Delay_Resp — and asserts the slave finds it, steps, converges and reports a
//! sensible offset.
//!
//! It exercises the whole stack together: multicast sockets, message encoding, the
//! BMCA, sequence matching and the servo. What it cannot exercise is a real network's
//! asymmetry, a switch's residence time, or another vendor's interpretation of the
//! standard.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use socket2::{Domain, Protocol, Socket, Type};
use squawk_ptp::message::{
    ClockIdentity, Flags, Header, Message, MessageType, PortIdentity, Timestamp, HEADER_LEN,
    TIMESTAMP_LEN,
};
use squawk_ptp::servo::LockState;
use squawk_ptp::{PtpPort, PORT_EVENT, PORT_GENERAL, PTP_PRIMARY};

const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
/// How far ahead of local time the synthetic master's clock runs.
const MASTER_OFFSET_NANOS: i64 = 5_000_000; // 5 ms — large enough to force a step

fn join(port: u16) -> UdpSocket {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    s.set_reuse_address(true).unwrap();
    #[cfg(unix)]
    s.set_reuse_port(true).unwrap();
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into()).unwrap();
    s.join_multicast_v4(&PTP_PRIMARY, &LOOPBACK).unwrap();
    s.set_multicast_if_v4(&LOOPBACK).unwrap();
    s.set_multicast_ttl_v4(1).unwrap();
    s.set_nonblocking(true).unwrap();
    s.into()
}

struct Master {
    identity: PortIdentity,
    epoch: Instant,
    epoch_nanos: u128,
}

impl Master {
    fn now(&self) -> Timestamp {
        let total = self.epoch_nanos + self.epoch.elapsed().as_nanos();
        let total = (total as i128 + MASTER_OFFSET_NANOS as i128).max(0) as u128;
        Timestamp {
            seconds: (total / 1_000_000_000) as u64,
            nanos: (total % 1_000_000_000) as u32,
        }
    }

    fn header(&self, kind: MessageType, seq: u16, two_step: bool) -> Header {
        Header {
            message_type: kind,
            domain: 0,
            flags: Flags { two_step, ptp_timescale: true, ..Default::default() },
            correction_subnanos: 0,
            source: self.identity,
            sequence_id: seq,
            log_message_interval: -4,
        }
    }
}

/// Run a synthetic grandmaster until told to stop.
fn spawn_master(stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let event = join(PORT_EVENT);
        let general = join(PORT_GENERAL);
        let master = Master {
            identity: PortIdentity {
                clock: ClockIdentity::from_mac([0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE]),
                port: 1,
            },
            epoch: Instant::now(),
            epoch_nanos: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        };

        let event_dest = SocketAddrV4::new(PTP_PRIMARY, PORT_EVENT);
        let general_dest = SocketAddrV4::new(PTP_PRIMARY, PORT_GENERAL);

        let mut seq = 0u16;
        let mut last_sync = Instant::now();
        let mut last_announce = Instant::now() - Duration::from_secs(1);
        let mut buf = [0u8; 1500];

        while !stop.load(Ordering::Relaxed) {
            // Announce, so the slave's BMCA has something to select.
            if last_announce.elapsed() >= Duration::from_millis(100) {
                let mut pkt = [0u8; 64];
                master.header(MessageType::Announce, seq, false).write(64, 0x05, &mut pkt);
                let body = &mut pkt[HEADER_LEN..];
                body[10..12].copy_from_slice(&37i16.to_be_bytes());
                body[13] = 128; // priority1
                body[14] = 6; // clockClass: locked to a primary reference
                body[15] = 0x21;
                body[16..18].copy_from_slice(&0x436Au16.to_be_bytes());
                body[18] = 128; // priority2
                body[19..27].copy_from_slice(&master.identity.clock.0);
                body[29] = 0x20; // timeSource: GPS
                let _ = general.send_to(&pkt, general_dest);
                last_announce = Instant::now();
            }

            // Two-step Sync: the Sync itself carries a placeholder, and the real
            // transmit time follows in the Follow_Up.
            if last_sync.elapsed() >= Duration::from_millis(20) {
                seq = seq.wrapping_add(1);
                let mut pkt = [0u8; 44];
                master.header(MessageType::Sync, seq, true).write(44, 0x00, &mut pkt);
                Timestamp::default().write(&mut pkt[HEADER_LEN..]);
                let _ = event.send_to(&pkt, event_dest);
                let t1 = master.now();

                let mut fu = [0u8; 44];
                master.header(MessageType::FollowUp, seq, false).write(44, 0x02, &mut fu);
                t1.write(&mut fu[HEADER_LEN..]);
                let _ = general.send_to(&fu, general_dest);
                last_sync = Instant::now();
            }

            // Answer Delay_Reqs. Everything on this group loops back, including our own
            // Syncs, so anything that is not a Delay_Req is ignored.
            while let Ok(len) = event.recv(&mut buf) {
                let t4 = master.now();
                let Ok(msg) = Message::parse(&buf[..len]) else { continue };
                if msg.header.message_type != MessageType::DelayReq {
                    continue;
                }
                let mut resp = [0u8; 54];
                master
                    .header(MessageType::DelayResp, msg.header.sequence_id, false)
                    .write(54, 0x03, &mut resp);
                t4.write(&mut resp[HEADER_LEN..]);
                msg.header.source.write(&mut resp[HEADER_LEN + TIMESTAMP_LEN..]);
                let _ = general.send_to(&resp, general_dest);
            }

            // Drain promptly. Every millisecond a Delay_Req waits here lands in t4,
            // where it is indistinguishable from path delay — the synthetic master has
            // to be a better timestamper than the thing it is testing.
            std::thread::sleep(Duration::from_micros(200));
        }
    })
}

#[test]
fn the_slave_locks_to_a_grandmaster_over_real_sockets() {
    let stop = Arc::new(AtomicBool::new(false));
    let master = spawn_master(Arc::clone(&stop));
    // Let the master start announcing before the slave looks for it.
    std::thread::sleep(Duration::from_millis(150));

    let identity = PortIdentity {
        clock: ClockIdentity::from_mac([0x02, 0x73, 0x71, 0x77, 0x6B, 0x02]),
        port: 1,
    };
    let mut slave = PtpPort::new(LOOPBACK, identity, 0).expect("join PTP groups");
    // Far faster than the profile default, so the servo gets its eight good samples
    // inside a test rather than inside eight seconds.
    slave.set_delay_interval(Duration::from_millis(30));

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut locked_at = None;
    while Instant::now() < deadline {
        slave.poll().expect("poll");
        if slave.lock_state() == LockState::Locked && locked_at.is_none() {
            locked_at = Some(Instant::now());
            // Keep going a little after lock so the assertions see a settled servo.
        }
        if locked_at.is_some_and(|t| t.elapsed() > Duration::from_millis(600)) {
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }

    let status = slave.status();
    stop.store(true, Ordering::Relaxed);
    master.join().ok();

    assert!(
        status.grandmaster.is_some(),
        "never found the grandmaster; stats {:?}",
        status.stats
    );
    assert_eq!(
        status.state,
        LockState::Locked,
        "never locked; offset {} ns, stats {:?}",
        status.offset_nanos,
        status.stats
    );
    assert!(status.steps >= 1, "a 5 ms initial offset should have been stepped, not slewed");
    assert!(
        status.stats.unmatched == 0,
        "sequence matching dropped {} messages",
        status.stats.unmatched
    );

    // Software timestamping on a loaded machine is worth tens of microseconds; this is
    // a sanity bound, not a specification.
    assert!(
        status.offset_nanos.abs() < 500_000,
        "residual offset {} ns is too large to call locked",
        status.offset_nanos
    );

    println!(
        "\nlocked to {} in {:.1}s: offset {:+.1} us, delay {:.1} us, {} step(s)\n\
         sync {} follow-up {} delay-resp {} announce {}\n",
        status.grandmaster.as_deref().unwrap_or("?"),
        locked_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0),
        status.offset_nanos as f64 / 1000.0,
        status.delay_nanos as f64 / 1000.0,
        status.steps,
        status.stats.syncs,
        status.stats.follow_ups,
        status.stats.delay_resps,
        status.stats.announces,
    );
}
