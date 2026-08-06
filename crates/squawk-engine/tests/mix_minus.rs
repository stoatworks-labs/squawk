//! Behavioural tests for the mix-minus engine.
//!
//! Everything here drives the engine with DC levels rather than tones. Mix-minus is a
//! sample-exact subtraction, so DC lets each assertion be an exact equality against a
//! hand-computed sum — a tone would only let us assert something statistical, and would
//! hide precisely the small residues (a talker leaking into their own ear at -40 dB)
//! that these tests exist to catch.
//!
//! Test levels are kept low enough that the output limiter never engages; anything
//! testing the limiter belongs in its own unit tests.

use squawk_core::{Config, Endpoint, EndpointKind, KeyTarget, Partyline, TalkMode};
use squawk_engine::{Command, Engine};

const BLOCK: usize = 48;
/// Comfortably longer than the 5 ms default talk ramp (240 samples = 5 blocks).
const SETTLE: usize = 20;
const EPS: f32 = 1e-6;

/// A partyline with `ids` assigned to it, one key each in slot 0.
fn partyline_config(ids: &[&str]) -> Config {
    let mut cfg = Config::default();
    cfg.partylines.push(Partyline::new("pl", "Production"));
    for id in ids {
        let mut e = Endpoint::new(*id, *id);
        e.assign(KeyTarget::Partyline("pl".into()));
        cfg.endpoints.push(e);
    }
    cfg
}

/// Push `blocks` blocks of per-endpoint DC through the engine.
fn feed(eng: &mut Engine, dc: &[f32], blocks: usize) {
    let mut buf = vec![0.0f32; dc.len() * BLOCK];
    for (i, v) in dc.iter().enumerate() {
        buf[i * BLOCK..(i + 1) * BLOCK].fill(*v);
    }
    for _ in 0..blocks {
        eng.process(&buf);
    }
}

/// The steady value on one endpoint's key feed. Panics if the block is not flat, which
/// would mean a ramp had not settled and the test is asserting on a transient.
fn feed_value(eng: &Engine, endpoint: usize, slot: u8) -> f32 {
    let s = eng.stream_index(endpoint, slot).expect("stream exists");
    let out = eng.stream_output(s);
    let first = out[0];
    for (i, v) in out.iter().enumerate() {
        assert!(
            (v - first).abs() < EPS,
            "block is not flat at sample {i}: {v} vs {first} — a ramp has not settled"
        );
    }
    first
}

fn talk_all(eng: &mut Engine, n: usize) {
    for i in 0..n {
        eng.apply(Command::SetTalk { endpoint: i, slot: 0, on: true });
    }
}

#[test]
fn three_talkers_each_hear_the_other_two() {
    let cfg = partyline_config(&["a", "b", "c"]);
    let mut eng = Engine::new(&cfg).unwrap();
    talk_all(&mut eng, 3);

    let dc = [0.1, 0.2, 0.3];
    feed(&mut eng, &dc, SETTLE);

    assert!((feed_value(&eng, 0, 0) - 0.5).abs() < EPS, "a should hear b+c");
    assert!((feed_value(&eng, 1, 0) - 0.4).abs() < EPS, "b should hear a+c");
    assert!((feed_value(&eng, 2, 0) - 0.3).abs() < EPS, "c should hear a+b");
}

#[test]
fn a_lone_talker_hears_exact_silence() {
    let cfg = partyline_config(&["a", "b"]);
    let mut eng = Engine::new(&cfg).unwrap();
    eng.apply(Command::SetTalk { endpoint: 0, slot: 0, on: true });

    feed(&mut eng, &[0.5, 0.5], SETTLE);

    // b is not talking, so a's own contribution is the entire bus and must cancel to
    // nothing at all. This is the assertion the whole design exists to satisfy.
    assert_eq!(
        feed_value(&eng, 0, 0),
        0.0,
        "talker leaked into their own feed"
    );
    assert!((feed_value(&eng, 1, 0) - 0.5).abs() < EPS, "b should hear a");
}

#[test]
fn bus_trim_does_not_leak_the_talker_into_their_own_feed() {
    // The trap this guards: if the trim were applied to the summed bus instead of to
    // each contribution, mix-minus would subtract an untrimmed buffer from a trimmed
    // sum and leave (1 - trim) of the talker in their own ear.
    let mut cfg = partyline_config(&["a", "b"]);
    cfg.partylines[0].bus_trim_db = -6.0;
    let mut eng = Engine::new(&cfg).unwrap();
    talk_all(&mut eng, 2);

    feed(&mut eng, &[0.4, 0.0], SETTLE);

    assert_eq!(feed_value(&eng, 0, 0), 0.0, "trim leaked the talker into their own feed");

    let trim = 10f32.powf(-6.0 / 20.0);
    assert!(
        (feed_value(&eng, 1, 0) - 0.4 * trim).abs() < EPS,
        "b should hear a at the trimmed level"
    );
}

#[test]
fn listen_only_key_never_reaches_the_bus() {
    let mut cfg = partyline_config(&["a", "b"]);
    cfg.endpoints[0].keys[0].talk_mode = TalkMode::ListenOnly;
    let mut eng = Engine::new(&cfg).unwrap();

    // Try to make it talk anyway — the engine must refuse.
    talk_all(&mut eng, 2);
    feed(&mut eng, &[0.5, 0.2], SETTLE);

    assert!(
        (feed_value(&eng, 1, 0) - 0.0).abs() < EPS,
        "b heard a listen-only endpoint"
    );
    assert!((feed_value(&eng, 0, 0) - 0.2).abs() < EPS, "a should still hear b");
}

#[test]
fn input_mute_overrides_a_latched_talk_key() {
    let cfg = partyline_config(&["a", "b"]);
    let mut eng = Engine::new(&cfg).unwrap();
    talk_all(&mut eng, 2);
    feed(&mut eng, &[0.3, 0.0], SETTLE);
    assert!((feed_value(&eng, 1, 0) - 0.3).abs() < EPS);

    eng.apply(Command::SetInputMute { endpoint: 0, muted: true });
    feed(&mut eng, &[0.3, 0.0], SETTLE);
    assert_eq!(feed_value(&eng, 1, 0), 0.0, "hard mute did not override talk");
}

#[test]
fn listen_level_scales_only_the_listener() {
    let cfg = partyline_config(&["a", "b", "c"]);
    let mut eng = Engine::new(&cfg).unwrap();
    talk_all(&mut eng, 3);
    eng.apply(Command::SetListenLevel { endpoint: 0, slot: 0, db: -6.0 });

    feed(&mut eng, &[0.1, 0.2, 0.3], SETTLE);

    let trim = 10f32.powf(-6.0 / 20.0);
    assert!((feed_value(&eng, 0, 0) - 0.5 * trim).abs() < EPS, "a's own level");
    assert!((feed_value(&eng, 1, 0) - 0.4).abs() < EPS, "b unaffected by a's level");
}

#[test]
fn direct_key_carries_only_the_far_end() {
    let mut cfg = Config::default();
    // A partyline both are also on, to prove the direct path does not pick up the bus.
    cfg.partylines.push(Partyline::new("pl", "Production"));
    for id in ["a", "b", "c"] {
        let mut e = Endpoint::new(id, id);
        e.assign(KeyTarget::Partyline("pl".into()));
        cfg.endpoints.push(e);
    }
    cfg.endpoints[0].assign(KeyTarget::Direct("b".into()));
    cfg.endpoints[1].assign(KeyTarget::Direct("a".into()));

    let mut eng = Engine::new(&cfg).unwrap();
    talk_all(&mut eng, 3);
    eng.apply(Command::SetTalk { endpoint: 0, slot: 1, on: true });
    eng.apply(Command::SetTalk { endpoint: 1, slot: 1, on: true });

    feed(&mut eng, &[0.1, 0.2, 0.3], SETTLE);

    assert!(
        (feed_value(&eng, 0, 1) - 0.2).abs() < EPS,
        "a's direct key should carry only b, not the partyline"
    );
    assert!((feed_value(&eng, 1, 1) - 0.1).abs() < EPS, "b's direct key should carry only a");
    // And the partyline keys are unchanged by the direct pair existing.
    assert!((feed_value(&eng, 2, 0) - 0.3).abs() < EPS, "c still hears a+b");
}

#[test]
fn a_direct_key_with_no_key_back_is_silent() {
    let mut cfg = partyline_config(&["a", "b"]);
    cfg.endpoints[0].assign(KeyTarget::Direct("b".into()));
    // b deliberately gets no reciprocal key. The validator warns; the engine must run
    // and render silence rather than panic or route the bus into it.
    let mut eng = Engine::new(&cfg).unwrap();
    talk_all(&mut eng, 2);
    eng.apply(Command::SetTalk { endpoint: 0, slot: 1, on: true });

    feed(&mut eng, &[0.1, 0.2], SETTLE);

    assert_eq!(feed_value(&eng, 0, 1), 0.0, "one-way direct key should be silent");
}

#[test]
fn talk_engages_without_a_step_discontinuity() {
    let cfg = partyline_config(&["a", "b"]);
    let mut eng = Engine::new(&cfg).unwrap();

    let dc = [0.5f32, 0.0];
    let mut buf = vec![0.0f32; 2 * BLOCK];
    buf[0..BLOCK].fill(dc[0]);

    // One block with talk off, then press and watch b's feed climb.
    eng.process(&buf);
    eng.apply(Command::SetTalk { endpoint: 0, slot: 0, on: true });

    let listener = eng.stream_index(1, 0).unwrap();
    let mut prev = 0.0f32;
    let mut max_step = 0.0f32;
    let mut reached = 0.0f32;

    for _ in 0..SETTLE {
        eng.process(&buf);
        for &s in eng.stream_output(listener) {
            max_step = max_step.max((s - prev).abs());
            prev = s;
            reached = reached.max(s);
        }
    }

    // 5 ms at 48 kHz is 240 samples, so a 0.5 target should move ~0.00208 per sample.
    assert!(
        max_step < 0.005,
        "talk engaged with a {max_step} step — that is an audible click"
    );
    assert!((reached - 0.5).abs() < EPS, "ramp should reach unity, got {reached}");
}

#[test]
fn releasing_talk_ramps_back_to_silence() {
    let cfg = partyline_config(&["a", "b"]);
    let mut eng = Engine::new(&cfg).unwrap();
    eng.apply(Command::SetTalk { endpoint: 0, slot: 0, on: true });
    feed(&mut eng, &[0.5, 0.0], SETTLE);
    assert!((feed_value(&eng, 1, 0) - 0.5).abs() < EPS);

    eng.apply(Command::ClearAllTalk);
    feed(&mut eng, &[0.5, 0.0], SETTLE);
    assert_eq!(feed_value(&eng, 1, 0), 0.0, "talk release did not reach silence");
}

#[test]
fn stream_indices_are_allocated_in_endpoint_then_slot_order() {
    // The RTP layer maps stream index to a destination and pins it for the life of a
    // config, so this ordering is load-bearing, not incidental.
    let mut cfg = partyline_config(&["a", "b"]);
    cfg.endpoints[0].assign(KeyTarget::Direct("b".into()));
    cfg.endpoints[1].assign(KeyTarget::Direct("a".into()));
    let eng = Engine::new(&cfg).unwrap();

    let routes = eng.routes();
    assert_eq!(routes.len(), 4);
    assert_eq!((routes[0].endpoint, routes[0].slot), (0, 0));
    assert_eq!((routes[1].endpoint, routes[1].slot), (0, 1));
    assert_eq!((routes[2].endpoint, routes[2].slot), (1, 0));
    assert_eq!((routes[3].endpoint, routes[3].slot), (1, 1));
}

#[test]
fn a_config_with_errors_will_not_build() {
    let mut cfg = partyline_config(&["a", "b"]);
    // Two keys at the same target — the mix-minus breaker.
    cfg.endpoints[0].assign(KeyTarget::Partyline("pl".into()));
    assert!(Engine::new(&cfg).is_err());
}

#[test]
fn meters_track_inputs_and_buses() {
    let cfg = partyline_config(&["a", "b"]);
    let mut eng = Engine::new(&cfg).unwrap();
    talk_all(&mut eng, 2);
    feed(&mut eng, &[0.4, 0.2], SETTLE);

    let m = eng.meters();
    assert!((m.inputs[0] - 0.4).abs() < EPS);
    assert!((m.inputs[1] - 0.2).abs() < EPS);
    assert!((m.buses[0] - 0.6).abs() < EPS, "bus should carry the full sum");
    // Outputs are post mix-minus, so each is the sum minus its own contribution.
    assert!((m.outputs[0] - 0.2).abs() < EPS);
    assert!((m.outputs[1] - 0.4).abs() < EPS);
}

#[test]
fn aes67_stream_count_excludes_opus_only_endpoints() {
    let mut cfg = partyline_config(&["a", "b", "phone"]);
    cfg.endpoints[2].kind = EndpointKind::Mobile;
    // Two AES67 endpoints with one key each.
    assert_eq!(cfg.aes67_stream_count(), 2);
    // But the engine still mixes for all three — the fold happens downstream.
    let eng = Engine::new(&cfg).unwrap();
    assert_eq!(eng.stream_count(), 3);
}
