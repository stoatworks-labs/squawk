//! The engine host: owns the mix engine on its own thread and publishes a snapshot the
//! web layer can read.
//!
//! # This is not yet a real audio thread
//!
//! There is no transport, so there is nothing to pace the engine and nothing to feed it.
//! The host therefore runs a **simulated source** — a distinct sine per endpoint — and
//! paces itself off `Instant`, processing a batch of blocks per wake rather than
//! sleeping 1 ms at a time (no general-purpose OS will honour a 1 ms sleep reliably, and
//! pretending otherwise would only produce a convincing-looking lie).
//!
//! When the AES67 transport lands it replaces both of those: the PTP-derived clock paces
//! the loop, and the jitter buffers supply the input. Everything below that boundary —
//! the command queue, the talk-intent map, the snapshot — stays as it is.
//!
//! # Why talk intent lives here and not in the engine
//!
//! Editing the config rebuilds the engine, and a rebuilt engine starts with every key
//! released. If the engine were the only record of who is talking, adding a partyline
//! would silently drop every live talk key in the building. So the host keeps the
//! authoritative intent, keyed by endpoint id and slot — names that survive a rebuild,
//! unlike the integer indices the engine uses — and re-applies it afterwards.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use squawk_core::Config;
use squawk_engine::{Command, Engine};

/// How often the host publishes a meter snapshot. Also the UI's refresh rate.
const PUBLISH_INTERVAL: Duration = Duration::from_millis(50);

/// Blocks processed per wake-up. At 1 ms per block this is a 20 ms batch.
const BLOCKS_PER_WAKE: usize = 20;

/// A level reading tagged with the id it belongs to.
///
/// Tagged rather than positional on purpose: the config can be edited between two
/// snapshots, and a positional array would let the UI paint endpoint 3's level onto
/// endpoint 2's meter for one frame after any reorder.
#[derive(Debug, Clone, Serialize)]
pub struct Level {
    pub id: String,
    /// Peak over the publish interval, in dBFS. `-120.0` stands in for silence.
    pub db: f32,
}

/// What the UI is shown, once per publish interval.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    /// Increments on every rebuild, so the UI knows to refetch the config.
    pub generation: u64,
    pub inputs: Vec<Level>,
    pub buses: Vec<Level>,
    /// What each key is actually hearing, keyed `"endpoint_id:slot"`.
    ///
    /// This is the reading that makes mix-minus visible: key a talker up and every
    /// other member's feed rises while the talker's own stays at the floor.
    pub outputs: Vec<Level>,
    /// `"endpoint_id:slot"` for every key currently keyed.
    pub talking: Vec<String>,
    /// Blocks processed since start — a liveness check for the audio loop.
    pub blocks: u64,
}

enum HostCommand {
    Rebuild(Box<Config>),
    SetTalk { endpoint: String, slot: u8, on: bool },
    ClearAllTalk,
    Engine(Command),
}

/// Handle to the running host thread.
#[derive(Clone)]
pub struct Host {
    tx: Sender<HostCommand>,
    snapshot: Arc<Mutex<Snapshot>>,
}

impl Host {
    /// Spawn the host thread for a config that has already been validated.
    pub fn spawn(config: Config) -> Self {
        let (tx, rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));

        let thread_snapshot = Arc::clone(&snapshot);
        std::thread::Builder::new()
            .name("squawk-engine".into())
            .spawn(move || run(config, rx, thread_snapshot))
            .expect("spawn engine thread");

        Self { tx, snapshot }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().expect("snapshot lock").clone()
    }

    pub fn rebuild(&self, config: Config) {
        let _ = self.tx.send(HostCommand::Rebuild(Box::new(config)));
    }

    pub fn set_talk(&self, endpoint: &str, slot: u8, on: bool) {
        let _ = self.tx.send(HostCommand::SetTalk {
            endpoint: endpoint.to_owned(),
            slot,
            on,
        });
    }

    pub fn clear_all_talk(&self) {
        let _ = self.tx.send(HostCommand::ClearAllTalk);
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(HostCommand::Engine(cmd));
    }
}

/// Distinct sine per endpoint, so a mix of several is legible on a meter and would be
/// legible by ear. Replaced wholesale by the jitter buffers when the transport lands.
struct SimulatedSource {
    phase: Vec<f32>,
    step: Vec<f32>,
    buf: Vec<f32>,
}

impl SimulatedSource {
    fn new(endpoints: usize, block: usize, sample_rate: f32) -> Self {
        Self {
            phase: vec![0.0; endpoints],
            // 180 Hz upward in minor thirds-ish steps; nothing harmonically related, so
            // the sum never phase-cancels into a misleadingly quiet bus meter.
            step: (0..endpoints)
                .map(|i| {
                    let hz = 180.0 * 1.19f32.powi(i as i32);
                    std::f32::consts::TAU * hz / sample_rate
                })
                .collect(),
            buf: vec![0.0; endpoints * block],
        }
    }

    fn fill(&mut self, block: usize) -> &[f32] {
        for (i, phase) in self.phase.iter_mut().enumerate() {
            let step = self.step[i];
            let dst = &mut self.buf[i * block..(i + 1) * block];
            for d in dst.iter_mut() {
                *d = 0.3 * phase.sin();
                *phase += step;
                if *phase > std::f32::consts::TAU {
                    *phase -= std::f32::consts::TAU;
                }
            }
        }
        &self.buf
    }
}

fn linear_to_db(v: f32) -> f32 {
    if v <= 1e-6 {
        -120.0
    } else {
        20.0 * v.log10()
    }
}

fn run(mut config: Config, rx: Receiver<HostCommand>, snapshot: Arc<Mutex<Snapshot>>) {
    let mut engine = match Engine::new(&config) {
        Ok(e) => e,
        Err(err) => {
            tracing::error!(%err, "initial config would not build; host thread exiting");
            return;
        }
    };

    // Authoritative talk intent, keyed by names that survive a rebuild.
    let mut talk: BTreeMap<(String, u8), bool> = BTreeMap::new();
    let mut generation = 0u64;
    let mut blocks = 0u64;

    let mut source = SimulatedSource::new(
        config.endpoints.len(),
        engine.block_size(),
        config.system.sample_rate as f32,
    );

    // Peak-hold accumulators, reset each time we publish.
    let mut in_peak = vec![0.0f32; config.endpoints.len()];
    let mut bus_peak = vec![0.0f32; config.partylines.len()];
    let mut out_peak = vec![0.0f32; engine.stream_count()];

    let mut last_publish = Instant::now();
    let mut next_wake = Instant::now();

    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                HostCommand::Rebuild(new_config) => {
                    match Engine::new(&new_config) {
                        Ok(new_engine) => {
                            config = *new_config;
                            engine = new_engine;
                            generation += 1;
                            source = SimulatedSource::new(
                                config.endpoints.len(),
                                engine.block_size(),
                                config.system.sample_rate as f32,
                            );
                            in_peak = vec![0.0; config.endpoints.len()];
                            bus_peak = vec![0.0; config.partylines.len()];
                            out_peak = vec![0.0; engine.stream_count()];

                            // Re-apply talk intent for keys that still exist, and drop
                            // intent for keys that no longer do.
                            talk.retain(|(id, slot), _| {
                                config
                                    .endpoint(&id.as_str().into())
                                    .is_some_and(|e| e.key(*slot).is_some())
                            });
                            for ((id, slot), on) in &talk {
                                if let Some(ei) = engine.endpoint_index(&id.as_str().into()) {
                                    engine.apply(Command::SetTalk {
                                        endpoint: ei,
                                        slot: *slot,
                                        on: *on,
                                    });
                                }
                            }
                            tracing::info!(generation, "engine rebuilt");
                        }
                        Err(err) => {
                            // The web layer validates before sending, so this is a bug
                            // rather than user error — but dropping audio for it would
                            // be worse than carrying on with the config we have.
                            tracing::error!(%err, "rejected rebuild; keeping current engine");
                        }
                    }
                }
                HostCommand::SetTalk { endpoint, slot, on } => {
                    talk.insert((endpoint.clone(), slot), on);
                    if let Some(ei) = engine.endpoint_index(&endpoint.as_str().into()) {
                        engine.apply(Command::SetTalk { endpoint: ei, slot, on });
                    }
                }
                HostCommand::ClearAllTalk => {
                    talk.clear();
                    engine.apply(Command::ClearAllTalk);
                }
                HostCommand::Engine(c) => engine.apply(c),
            }
        }

        let block = engine.block_size();
        for _ in 0..BLOCKS_PER_WAKE {
            let input = source.fill(block);
            engine.process(input);
            blocks += 1;

            let m = engine.meters();
            for (p, v) in in_peak.iter_mut().zip(&m.inputs) {
                *p = p.max(*v);
            }
            for (p, v) in bus_peak.iter_mut().zip(&m.buses) {
                *p = p.max(*v);
            }
            for (p, v) in out_peak.iter_mut().zip(&m.outputs) {
                *p = p.max(*v);
            }
        }

        if last_publish.elapsed() >= PUBLISH_INTERVAL {
            let snap = Snapshot {
                generation,
                inputs: config
                    .endpoints
                    .iter()
                    .zip(&in_peak)
                    .map(|(e, v)| Level { id: e.id.0.clone(), db: linear_to_db(*v) })
                    .collect(),
                buses: config
                    .partylines
                    .iter()
                    .zip(&bus_peak)
                    .map(|(p, v)| Level { id: p.id.0.clone(), db: linear_to_db(*v) })
                    .collect(),
                outputs: engine
                    .routes()
                    .iter()
                    .zip(&out_peak)
                    .filter_map(|(route, v)| {
                        let ep = config.endpoints.get(route.endpoint)?;
                        Some(Level {
                            id: format!("{}:{}", ep.id.0, route.slot),
                            db: linear_to_db(*v),
                        })
                    })
                    .collect(),
                talking: talk
                    .iter()
                    .filter(|(_, on)| **on)
                    .map(|((id, slot), _)| format!("{id}:{slot}"))
                    .collect(),
                blocks,
            };
            *snapshot.lock().expect("snapshot lock") = snap;

            in_peak.fill(0.0);
            bus_peak.fill(0.0);
            out_peak.fill(0.0);
            last_publish = Instant::now();
        }

        // Pace off a fixed schedule rather than sleeping for the batch duration, so
        // scheduling jitter does not accumulate into drift.
        next_wake += Duration::from_millis(BLOCKS_PER_WAKE as u64);
        let now = Instant::now();
        if next_wake > now {
            std::thread::sleep(next_wake - now);
        } else {
            // Fell behind — give up the lost time rather than sprinting to catch up.
            next_wake = now;
        }
    }
}
