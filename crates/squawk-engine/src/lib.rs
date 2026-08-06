//! Block-based mix-minus engine for the squawk partyline intercom.
//!
//! # Shape
//!
//! One [`Engine::process`] call consumes exactly one block of microphone audio from
//! every endpoint and produces exactly one block for every key stream. The block size
//! is the AES67 packet time in samples (48 at the 1 ms / 48 kHz default), so one engine
//! block is one RTP packet and there is no re-blocking anywhere in the path.
//!
//! # Mix-minus, and why it is cheap
//!
//! The naive reading of "every member hears everyone but themselves" is a per-member
//! sum, which is O(members²) per partyline. It is not necessary. Each contribution is
//! computed once, the bus is summed once, and every member's feed is that one sum with
//! their own contribution subtracted — O(members).
//!
//! The subtraction is exact rather than approximate because it removes precisely the
//! buffer that was added: same samples, same gain, same block. That only holds because
//! every input has already been resampled and aligned into the server's clock domain by
//! the jitter buffers upstream. An engine fed unaligned inputs would leak the talker
//! back into their own ear, quietly and unfixably.
//!
//! # Transport independence
//!
//! The engine emits one stream per key and knows nothing about how a stream reaches its
//! endpoint. AES67 clients take their streams individually; Opus clients get a fold-down
//! of the same streams performed downstream. Keeping the fold outside the engine means
//! there is exactly one mixing implementation to reason about and test.
//!
//! # Threading
//!
//! [`Engine`] is a plain `&mut self` state machine with no locks, allocation or I/O in
//! [`Engine::process`]. Control changes arrive through [`Engine::apply`] as
//! [`Command`]s. The host is expected to own an SPSC queue and drain it between blocks;
//! the engine deliberately does not own that queue, so it stays testable in-process.

mod limiter;

use squawk_core::{db_to_linear, Config, EndpointId, KeyTarget, Severity};
use thiserror::Error;

pub use limiter::Limiter;

/// Why a config could not be compiled into a routing table.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("config has {0} validation error(s); first: {1}")]
    Invalid(usize, String),
}

/// A control change applied between blocks.
///
/// Everything that can change while audio is running is expressed here so that the host
/// can funnel it through one lock-free queue rather than reaching into engine state.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Press or release a talk key.
    SetTalk { endpoint: usize, slot: u8, on: bool },
    /// Change a key's listen level in dB.
    SetListenLevel { endpoint: usize, slot: u8, db: f32 },
    /// Mute or unmute a key's listen path.
    SetListenMute { endpoint: usize, slot: u8, muted: bool },
    /// Change an endpoint's microphone gain in dB.
    SetInputGain { endpoint: usize, db: f32 },
    /// Hard-mute an endpoint's microphone, overriding every talk key it holds.
    SetInputMute { endpoint: usize, muted: bool },
    /// Release every latched talk key in the system.
    ClearAllTalk,
}

/// Where a key's audio comes from, resolved to indices at compile time so the audio
/// path never touches a string.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    /// Sum of this bus, minus this key's own contribution.
    Bus(usize),
    /// The single key on the far endpoint that points back at this one. `None` when the
    /// far end has no reciprocal key — the config validator warns about this, and the
    /// engine renders it as silence rather than refusing to run.
    Direct(Option<usize>),
}

struct KeyRt {
    slot: u8,
    source: Source,
    /// Flat index of this key's output stream, and of its contribution scratch.
    stream: usize,
    /// Which bus this key's contribution is summed onto, if any.
    ///
    /// `None` covers two different cases, which is why it is not the test for whether a
    /// contribution gets *produced*: a listen-only key (nothing to sum) and a direct
    /// key (something to produce, but no bus to sum it onto — the far end reads the
    /// contribution buffer directly). Production is gated on `can_talk` instead.
    contributes_to: Option<usize>,
    /// Gain folded into the contribution as it is written: the bus trim for a bus key,
    /// unity for a direct key.
    contrib_trim: f32,
    listen_gain: f32,
    listen_muted: bool,
    can_talk: bool,
    talk_on: bool,
    /// Current position of the talk fade, 0.0..=1.0. Ramped per sample so that
    /// pressing talk does not put a step discontinuity onto a live bus.
    ramp: f32,
    limiter: Limiter,
}

struct EndpointRt {
    id: EndpointId,
    input_gain: f32,
    input_muted: bool,
    keys: Vec<KeyRt>,
}

/// Which endpoint and key slot a flat stream index belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRoute {
    pub endpoint: usize,
    pub slot: u8,
}

/// Peak levels for one block, for UI metering. Linear, not dB.
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Peak of each endpoint's microphone, post input gain, pre talk gate.
    pub inputs: Vec<f32>,
    /// Peak of each bus sum.
    pub buses: Vec<f32>,
    /// Peak of each output stream, post limiter.
    pub outputs: Vec<f32>,
}

pub struct Engine {
    block: usize,
    /// Per-sample increment of the talk ramp, derived from `talk_ramp_ms`.
    ramp_step: f32,
    endpoints: Vec<EndpointRt>,
    bus_ids: Vec<String>,

    /// `n_endpoints * block`. Microphone audio after input gain and hard mute.
    mic: Vec<f32>,
    /// `n_streams * block`. What each key contributed to its bus this block — the exact
    /// buffer that mix-minus subtracts again.
    contrib: Vec<f32>,
    /// `n_buses * block`.
    bus: Vec<f32>,
    /// `n_streams * block`. The engine's output.
    out: Vec<f32>,

    routes: Vec<StreamRoute>,
    meters: Meters,
}

impl Engine {
    /// Compile a config into a routing table.
    ///
    /// Fails if the config has validation errors; warnings are ignored, since a
    /// one-way direct key or an empty partyline is a legitimate intermediate state
    /// while an operator is patching.
    pub fn new(config: &Config) -> Result<Self, BuildError> {
        let problems = config.validate();
        let errors: Vec<_> = problems
            .iter()
            .filter(|p| p.severity == Severity::Error)
            .collect();
        if !errors.is_empty() {
            return Err(BuildError::Invalid(errors.len(), errors[0].message.clone()));
        }

        let block = config.system.ptime_samples as usize;
        let sample_rate = config.system.sample_rate as f32;

        let bus_ids: Vec<String> = config.partylines.iter().map(|p| p.id.0.clone()).collect();
        let bus_trims: Vec<f32> = config
            .partylines
            .iter()
            .map(|p| db_to_linear(p.bus_trim_db))
            .collect();
        let bus_index = |id: &str| bus_ids.iter().position(|b| b == id);
        let ep_index = |id: &EndpointId| config.endpoints.iter().position(|e| &e.id == id);

        // Pass one: allocate a stream index for every key, in endpoint-then-slot order.
        // The RTP layer relies on this ordering being stable for the life of a config.
        let mut routes = Vec::new();
        let mut stream_of: Vec<Vec<usize>> = Vec::with_capacity(config.endpoints.len());
        for (ei, e) in config.endpoints.iter().enumerate() {
            let mut per_key = Vec::with_capacity(e.keys.len());
            for k in &e.keys {
                per_key.push(routes.len());
                routes.push(StreamRoute { endpoint: ei, slot: k.slot });
            }
            stream_of.push(per_key);
        }

        // Pass two: resolve sources. Direct keys need the far endpoint's reciprocal key,
        // which is why this cannot be done in one pass.
        let mut endpoints = Vec::with_capacity(config.endpoints.len());
        for (ei, e) in config.endpoints.iter().enumerate() {
            let mut keys = Vec::with_capacity(e.keys.len());
            for (ki, k) in e.keys.iter().enumerate() {
                let (source, contributes_to, contrib_trim) = match &k.target {
                    KeyTarget::Partyline(pid) => {
                        let b = bus_index(pid.as_str()).expect("validated");
                        let contributes = k.talk_mode.can_talk().then_some(b);
                        (Source::Bus(b), contributes, bus_trims[b])
                    }
                    KeyTarget::Direct(other_id) => {
                        let oi = ep_index(other_id).expect("validated");
                        let back = config.endpoints[oi]
                            .keys
                            .iter()
                            .position(|ok| ok.target == KeyTarget::Direct(e.id.clone()))
                            .map(|oki| stream_of[oi][oki]);
                        (Source::Direct(back), None, 1.0)
                    }
                };

                keys.push(KeyRt {
                    slot: k.slot,
                    source,
                    stream: stream_of[ei][ki],
                    contributes_to,
                    listen_gain: db_to_linear(k.listen_level_db),
                    listen_muted: k.listen_muted,
                    contrib_trim,
                    can_talk: k.talk_mode.can_talk(),
                    talk_on: false,
                    ramp: 0.0,
                    limiter: Limiter::new(sample_rate),
                });
            }

            endpoints.push(EndpointRt {
                id: e.id.clone(),
                input_gain: db_to_linear(e.input_gain_db),
                input_muted: e.input_muted,
                keys,
            });
        }

        let n_ep = endpoints.len();
        let n_bus = bus_ids.len();
        let n_streams = routes.len();

        let ramp_ms = config.system.talk_ramp_ms.max(0.1);
        let ramp_step = 1.0 / (ramp_ms * 0.001 * sample_rate);

        Ok(Self {
            block,
            ramp_step,
            endpoints,
            bus_ids,
            mic: vec![0.0; n_ep * block],
            contrib: vec![0.0; n_streams * block],
            bus: vec![0.0; n_bus * block],
            out: vec![0.0; n_streams * block],
            routes,
            meters: Meters {
                inputs: vec![0.0; n_ep],
                buses: vec![0.0; n_bus],
                outputs: vec![0.0; n_streams],
            },
        })
    }

    pub fn block_size(&self) -> usize {
        self.block
    }
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }
    pub fn bus_count(&self) -> usize {
        self.bus_ids.len()
    }
    pub fn stream_count(&self) -> usize {
        self.routes.len()
    }
    /// Flat stream index → which endpoint and key slot it feeds.
    pub fn routes(&self) -> &[StreamRoute] {
        &self.routes
    }
    pub fn meters(&self) -> &Meters {
        &self.meters
    }

    /// Index of an endpoint by id. For the control plane, not the audio path.
    pub fn endpoint_index(&self, id: &EndpointId) -> Option<usize> {
        self.endpoints.iter().position(|e| &e.id == id)
    }

    /// Flat stream index for one endpoint's key slot.
    pub fn stream_index(&self, endpoint: usize, slot: u8) -> Option<usize> {
        self.endpoints
            .get(endpoint)?
            .keys
            .iter()
            .find(|k| k.slot == slot)
            .map(|k| k.stream)
    }

    /// One output block, `block_size()` samples, for a flat stream index.
    pub fn stream_output(&self, stream: usize) -> &[f32] {
        &self.out[stream * self.block..(stream + 1) * self.block]
    }

    /// The whole output buffer, `stream_count() * block_size()` samples, laid out
    /// stream-major. Handed straight to the RTP layer for batched transmission.
    pub fn output(&self) -> &[f32] {
        &self.out
    }

    pub fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::SetTalk { endpoint, slot, on } => {
                if let Some(k) = self.key_mut(endpoint, slot) {
                    if k.can_talk {
                        k.talk_on = on;
                    }
                }
            }
            Command::SetListenLevel { endpoint, slot, db } => {
                if let Some(k) = self.key_mut(endpoint, slot) {
                    k.listen_gain = db_to_linear(db);
                }
            }
            Command::SetListenMute { endpoint, slot, muted } => {
                if let Some(k) = self.key_mut(endpoint, slot) {
                    k.listen_muted = muted;
                }
            }
            Command::SetInputGain { endpoint, db } => {
                if let Some(e) = self.endpoints.get_mut(endpoint) {
                    e.input_gain = db_to_linear(db);
                }
            }
            Command::SetInputMute { endpoint, muted } => {
                if let Some(e) = self.endpoints.get_mut(endpoint) {
                    e.input_muted = muted;
                }
            }
            Command::ClearAllTalk => {
                for e in &mut self.endpoints {
                    for k in &mut e.keys {
                        k.talk_on = false;
                    }
                }
            }
        }
    }

    fn key_mut(&mut self, endpoint: usize, slot: u8) -> Option<&mut KeyRt> {
        self.endpoints
            .get_mut(endpoint)?
            .keys
            .iter_mut()
            .find(|k| k.slot == slot)
    }

    /// Process one block.
    ///
    /// `mic_in` is `endpoint_count() * block_size()` samples, endpoint-major: every
    /// sample of endpoint 0, then every sample of endpoint 1, and so on. An endpoint
    /// with nothing connected supplies silence.
    ///
    /// No allocation, no locks and no I/O happen here.
    pub fn process(&mut self, mic_in: &[f32]) {
        let block = self.block;
        debug_assert_eq!(mic_in.len(), self.endpoints.len() * block);

        self.stage_inputs(mic_in);
        self.stage_contributions();
        self.stage_buses();
        self.stage_outputs();
    }

    /// Apply input gain and hard mute, and meter the result.
    fn stage_inputs(&mut self, mic_in: &[f32]) {
        let block = self.block;
        for (ei, e) in self.endpoints.iter().enumerate() {
            let g = if e.input_muted { 0.0 } else { e.input_gain };
            let src = &mic_in[ei * block..(ei + 1) * block];
            let dst = &mut self.mic[ei * block..(ei + 1) * block];
            let mut peak = 0.0f32;
            for (d, s) in dst.iter_mut().zip(src) {
                let v = s * g;
                *d = v;
                peak = peak.max(v.abs());
            }
            self.meters.inputs[ei] = peak;
        }
    }

    /// Advance each talk ramp and write what every key contributes this block.
    ///
    /// The bus trim is folded in here rather than applied to the summed bus, so that
    /// the buffer subtracted by mix-minus is byte-for-byte the buffer that was added.
    /// Trimming after the sum would leave a residue of the talker in their own feed
    /// scaled by (1 - trim) — inaudible at unity, and a mystery at any other setting.
    fn stage_contributions(&mut self) {
        let block = self.block;
        let step = self.ramp_step;
        for (ei, e) in self.endpoints.iter_mut().enumerate() {
            let mic = &self.mic[ei * block..(ei + 1) * block];
            for k in &mut e.keys {
                let dst = &mut self.contrib[k.stream * block..(k.stream + 1) * block];

                // Gated on `can_talk`, not on `contributes_to`: a direct key has no bus
                // but must still produce a contribution, because that buffer is what
                // the far end's direct key reads as its source.
                if !k.can_talk {
                    dst.fill(0.0);
                    continue;
                }
                let trim = k.contrib_trim;
                let target = if k.talk_on { 1.0 } else { 0.0 };

                if k.ramp == target {
                    // Steady state, which is the overwhelmingly common case: skip the
                    // per-sample ramp arithmetic entirely.
                    if target == 0.0 {
                        dst.fill(0.0);
                    } else {
                        for (d, m) in dst.iter_mut().zip(mic) {
                            *d = m * trim;
                        }
                    }
                } else {
                    let mut r = k.ramp;
                    for (d, m) in dst.iter_mut().zip(mic) {
                        r = if target > r {
                            (r + step).min(target)
                        } else {
                            (r - step).max(target)
                        };
                        *d = m * r * trim;
                    }
                    k.ramp = r;
                }
            }
        }
    }

    /// Sum every contribution onto its bus. This is the only O(members) pass.
    fn stage_buses(&mut self) {
        let block = self.block;
        self.bus.fill(0.0);
        for e in &self.endpoints {
            for k in &e.keys {
                let Some(bus) = k.contributes_to else { continue };
                let src = &self.contrib[k.stream * block..(k.stream + 1) * block];
                let dst = &mut self.bus[bus * block..(bus + 1) * block];
                for (d, s) in dst.iter_mut().zip(src) {
                    *d += s;
                }
            }
        }
        for b in 0..self.bus_ids.len() {
            let slice = &self.bus[b * block..(b + 1) * block];
            self.meters.buses[b] = slice.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        }
    }

    /// Produce every key's feed: bus minus own contribution, or the far end's
    /// contribution for a direct key, then listen gain and a peak limiter.
    ///
    /// The limiter sits per output rather than on the bus on purpose. Limiting the bus
    /// before subtraction would make the subtraction wrong — the gain reduction applies
    /// to the sum but not to the contribution being removed, so the talker leaks back
    /// into their own ear exactly when the bus is busiest and they would least notice
    /// why.
    fn stage_outputs(&mut self) {
        let block = self.block;
        for e in &mut self.endpoints {
            for k in &mut e.keys {
                let o = k.stream * block;
                let gain = if k.listen_muted { 0.0 } else { k.listen_gain };

                match k.source {
                    Source::Bus(b) => {
                        let bus = &self.bus[b * block..(b + 1) * block];
                        let own = &self.contrib[o..o + block];
                        for i in 0..block {
                            self.out[o + i] = (bus[i] - own[i]) * gain;
                        }
                    }
                    Source::Direct(Some(src_stream)) => {
                        let src = src_stream * block;
                        for i in 0..block {
                            self.out[o + i] = self.contrib[src + i] * gain;
                        }
                    }
                    Source::Direct(None) => {
                        self.out[o..o + block].fill(0.0);
                    }
                }

                let peak = k.limiter.process(&mut self.out[o..o + block]);
                self.meters.outputs[k.stream] = peak;
            }
        }
    }
}
