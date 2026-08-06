use squawk_core::{
    Config, Endpoint, KeyTarget, Partyline, ProblemKind, Severity, TalkMode, MAX_KEYS,
};

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

fn has(cfg: &Config, want: &ProblemKind) -> bool {
    cfg.validate().iter().any(|p| &p.kind == want)
}

fn errors(cfg: &Config) -> Vec<ProblemKind> {
    cfg.validate()
        .into_iter()
        .filter(|p| p.severity == Severity::Error)
        .map(|p| p.kind)
        .collect()
}

#[test]
fn a_plain_two_person_partyline_is_clean() {
    let cfg = two_on_a_partyline();
    assert_eq!(cfg.validate(), vec![], "expected no problems at all");
    assert!(cfg.is_valid());
}

#[test]
fn two_keys_at_the_same_target_is_an_error() {
    // The mix-minus breaker: each key subtracts only its own contribution, so the
    // endpoint would hear itself back through the other key.
    let mut cfg = two_on_a_partyline();
    cfg.endpoints[0].assign(KeyTarget::Partyline("pl".into())).unwrap();
    assert!(has(
        &cfg,
        &ProblemKind::DuplicateKeyTarget { endpoint: "a".into(), slots: (0, 1) }
    ));
    assert!(!cfg.is_valid());
}

#[test]
fn a_key_pointing_at_a_partyline_that_does_not_exist_is_an_error() {
    let mut cfg = two_on_a_partyline();
    cfg.endpoints[0].assign(KeyTarget::Partyline("ghost".into())).unwrap();
    assert!(has(
        &cfg,
        &ProblemKind::UnknownPartyline {
            endpoint: "a".into(),
            slot: 1,
            target: "ghost".into()
        }
    ));
}

#[test]
fn a_direct_key_to_yourself_is_an_error() {
    let mut cfg = two_on_a_partyline();
    cfg.endpoints[0].assign(KeyTarget::Direct("a".into())).unwrap();
    assert!(has(&cfg, &ProblemKind::DirectToSelf { endpoint: "a".into(), slot: 1 }));
}

#[test]
fn duplicate_endpoint_ids_are_an_error() {
    let mut cfg = two_on_a_partyline();
    let mut dup = Endpoint::new("a", "another a");
    dup.assign(KeyTarget::Partyline("pl".into())).unwrap();
    cfg.endpoints.push(dup);
    assert!(has(&cfg, &ProblemKind::DuplicateEndpointId("a".into())));
}

#[test]
fn more_than_max_keys_is_an_error() {
    let mut cfg = two_on_a_partyline();
    for i in 0..MAX_KEYS {
        cfg.partylines.push(Partyline::new(format!("extra{i}"), format!("Extra {i}")));
    }
    // Bypass free_slot()'s limit by pushing keys directly, as a hand-edited file would.
    for i in 0..MAX_KEYS {
        cfg.endpoints[0].keys.push(squawk_core::Key::new(
            (i + 1) as u8,
            KeyTarget::Partyline(format!("extra{i}").as_str().into()),
        ));
    }
    assert!(has(
        &cfg,
        &ProblemKind::TooManyKeys { endpoint: "a".into(), count: MAX_KEYS + 1 }
    ));
}

#[test]
fn assign_refuses_to_overfill_an_endpoint() {
    let mut e = Endpoint::new("a", "a");
    for i in 0..MAX_KEYS {
        assert!(
            e.assign(KeyTarget::Partyline(format!("pl{i}").as_str().into())).is_some(),
            "assign {i} should have succeeded"
        );
    }
    assert!(e.free_slot().is_none());
    assert!(e.assign(KeyTarget::Partyline("overflow".into())).is_none());
    assert_eq!(e.keys.len(), MAX_KEYS);
}

#[test]
fn a_one_way_direct_key_warns_but_still_builds() {
    let mut cfg = two_on_a_partyline();
    cfg.endpoints[0].assign(KeyTarget::Direct("b".into())).unwrap();

    assert!(has(&cfg, &ProblemKind::OneWayDirect { from: "a".into(), to: "b".into() }));
    // Warning only — patching one side before the other is a legitimate intermediate
    // state, and the engine renders the unfinished half as silence.
    assert_eq!(errors(&cfg), vec![]);
    assert!(cfg.is_valid());
}

#[test]
fn a_reciprocated_direct_pair_does_not_warn() {
    let mut cfg = two_on_a_partyline();
    cfg.endpoints[0].assign(KeyTarget::Direct("b".into())).unwrap();
    cfg.endpoints[1].assign(KeyTarget::Direct("a".into())).unwrap();
    assert_eq!(cfg.validate(), vec![]);
}

#[test]
fn empty_and_single_member_partylines_warn() {
    let mut cfg = two_on_a_partyline();
    cfg.partylines.push(Partyline::new("empty", "Nobody"));
    cfg.partylines.push(Partyline::new("lonely", "One"));
    cfg.endpoints[0].assign(KeyTarget::Partyline("lonely".into())).unwrap();

    assert!(has(&cfg, &ProblemKind::EmptyPartyline("empty".into())));
    assert!(has(&cfg, &ProblemKind::LonelyPartyline("lonely".into())));
    assert_eq!(errors(&cfg), vec![]);
}

#[test]
fn an_endpoint_that_can_only_listen_warns() {
    let mut cfg = two_on_a_partyline();
    cfg.endpoints[0].keys[0].talk_mode = TalkMode::ListenOnly;
    assert!(has(&cfg, &ProblemKind::EndpointCannotTalk("a".into())));
}

#[test]
fn membership_is_derived_from_keys() {
    let cfg = two_on_a_partyline();
    let members = cfg.members_of(&"pl".into());
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].0.id.as_str(), "a");
    assert_eq!(members[1].0.id.as_str(), "b");

    let map = cfg.membership_map();
    assert_eq!(map[&"pl".into()].len(), 2);
}

#[test]
fn config_round_trips_through_toml() {
    let mut cfg = two_on_a_partyline();
    cfg.endpoints[0].keys[0].listen_level_db = -3.0;
    cfg.endpoints[0].keys[0].talk_mode = TalkMode::Momentary;
    cfg.endpoints[1].input_gain_db = 6.0;
    cfg.partylines[0].colour = Some("#c8462d".into());

    let text = cfg.to_toml().expect("serialise");
    let back = Config::from_toml(&text).expect("deserialise");

    assert_eq!(back.partylines.len(), 1);
    assert_eq!(back.partylines[0].colour.as_deref(), Some("#c8462d"));
    assert_eq!(back.endpoints[0].keys[0].talk_mode, TalkMode::Momentary);
    assert_eq!(back.endpoints[0].keys[0].listen_level_db, -3.0);
    assert_eq!(back.endpoints[1].input_gain_db, 6.0);
    assert_eq!(back.validate(), vec![]);
}

#[test]
fn a_minimal_config_file_parses_with_defaults() {
    let text = r#"
        [[partylines]]
        id = "pl"
        name = "Production"

        [[endpoints]]
        id = "a"
        name = "Stage Left"
        kind = "hardware"

        [[endpoints.keys]]
        slot = 0
        target = { partyline = "pl" }
    "#;
    let cfg = Config::from_toml(text).expect("parse");
    assert_eq!(cfg.system.sample_rate, squawk_core::DEFAULT_SAMPLE_RATE);
    assert_eq!(cfg.system.ptime_samples, squawk_core::DEFAULT_PTIME_SAMPLES);
    assert_eq!(cfg.endpoints[0].kind, squawk_core::EndpointKind::Hardware);
    assert_eq!(cfg.endpoints[0].keys[0].talk_mode, TalkMode::Latching);
}

#[test]
fn a_sample_rate_no_aes67_device_must_support_is_an_error() {
    let mut cfg = two_on_a_partyline();
    cfg.system.sample_rate = 44_101;
    assert!(matches!(
        errors(&cfg).as_slice(),
        [ProblemKind::UnsupportedSampleRate(44_101), ..]
    ));
}
