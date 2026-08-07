//! Lock a real `PtpPort` to a synthetic grandmaster over real sockets.
//!
//! There is no grandmaster on the network this was developed on, so the alternative to
//! this test is shipping a PTP slave that has never once locked to anything.
//!
//! It exercises the whole stack together: multicast sockets, message encoding, the
//! BMCA, sequence matching and the servo. What it cannot exercise is a real network's
//! asymmetry, a switch's residence time, or another vendor's interpretation of the
//! standard.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use squawk_ptp::message::{ClockIdentity, PortIdentity};
use squawk_ptp::servo::LockState;
use squawk_ptp::testing::Grandmaster;
use squawk_ptp::PtpPort;

const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
/// Large enough to force a step rather than a slew.
const MASTER_OFFSET_NANOS: i64 = 5_000_000;

#[test]
fn the_slave_locks_to_a_grandmaster_over_real_sockets() {
    let gm = Grandmaster::spawn(LOOPBACK, 0, MASTER_OFFSET_NANOS);
    // Let it start announcing before the slave looks for it.
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
        }
        if locked_at.is_some_and(|t| t.elapsed() > Duration::from_millis(600)) {
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }

    let status = slave.status();
    drop(gm);

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
    assert_eq!(status.stats.unmatched, 0, "sequence matching dropped messages");

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

#[test]
fn the_slave_locks_on_a_domain_other_than_zero() {
    // Delay_Reqs must carry the configured domain. Sending them on domain 0 regardless
    // works by accident on domain 0 and fails completely everywhere else: the master
    // ignores them, no delay measurement ever completes, and the servo sits on its
    // first step forever while the grandmaster appears to announce but not answer.
    // SMPTE 2059-2 installations commonly run domain 127.
    const DOMAIN: u8 = 127;

    let gm = Grandmaster::spawn(LOOPBACK, DOMAIN, MASTER_OFFSET_NANOS);
    std::thread::sleep(Duration::from_millis(150));

    let identity = PortIdentity {
        clock: ClockIdentity::from_mac([0x02, 0x73, 0x71, 0x77, 0x6B, 0x03]),
        port: 1,
    };
    let mut slave = PtpPort::new(LOOPBACK, identity, DOMAIN).expect("join PTP groups");
    slave.set_delay_interval(Duration::from_millis(30));

    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline && slave.lock_state() != LockState::Locked {
        slave.poll().expect("poll");
        std::thread::sleep(Duration::from_micros(200));
    }

    let status = slave.status();
    drop(gm);

    assert_eq!(status.domain, DOMAIN, "the port should report its own domain");
    assert!(
        status.stats.delay_resps > 0,
        "no Delay_Resp ever came back — the requests are probably going out on the \
         wrong domain. stats {:?}",
        status.stats
    );
    assert_eq!(
        status.state,
        LockState::Locked,
        "never locked on domain {DOMAIN}; offset {} ns, stats {:?}",
        status.offset_nanos,
        status.stats
    );
}
