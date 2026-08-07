//! Does the server stamp its packets with PTP time?
//!
//! This is the property that makes squawk interoperable rather than self-contained.
//! AES67's `a=mediaclk:direct=0` means the RTP timestamp *is* the media clock, so two
//! devices locked to the same grandmaster produce the same timestamp for the same
//! instant — and a receiver can line up their streams without negotiating anything.
//!
//! A free-running sender fails this by an arbitrary margin: squawk's own free-running
//! counter starts from the stream's SSRC, so the gap would be measured in hours.
//!
//! Locking itself is `squawk-ptp`'s test, not this one. This asserts what the *server*
//! does with a clock once it has one.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use squawk_core::{Config, Endpoint, KeyTarget, Partyline};
use squawk_ptp::testing::Grandmaster;
use squawk_ptp::MediaClock;
use squawk_rtp::packet::{payload_range, RtpHeader};
use squawk_rtp::sdp::DEFAULT_RTP_PORT;
use squawk_rtp::{addressing, StreamSender};
use squawk_server::host::TransportOptions;
use squawk_server::AppState;

use std::net::{SocketAddrV4, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};

const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const BLOCK: usize = 48;
const RATE: u32 = 48_000;
/// Offset of the synthetic grandmaster from local time. Non-zero so that a server
/// ignoring PTP entirely cannot accidentally pass.
const GM_OFFSET_NANOS: i64 = 40_000_000; // 40 ms

fn one_endpoint() -> Config {
    let mut cfg = Config::default();
    cfg.partylines.push(Partyline::new("pl", "Production"));
    for id in ["a", "b"] {
        let mut e = Endpoint::new(id, id);
        e.assign(KeyTarget::Partyline("pl".into())).unwrap();
        cfg.endpoints.push(e);
    }
    cfg
}

/// A raw receiver, because this test needs the RTP header rather than the audio.
fn raw_receiver(group: Ipv4Addr, port: u16) -> UdpSocket {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    s.set_reuse_address(true).unwrap();
    #[cfg(unix)]
    s.set_reuse_port(true).unwrap();
    s.bind(&SocketAddrV4::new(group, port).into()).unwrap();
    s.join_multicast_v4(&group, &LOOPBACK).unwrap();
    s.set_nonblocking(true).unwrap();
    s.into()
}

#[test]
fn outgoing_timestamps_are_ptp_time_counted_in_samples() {
    let gm = Grandmaster::spawn(LOOPBACK, 0, GM_OFFSET_NANOS);
    std::thread::sleep(Duration::from_millis(200));

    let state = AppState::with_transport(
        one_endpoint(),
        None,
        Some(TransportOptions { iface: LOOPBACK, jitter_depth: 2, ptp_domain: Some(0) }),
    );

    // Feed a mic so the streams carry something, and key it up.
    let mut mic = StreamSender::new(
        LOOPBACK, addressing::mic_group(0), DEFAULT_RTP_PORT, BLOCK, 96, addressing::mic_ssrc(0), 1,
    )
    .unwrap();
    state.host().set_talk("a", 0, true);

    let ear = raw_receiver(addressing::key_group(1, 0), DEFAULT_RTP_PORT);
    let clock = MediaClock::new(RATE);

    // Wait for the servo to have applied its first correction. Full lock takes eight
    // delay exchanges at the profile's one-a-second, and is squawk-ptp's business.
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut stepped = false;
    while Instant::now() < deadline {
        mic.send(&[0.3; BLOCK]).ok();
        std::thread::sleep(Duration::from_micros(900));
        if let Some(p) = state.host().snapshot().ptp {
            if p.steps >= 1 && p.grandmaster.is_some() {
                stepped = true;
                break;
            }
        }
    }
    let ptp = state.host().snapshot().ptp.expect("PTP should be configured");
    assert!(
        stepped,
        "the server never disciplined its clock: {:?}",
        ptp
    );

    // Let it settle, then collect timestamps alongside the grandmaster's own time.
    std::thread::sleep(Duration::from_millis(300));
    let steps_before = state.host().snapshot().ptp.map(|p| p.steps).unwrap_or(0);

    // Discard everything the socket buffered while we waited for the servo. Those
    // packets are minutes old by media-clock standards and span the clock step, so
    // measuring them would be measuring history — which is exactly what made this test
    // report a timestamp discontinuity the server had not committed.
    {
        let mut scratch = [0u8; 2048];
        while ear.recv(&mut scratch).is_ok() {}
    }
    let mut buf = [0u8; 2048];
    let mut samples: Vec<(u32, u32)> = Vec::new(); // (received, expected)
    let mut previous: Option<(u16, u32)> = None; // (sequence, timestamp)
    let mut spacing_errors = 0;
    let mut dropped = 0u32;
    let mut discontinuities: Vec<(u16, u32, u16, u32)> = Vec::new();

    let collect_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < collect_until && samples.len() < 400 {
        mic.send(&[0.3; BLOCK]).ok();
        std::thread::sleep(Duration::from_micros(900));
        while let Ok(len) = ear.recv(&mut buf) {
            // Take the reference the instant the packet arrives.
            let expected = clock.rtp_timestamp(gm.now());
            let Ok((header, offset)) = RtpHeader::parse(&buf[..len]) else { continue };
            if payload_range(&buf[..len], offset).is_err() {
                continue;
            }
            // Check spacing against the *sequence number*, not against a fixed block.
            // Loopback drops the occasional packet under load, and a dropped packet
            // legitimately leaves a two-block timestamp gap — indistinguishable from a
            // sender fault unless you ask how many packets went missing.
            if let Some((prev_seq, prev_ts)) = previous {
                let seq_delta = header.sequence.wrapping_sub(prev_seq) as u32;
                if seq_delta > 1 {
                    dropped += seq_delta - 1;
                }
                if header.timestamp.wrapping_sub(prev_ts) != seq_delta * BLOCK as u32 {
                    spacing_errors += 1;
                    discontinuities.push((prev_seq, prev_ts, header.sequence, header.timestamp));
                }
            }
            previous = Some((header.sequence, header.timestamp));
            samples.push((header.timestamp, expected));
        }
    }

    assert!(samples.len() > 50, "only {} packets arrived", samples.len());

    // Uniform spacing matters more to a receiver than the absolute value does. The one
    // legitimate exception is a clock step: the timeline is snapped deliberately, since
    // the previous timestamps referred to a different notion of time.
    let steps_after = state.host().snapshot().ptp.map(|p| p.steps).unwrap_or(0);
    let allowed = (steps_after - steps_before) as usize;
    assert!(
        spacing_errors <= allowed,
        "{spacing_errors} timestamp discontinuities but only {allowed} clock step(s) to \
         explain them ({dropped} dropped). seq/ts pairs: {discontinuities:?}"
    );

    // Each packet's timestamp against PTP time at the moment it arrived. The gap is a
    // real latency — mixing, sending, looping back — so it is a window, not zero. What
    // matters is that it is *small and bounded*: a free-running counter starts from the
    // SSRC and would be out by hours.
    let deltas: Vec<i32> = samples
        .iter()
        .map(|(got, want)| got.wrapping_sub(*want) as i32)
        .collect();
    let worst = deltas.iter().map(|d| d.abs()).max().unwrap();
    let spread = deltas.iter().max().unwrap() - deltas.iter().min().unwrap();

    assert!(
        worst < RATE as i32 / 10,
        "timestamps are {worst} samples ({:.1} ms) from PTP time — that is not a media clock",
        worst as f64 * 1000.0 / RATE as f64
    );
    // A drifting clock would show up as a widening gap rather than a large one.
    assert!(
        spread < RATE as i32 / 20,
        "the gap to PTP time wandered by {spread} samples across the run"
    );

    let ptp = state.host().snapshot().ptp.unwrap();
    assert_eq!(ptp.realignments, 0, "the media timeline had to be snapped back to PTP");

    println!(
        "\n{} packets, spacing exact ({dropped} dropped on loopback)\n\
         gap to PTP time: worst {worst} samples ({:.2} ms), spread {spread} samples\n\
         ptp: {} gm {:?} offset {:+} ns, {} step(s)\n",
        samples.len(),
        worst as f64 * 1000.0 / RATE as f64,
        ptp.state,
        ptp.grandmaster,
        ptp.offset_nanos,
        ptp.steps,
    );
}

#[test]
fn without_ptp_the_timestamps_are_not_media_clock_time() {
    // The negative control: proves the test above is measuring something. A
    // free-running counter starts from the stream's SSRC and bears no relation to
    // wall-clock time at all.
    let state = AppState::with_transport(
        one_endpoint(),
        None,
        Some(TransportOptions { iface: LOOPBACK, jitter_depth: 2, ptp_domain: None }),
    );
    std::thread::sleep(Duration::from_millis(300));

    let ear = raw_receiver(addressing::key_group(1, 0), DEFAULT_RTP_PORT);
    let clock = MediaClock::new(RATE);
    let mut buf = [0u8; 2048];

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut worst: Option<i64> = None;
    while Instant::now() < deadline && worst.is_none() {
        std::thread::sleep(Duration::from_millis(20));
        while let Ok(len) = ear.recv(&mut buf) {
            let expected = clock.rtp_timestamp(squawk_ptp::Timestamp {
                seconds: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                nanos: 0,
            });
            let Ok((header, _)) = RtpHeader::parse(&buf[..len]) else { continue };
            worst = Some((header.timestamp.wrapping_sub(expected) as i32).abs() as i64);
            break;
        }
    }

    let worst = worst.expect("no packets arrived");
    // The same window the positive test uses. A wider bound is tempting and wrong:
    // the gap between a free-running counter and media-clock time is arbitrary modulo
    // 2^32, so it can land anywhere — and on the day this was written it landed within
    // a quarter of a second, because (unix_seconds * 48000) mod 2^32 happened to be
    // near zero. Matching the positive test's tolerance is what makes this a control
    // rather than a coin toss.
    assert!(
        worst > (RATE / 10) as i64,
        "a free-running counter landed within the positive test's window ({worst} \
         samples) — this control is not proving anything today"
    );

    assert!(state.host().snapshot().ptp.is_none(), "no PTP was configured");
}
