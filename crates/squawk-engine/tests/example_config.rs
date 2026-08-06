//! The shipped example config must always parse, validate clean and build an engine.
//!
//! Example files rot silently otherwise — a field gets renamed, and the first person to
//! hit it is someone copying the example as their starting point.

use squawk_core::{Config, EndpointKind};
use squawk_engine::Engine;

fn example() -> Config {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../squawk.example.toml");
    let text = std::fs::read_to_string(path).expect("example config is where README says");
    Config::from_toml(&text).expect("example config parses")
}

#[test]
fn the_example_config_has_no_problems_at_all() {
    let cfg = example();
    assert_eq!(
        cfg.validate(),
        vec![],
        "the shipped example should be a clean bill of health, warnings included"
    );
}

#[test]
fn the_example_config_builds_an_engine() {
    let cfg = example();
    let eng = Engine::new(&cfg).expect("example builds");

    assert_eq!(eng.endpoint_count(), 8);
    assert_eq!(eng.bus_count(), 4);
    assert_eq!(eng.block_size(), 48);

    // 18 keys in total across 8 endpoints.
    assert_eq!(eng.stream_count(), 18);
    // The phone's 2 keys are folded downstream, so only 16 of those are AES67.
    assert_eq!(cfg.aes67_stream_count(), 16);
}

#[test]
fn the_example_puts_the_phone_on_the_opus_leg() {
    let cfg = example();
    let phone = cfg.endpoint(&"roving".into()).expect("roving exists");
    assert_eq!(phone.kind, EndpointKind::Mobile);
    assert!(!phone.kind.uses_aes67());
}

#[test]
fn the_example_direct_pair_is_reciprocated_and_carries_audio() {
    let cfg = example();
    let mut eng = Engine::new(&cfg).unwrap();

    let sm = eng.endpoint_index(&"sm".into()).unwrap();
    let producer = eng.endpoint_index(&"producer".into()).unwrap();

    // Producer keys their private line to the stage manager.
    eng.apply(squawk_engine::Command::SetTalk { endpoint: producer, slot: 1, on: true });

    let mut buf = vec![0.0f32; eng.endpoint_count() * 48];
    buf[producer * 48..(producer + 1) * 48].fill(0.4);
    for _ in 0..20 {
        eng.process(&buf);
    }

    let s = eng.stream_index(sm, 4).expect("sm's direct key");
    assert!(
        (eng.stream_output(s)[0] - 0.4).abs() < 1e-6,
        "the stage manager should hear the producer on the private line"
    );
}
