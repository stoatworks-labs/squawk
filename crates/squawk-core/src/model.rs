use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{DEFAULT_PTIME_SAMPLES, DEFAULT_SAMPLE_RATE, MAX_KEYS};

macro_rules! id_newtype {
    ($name:ident, $what:literal) => {
        #[doc = concat!("Stable identifier for a ", $what, ".")]
        ///
        /// Held as a slug rather than an integer so that config files stay readable and
        /// ids survive reordering, renaming and hand-editing.
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

id_newtype!(EndpointId, "endpoint");
id_newtype!(PartylineId, "partyline");

/// A complete authored system: the thing that is saved to and loaded from disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub system: SystemConfig,
    #[serde(default)]
    pub partylines: Vec<Partyline>,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// System-wide audio and identity settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    #[serde(default = "default_system_name")]
    pub name: String,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    /// Samples per RTP packet, and therefore the engine's block size.
    #[serde(default = "default_ptime")]
    pub ptime_samples: u32,
    /// Milliseconds to ramp a talk key in and out. Anything below about 2 ms is
    /// audible as a click on every press; anything above about 20 ms feels sluggish.
    #[serde(default = "default_talk_ramp_ms")]
    pub talk_ramp_ms: f32,
}

fn default_system_name() -> String {
    "squawk".to_owned()
}
fn default_sample_rate() -> u32 {
    DEFAULT_SAMPLE_RATE
}
fn default_ptime() -> u32 {
    DEFAULT_PTIME_SAMPLES
}
fn default_talk_ramp_ms() -> f32 {
    5.0
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            name: default_system_name(),
            sample_rate: default_sample_rate(),
            ptime_samples: default_ptime(),
            talk_ramp_ms: default_talk_ramp_ms(),
        }
    }
}

/// A partyline: a bus that every member talks onto and hears back minus themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Partyline {
    pub id: PartylineId,
    pub name: String,
    /// UI hint only — the engine never reads this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    /// Trim applied to the summed bus before mix-minus subtraction.
    #[serde(default)]
    pub bus_trim_db: f32,
}

impl Partyline {
    pub fn new(id: impl Into<PartylineId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            colour: None,
            bus_trim_db: 0.0,
        }
    }
}

/// Anything that can talk and listen: a desk, a phone, a beltpack, or a third-party
/// AES67 device patched in at the edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub id: EndpointId,
    pub name: String,
    #[serde(default)]
    pub kind: EndpointKind,
    /// Gain applied to this endpoint's microphone before it reaches any bus.
    #[serde(default)]
    pub input_gain_db: f32,
    /// Hard mute of the microphone. Independent of, and overriding, every talk key.
    #[serde(default)]
    pub input_muted: bool,
    #[serde(default)]
    pub keys: Vec<Key>,
}

impl Endpoint {
    pub fn new(id: impl Into<EndpointId>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind: EndpointKind::default(),
            input_gain_db: 0.0,
            input_muted: false,
            keys: Vec::new(),
        }
    }

    /// The lowest key slot not already in use, or `None` if the endpoint is full.
    pub fn free_slot(&self) -> Option<u8> {
        (0..MAX_KEYS as u8).find(|slot| !self.keys.iter().any(|k| k.slot == *slot))
    }

    /// Assign a target to the next free key slot.
    ///
    /// This is what "add endpoint to partyline" means in the UI — membership is a key,
    /// so creating membership is allocating one.
    pub fn assign(&mut self, target: KeyTarget) -> Option<&mut Key> {
        let slot = self.free_slot()?;
        self.keys.push(Key::new(slot, target));
        self.keys.last_mut()
    }

    pub fn key(&self, slot: u8) -> Option<&Key> {
        self.keys.iter().find(|k| k.slot == slot)
    }
}

/// What kind of client an endpoint is.
///
/// This decides which transport the server uses to reach it. `Desktop`, `Hardware` and
/// `Aes67External` get per-key AES67 streams; `Mobile` and `Browser` cannot — no phone
/// has PTP, and multicast over wifi is transmitted at the basic rate with no
/// retries — so they get a server-folded Opus mix instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointKind {
    #[default]
    Desktop,
    Hardware,
    Mobile,
    Browser,
    /// A third-party AES67 device with no squawk client — the server publishes and
    /// subscribes to plain streams and there is no control plane.
    Aes67External,
}

impl EndpointKind {
    /// Whether this kind receives discrete per-key AES67 streams.
    pub fn uses_aes67(self) -> bool {
        matches!(
            self,
            EndpointKind::Desktop | EndpointKind::Hardware | EndpointKind::Aes67External
        )
    }
}

/// One button on a panel: a listen path, and optionally a talk path, to one target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Key {
    /// Position on the panel, `0..MAX_KEYS`. Also selects which of the endpoint's
    /// AES67 streams carries this key's audio.
    pub slot: u8,
    pub target: KeyTarget,
    /// Authored default level. The client may change it live without rewriting config.
    #[serde(default)]
    pub listen_level_db: f32,
    #[serde(default)]
    pub listen_muted: bool,
    #[serde(default)]
    pub talk_mode: TalkMode,
    /// Display override. Defaults to the target's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl Key {
    pub fn new(slot: u8, target: KeyTarget) -> Self {
        Self {
            slot,
            target,
            listen_level_db: 0.0,
            listen_muted: false,
            talk_mode: TalkMode::default(),
            label: None,
        }
    }
}

/// Where a key points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeyTarget {
    /// A shared bus. The endpoint hears the bus minus its own contribution.
    Partyline(PartylineId),
    /// A point-to-point path to one other endpoint. The endpoint hears only that
    /// endpoint's microphone, so there is nothing to subtract.
    Direct(EndpointId),
}

/// How the talk button behaves. Enforced by the client; the server is told the
/// resulting state either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TalkMode {
    /// Press to latch on, press again to release.
    #[default]
    Latching,
    /// Talk only while held.
    Momentary,
    /// No talk path at all — this key only listens.
    ListenOnly,
}

impl TalkMode {
    pub fn can_talk(self) -> bool {
        !matches!(self, TalkMode::ListenOnly)
    }
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    pub fn endpoint(&self, id: &EndpointId) -> Option<&Endpoint> {
        self.endpoints.iter().find(|e| &e.id == id)
    }

    pub fn endpoint_mut(&mut self, id: &EndpointId) -> Option<&mut Endpoint> {
        self.endpoints.iter_mut().find(|e| &e.id == id)
    }

    pub fn partyline(&self, id: &PartylineId) -> Option<&Partyline> {
        self.partylines.iter().find(|p| &p.id == id)
    }

    /// Every endpoint with a key targeting this partyline, with the slot of that key.
    ///
    /// This is the derived membership described in the crate docs — there is no stored
    /// member list to fall out of step with the keys.
    pub fn members_of(&self, id: &PartylineId) -> Vec<(&Endpoint, u8)> {
        self.endpoints
            .iter()
            .flat_map(|e| {
                e.keys
                    .iter()
                    .filter(move |k| k.target == KeyTarget::Partyline(id.clone()))
                    .map(move |k| (e, k.slot))
            })
            .collect()
    }

    /// How many AES67 streams the server must transmit to satisfy this config.
    ///
    /// One per key on every AES67 endpoint. Useful as a sanity check before deploying:
    /// each stream is ~1.6 Mbit/s of L24 at 1 ms, and — the part that actually bites —
    /// 1000 packets per second that have to be batched into the same tick.
    pub fn aes67_stream_count(&self) -> usize {
        self.endpoints
            .iter()
            .filter(|e| e.kind.uses_aes67())
            .map(|e| e.keys.len())
            .sum()
    }

    /// Endpoint ids grouped by partyline, for UI rendering of the assignment matrix.
    pub fn membership_map(&self) -> BTreeMap<PartylineId, Vec<EndpointId>> {
        let mut map: BTreeMap<PartylineId, Vec<EndpointId>> = BTreeMap::new();
        for p in &self.partylines {
            map.insert(p.id.clone(), Vec::new());
        }
        for e in &self.endpoints {
            for k in &e.keys {
                if let KeyTarget::Partyline(pid) = &k.target {
                    map.entry(pid.clone()).or_default().push(e.id.clone());
                }
            }
        }
        map
    }
}

/// Convert decibels to a linear gain multiplier.
///
/// Anything at or below -120 dB returns exactly 0.0, so a muted path contributes
/// nothing at all rather than a denormal that costs CPU on every block.
pub fn db_to_linear(db: f32) -> f32 {
    if db <= -120.0 {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}
