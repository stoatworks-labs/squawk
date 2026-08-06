use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Config, EndpointId, KeyTarget, PartylineId};
use crate::MAX_KEYS;

/// How badly a [`Problem`] matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The config is coherent but probably not what was meant.
    Warning,
    /// The config cannot be turned into a working routing table.
    Error,
}

/// A specific thing wrong with a config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemKind {
    DuplicateEndpointId(EndpointId),
    DuplicatePartylineId(PartylineId),
    TooManyKeys { endpoint: EndpointId, count: usize },
    KeySlotOutOfRange { endpoint: EndpointId, slot: u8 },
    DuplicateKeySlot { endpoint: EndpointId, slot: u8 },
    DuplicateKeyTarget { endpoint: EndpointId, slots: (u8, u8) },
    UnknownPartyline { endpoint: EndpointId, slot: u8, target: PartylineId },
    UnknownEndpoint { endpoint: EndpointId, slot: u8, target: EndpointId },
    DirectToSelf { endpoint: EndpointId, slot: u8 },
    UnsupportedSampleRate(u32),
    PtimeNotWholeMilliseconds { ptime: u32, sample_rate: u32 },
    OneWayDirect { from: EndpointId, to: EndpointId },
    EmptyPartyline(PartylineId),
    LonelyPartyline(PartylineId),
    EndpointWithNoKeys(EndpointId),
    EndpointCannotTalk(EndpointId),
}

/// A validation finding, ready to render in the UI next to the offending row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub severity: Severity,
    pub kind: ProblemKind,
    /// Human-readable explanation, including *why* it matters where that is not obvious.
    pub message: String,
}

impl Problem {
    fn error(kind: ProblemKind, message: impl Into<String>) -> Self {
        Self { severity: Severity::Error, kind, message: message.into() }
    }

    fn warning(kind: ProblemKind, message: impl Into<String>) -> Self {
        Self { severity: Severity::Warning, kind, message: message.into() }
    }
}

impl Config {
    /// Check the config for anything that would break the routing table or surprise
    /// the operator. Errors must be fixed before the engine will build; warnings are
    /// advisory and the engine will run regardless.
    pub fn validate(&self) -> Vec<Problem> {
        let mut out = Vec::new();

        self.check_system(&mut out);
        self.check_unique_ids(&mut out);
        self.check_keys(&mut out);
        self.check_reciprocity(&mut out);
        self.check_population(&mut out);

        out.sort_by_key(|p| std::cmp::Reverse(p.severity));
        out
    }

    /// True if nothing in [`Config::validate`] is an error.
    pub fn is_valid(&self) -> bool {
        !self.validate().iter().any(|p| p.severity == Severity::Error)
    }

    fn check_system(&self, out: &mut Vec<Problem>) {
        let sr = self.system.sample_rate;
        if !matches!(sr, 44_100 | 48_000 | 96_000) {
            out.push(Problem::error(
                ProblemKind::UnsupportedSampleRate(sr),
                format!(
                    "sample rate {sr} is not one an AES67 device is required to support; \
                     use 48000 unless the whole system is deliberately elsewhere"
                ),
            ));
        }
        // AES67 packet times are expressed in whole milliseconds (125 us and 250 us
        // exist in the standard but are optional and rare); a block that is not a
        // whole number of samples per millisecond cannot line up with one.
        if self.system.ptime_samples == 0 || !sr.is_multiple_of(self.system.ptime_samples.max(1)) {
            out.push(Problem::error(
                ProblemKind::PtimeNotWholeMilliseconds { ptime: self.system.ptime_samples, sample_rate: sr },
                format!(
                    "packet time of {} samples does not divide the {sr} Hz sample rate evenly, \
                     so one engine block cannot be one RTP packet",
                    self.system.ptime_samples
                ),
            ));
        }
    }

    fn check_unique_ids(&self, out: &mut Vec<Problem>) {
        let mut seen = BTreeSet::new();
        for e in &self.endpoints {
            if !seen.insert(&e.id) {
                out.push(Problem::error(
                    ProblemKind::DuplicateEndpointId(e.id.clone()),
                    format!("two endpoints share the id '{}'", e.id),
                ));
            }
        }
        let mut seen = BTreeSet::new();
        for p in &self.partylines {
            if !seen.insert(&p.id) {
                out.push(Problem::error(
                    ProblemKind::DuplicatePartylineId(p.id.clone()),
                    format!("two partylines share the id '{}'", p.id),
                ));
            }
        }
    }

    fn check_keys(&self, out: &mut Vec<Problem>) {
        for e in &self.endpoints {
            if e.keys.len() > MAX_KEYS {
                out.push(Problem::error(
                    ProblemKind::TooManyKeys { endpoint: e.id.clone(), count: e.keys.len() },
                    format!(
                        "'{}' has {} keys; the limit is {MAX_KEYS}, one per receivable stream",
                        e.id,
                        e.keys.len()
                    ),
                ));
            }

            let mut slots = BTreeSet::new();
            let mut targets: BTreeMap<String, u8> = BTreeMap::new();

            for k in &e.keys {
                if k.slot as usize >= MAX_KEYS {
                    out.push(Problem::error(
                        ProblemKind::KeySlotOutOfRange { endpoint: e.id.clone(), slot: k.slot },
                        format!("'{}' key slot {} is outside 0..{MAX_KEYS}", e.id, k.slot),
                    ));
                }
                if !slots.insert(k.slot) {
                    out.push(Problem::error(
                        ProblemKind::DuplicateKeySlot { endpoint: e.id.clone(), slot: k.slot },
                        format!("'{}' has two keys in slot {}", e.id, k.slot),
                    ));
                }

                // Two keys pointing at the same place is the one config mistake that
                // silently breaks mix-minus: each key's feed subtracts only its own
                // contribution, so the endpoint hears itself back through the other key.
                let tag = match &k.target {
                    KeyTarget::Partyline(p) => format!("p:{p}"),
                    KeyTarget::Direct(d) => format!("d:{d}"),
                };
                if let Some(&first) = targets.get(&tag) {
                    out.push(Problem::error(
                        ProblemKind::DuplicateKeyTarget {
                            endpoint: e.id.clone(),
                            slots: (first, k.slot),
                        },
                        format!(
                            "'{}' points keys {first} and {} at the same target; mix-minus \
                             subtracts each key's own contribution only, so this endpoint \
                             would hear itself back through the other key",
                            e.id, k.slot
                        ),
                    ));
                } else {
                    targets.insert(tag, k.slot);
                }

                match &k.target {
                    KeyTarget::Partyline(pid) => {
                        if self.partyline(pid).is_none() {
                            out.push(Problem::error(
                                ProblemKind::UnknownPartyline {
                                    endpoint: e.id.clone(),
                                    slot: k.slot,
                                    target: pid.clone(),
                                },
                                format!("'{}' key {} targets unknown partyline '{pid}'", e.id, k.slot),
                            ));
                        }
                    }
                    KeyTarget::Direct(eid) => {
                        if eid == &e.id {
                            out.push(Problem::error(
                                ProblemKind::DirectToSelf { endpoint: e.id.clone(), slot: k.slot },
                                format!("'{}' key {} is a direct connection to itself", e.id, k.slot),
                            ));
                        } else if self.endpoint(eid).is_none() {
                            out.push(Problem::error(
                                ProblemKind::UnknownEndpoint {
                                    endpoint: e.id.clone(),
                                    slot: k.slot,
                                    target: eid.clone(),
                                },
                                format!("'{}' key {} targets unknown endpoint '{eid}'", e.id, k.slot),
                            ));
                        }
                    }
                }
            }
        }
    }

    /// A direct key only carries audio if the other end has one pointing back. The UI
    /// creates both halves; a hand-edited config may not, so say so rather than
    /// leaving a key that appears to work and does nothing.
    fn check_reciprocity(&self, out: &mut Vec<Problem>) {
        for e in &self.endpoints {
            for k in &e.keys {
                let KeyTarget::Direct(other_id) = &k.target else { continue };
                let Some(other) = self.endpoint(other_id) else { continue };
                let reciprocated = other
                    .keys
                    .iter()
                    .any(|ok| ok.target == KeyTarget::Direct(e.id.clone()));
                if !reciprocated {
                    out.push(Problem::warning(
                        ProblemKind::OneWayDirect { from: e.id.clone(), to: other_id.clone() },
                        format!(
                            "'{}' has a direct key to '{other_id}' but '{other_id}' has none back, \
                             so '{}' can hear it and cannot be heard by it",
                            e.id, e.id
                        ),
                    ));
                }
            }
        }
    }

    fn check_population(&self, out: &mut Vec<Problem>) {
        for p in &self.partylines {
            let members = self.members_of(&p.id);
            match members.len() {
                0 => out.push(Problem::warning(
                    ProblemKind::EmptyPartyline(p.id.clone()),
                    format!("partyline '{}' has no endpoints assigned to it", p.id),
                )),
                1 => out.push(Problem::warning(
                    ProblemKind::LonelyPartyline(p.id.clone()),
                    format!(
                        "partyline '{}' has one member, which will only ever hear silence",
                        p.id
                    ),
                )),
                _ => {}
            }
        }

        for e in &self.endpoints {
            if e.keys.is_empty() {
                out.push(Problem::warning(
                    ProblemKind::EndpointWithNoKeys(e.id.clone()),
                    format!("'{}' has no keys, so it is connected to nothing", e.id),
                ));
            } else if !e.keys.iter().any(|k| k.talk_mode.can_talk()) {
                out.push(Problem::warning(
                    ProblemKind::EndpointCannotTalk(e.id.clone()),
                    format!("'{}' has only listen-only keys and can never be heard", e.id),
                ));
            }
        }
    }
}
