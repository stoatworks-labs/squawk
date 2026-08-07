//! End-to-end: audio into the server over real AES67 multicast, mixed, and back out.
//!
//! This is the test that makes the two halves of the project one thing. Everything
//! before it proved the engine mixes correctly, or that packets survive a socket. This
//! one drives the actual server binary's audio path from outside, over the loopback
//! interface, and asserts that mix-minus still holds after a round trip through RTP.
//!
//! It runs on `lo0`, which is MULTICAST-capable, so it needs no audio network — but for
//! the same reason it cannot prove anything about interface selection, switch IGMP
//! behaviour, or a real NIC's timing.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use squawk_core::{Config, Endpoint, KeyTarget, Partyline};
use squawk_rtp::sdp::DEFAULT_RTP_PORT;
use squawk_rtp::{addressing, Pull, StreamReceiver, StreamSender};
use squawk_server::host::TransportOptions;
use squawk_server::AppState;

const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const BLOCK: usize = 48;
/// Long enough to settle the 5 ms talk ramp and gather a stable run of blocks.
const TICKS: usize = 700;

fn two_on_a_partyline() -> Config {
    let mut cfg = Config::default();
    cfg.partylines.push(Partyline::new("pl", "Production"));
    for id in ["a", "b"] {
        let mut e = Endpoint::new(id, id);
        e.assign(KeyTarget::Partyline("pl".into())).unwrap();
        cfg.endpoints.push(e);
    }
    cfg
}

/// Busy-wait to a deadline.
///
/// The test sender has to keep the same 1 ms cadence as the server's audio loop. A
/// `thread::sleep(1ms)` overshoots by a variable amount, so the server would consume
/// faster than this fed it and the jitter buffer would underrun continuously — the test
/// would then be measuring `sleep` granularity rather than the audio path.
fn spin_until(deadline: Instant) {
    while Instant::now() < deadline {
        std::hint::spin_loop();
    }
}

#[test]
fn mix_minus_survives_a_round_trip_over_real_multicast() {
    let state = AppState::with_transport(
        two_on_a_partyline(),
        None,
        Some(TransportOptions { iface: LOOPBACK, jitter_depth: 2, ptp_domain: None }),
    );
    // Let the host thread bind its sockets before anything is sent at them.
    std::thread::sleep(Duration::from_millis(200));

    // Stand in for two hardware panels' microphones.
    let mut mic_a = StreamSender::new(
        LOOPBACK, addressing::mic_group(0), DEFAULT_RTP_PORT, BLOCK, 96, addressing::mic_ssrc(0), 1,
    )
    .unwrap();
    let mut mic_b = StreamSender::new(
        LOOPBACK, addressing::mic_group(1), DEFAULT_RTP_PORT, BLOCK, 96, addressing::mic_ssrc(1), 1,
    )
    .unwrap();

    // And for the two panels' ears.
    let mut ear_a =
        StreamReceiver::new(LOOPBACK, addressing::key_group(0, 0), DEFAULT_RTP_PORT, BLOCK, 2).unwrap();
    let mut ear_b =
        StreamReceiver::new(LOOPBACK, addressing::key_group(1, 0), DEFAULT_RTP_PORT, BLOCK, 2).unwrap();

    // Only 'a' talks. 'b' is keyed up but silent, so anything in a's ear is its own
    // voice coming back — which is the thing that must not happen.
    state.host().set_talk("a", 0, true);
    state.host().set_talk("b", 0, true);

    let mut a_heard: Vec<f32> = Vec::new();
    let mut b_heard: Vec<f32> = Vec::new();
    let mut buf = vec![0.0f32; BLOCK];
    let mut next = Instant::now();

    for tick in 0..TICKS {
        mic_a.send(&[0.4; BLOCK]).ok();
        mic_b.send(&[0.0; BLOCK]).ok();

        next += Duration::from_micros(1000);
        spin_until(next);

        ear_a.poll().ok();
        ear_b.poll().ok();

        // Ignore the first third: talk ramp, jitter pre-roll and thread start-up.
        let settled = tick > TICKS / 3;
        if ear_a.pull(&mut buf) == Pull::Filled && settled {
            a_heard.push(buf[0]);
        }
        if ear_b.pull(&mut buf) == Pull::Filled && settled {
            b_heard.push(buf[0]);
        }
    }

    assert!(
        a_heard.len() > 100 && b_heard.len() > 100,
        "not enough audio arrived to judge: a={} b={}",
        a_heard.len(),
        b_heard.len()
    );

    // The whole point. 'a' is talking; 'a' must hear nothing of itself, and no amount
    // of RTP, jitter buffering or concealment in between may change that.
    let a_worst = a_heard.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(
        a_worst < 0.001,
        "the talker heard themselves at {a_worst} after a round trip"
    );

    // 'b' must hear 'a' at the level 'a' sent. Peak rather than mean, so an occasional
    // concealed block does not drag the figure down and hide a real result.
    let b_peak = b_heard.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(
        (b_peak - 0.4).abs() < 0.01,
        "the listener heard {b_peak}, expected 0.4"
    );

    let snapshot = state.host().snapshot();
    assert!(!snapshot.simulated, "the server should be on real transport");
    assert_eq!(snapshot.health.len(), 2, "both endpoints should report receive health");
    for h in &snapshot.health {
        assert_eq!(h.foreign, 0, "{}: foreign packets on its group", h.id);
        assert_eq!(h.resyncs, 0, "{}: stream resynced mid-test", h.id);
    }

    println!(
        "\nround trip over multicast: {} blocks to a, {} to b\n\
         a (talking) peak {:.6}   b (listening) peak {:.4}\n\
         late ticks {}  depth {:?}\n",
        a_heard.len(),
        b_heard.len(),
        a_worst,
        b_peak,
        snapshot.late_ticks,
        snapshot.health.iter().map(|h| h.depth).collect::<Vec<_>>()
    );
}
