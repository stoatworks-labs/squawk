//! Scale test: a realistically large system, mixed for a full second of audio.
//!
//! This exists for two reasons. The first is correctness — mix-minus with 32 members on
//! a bus is where an O(members²) shortcut or an off-by-one in the flat buffer indexing
//! would show up, and it does not show up at three members. The second is that it is the
//! only honest way to talk about CPU cost before any of this touches a network card.
//!
//! Run `cargo test --release -- --nocapture` to see the timing.

use std::time::Instant;

use squawk_core::{Config, Endpoint, KeyTarget, Partyline};
use squawk_engine::{Command, Engine};

const ENDPOINTS: usize = 32;
const PARTYLINES: usize = 10;
const BLOCK: usize = 48;
/// One second of audio at 1 ms per block.
const BLOCKS: usize = 1000;

fn full_system() -> Config {
    let mut cfg = Config::default();
    for p in 0..PARTYLINES {
        cfg.partylines
            .push(Partyline::new(format!("pl{p}"), format!("Partyline {p}")));
    }
    for e in 0..ENDPOINTS {
        let mut ep = Endpoint::new(format!("ep{e}"), format!("Endpoint {e}"));
        for p in 0..PARTYLINES {
            ep.assign(KeyTarget::Partyline(format!("pl{p}").as_str().into()))
                .expect("within key limit");
        }
        cfg.endpoints.push(ep);
    }
    cfg
}

#[test]
fn a_fully_loaded_system_mixes_correctly_and_in_time() {
    let cfg = full_system();
    assert_eq!(cfg.aes67_stream_count(), ENDPOINTS * PARTYLINES);

    let mut eng = Engine::new(&cfg).unwrap();
    assert_eq!(eng.stream_count(), ENDPOINTS * PARTYLINES);

    // Everyone talking on every key at once — the worst case the engine can be put in.
    for e in 0..ENDPOINTS {
        for p in 0..PARTYLINES {
            eng.apply(Command::SetTalk { endpoint: e, slot: p as u8, on: true });
        }
    }

    // Levels chosen so the full 32-way sum stays under the limiter threshold, keeping
    // the mix-minus assertion exact.
    let dc: Vec<f32> = (0..ENDPOINTS).map(|i| 0.001 * (i + 1) as f32).collect();
    let total: f32 = dc.iter().sum();
    assert!(total < 0.89, "test levels would engage the limiter");

    let mut buf = vec![0.0f32; ENDPOINTS * BLOCK];
    for (i, v) in dc.iter().enumerate() {
        buf[i * BLOCK..(i + 1) * BLOCK].fill(*v);
    }

    let start = Instant::now();
    for _ in 0..BLOCKS {
        eng.process(&buf);
    }
    let elapsed = start.elapsed();

    // Every endpoint, on every partyline, hears the whole system minus itself.
    for (e, own) in dc.iter().enumerate() {
        let expected = total - own;
        for p in 0..PARTYLINES {
            let s = eng.stream_index(e, p as u8).unwrap();
            let got = eng.stream_output(s)[0];
            assert!(
                (got - expected).abs() < 1e-5,
                "endpoint {e} key {p}: expected {expected}, got {got}"
            );
        }
    }

    let realtime = elapsed.as_secs_f64();
    println!(
        "\n{ENDPOINTS} endpoints x {PARTYLINES} keys = {} streams, all talking\n\
         1.000 s of audio mixed in {:.1} ms  ({:.1}x realtime, {:.2}% of one core)\n",
        ENDPOINTS * PARTYLINES,
        realtime * 1000.0,
        1.0 / realtime,
        realtime * 100.0
    );

    // Generous bound — this is a smoke alarm for an accidental O(n²), not a benchmark.
    assert!(
        realtime < 0.5,
        "one second of audio took {realtime:.3} s to mix; that is far too close to \
         realtime for a debug-mode safety margin"
    );
}
