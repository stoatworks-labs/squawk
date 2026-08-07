//! Real AES67 in and out, replacing the simulated source when an interface is given.
//!
//! One receiver per endpoint microphone, one sender per key stream. Endpoints on the
//! Opus leg get neither: their audio arrives and leaves over WebRTC, which does not
//! exist yet, so for now they contribute silence and receive nothing. That is the
//! correct behaviour rather than a stub — putting a phone on a multicast group would
//! not work if we tried.

use std::io;
use std::net::Ipv4Addr;

use squawk_core::Config;
use squawk_engine::Engine;
use squawk_rtp::sdp::DEFAULT_RTP_PORT;
use squawk_rtp::{addressing, JitterStats, Pull, StreamReceiver, StreamSender};

/// Multicast TTL. 32 keeps streams inside the site and off any uplink.
const TTL: u32 = 32;

/// Health of one endpoint's inbound stream, for the UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StreamHealth {
    pub id: String,
    /// Packets buffered ahead of the playout point.
    pub depth: usize,
    pub lost: u64,
    pub late: u64,
    pub resyncs: u64,
    /// Mean depth minus target. Persistently non-zero means the sender is not locked
    /// to our clock, and no buffer size will save it.
    pub drift: Option<f32>,
    /// Packets dropped for carrying a different SSRC than the one we locked to.
    pub foreign: u64,
}

pub struct AudioIo {
    block: usize,
    /// Indexed by endpoint. `None` for endpoints on the Opus leg.
    mics: Vec<Option<StreamReceiver>>,
    /// Indexed by engine stream index. `None` for streams belonging to Opus endpoints.
    keys: Vec<Option<StreamSender>>,
    endpoint_ids: Vec<String>,
    send_errors: u64,
}

impl AudioIo {
    /// Bind every socket the config implies.
    ///
    /// `iface` must be the address of the NIC the audio network is on — see
    /// `squawk_rtp::transport` for why that is not something to let the OS decide.
    pub fn new(
        config: &Config,
        engine: &Engine,
        iface: Ipv4Addr,
        target_depth: usize,
    ) -> io::Result<Self> {
        let block = engine.block_size();

        let mut mics = Vec::with_capacity(config.endpoints.len());
        for (i, endpoint) in config.endpoints.iter().enumerate() {
            if !endpoint.kind.uses_aes67() {
                mics.push(None);
                continue;
            }
            // For `aes67-external` this address is wrong in principle: a third-party
            // device publishes on a group of its own choosing, which we would learn
            // from its SDP. Until SAP discovery exists we listen on the group we would
            // have allocated, which is right only for squawk's own clients.
            let group = addressing::mic_group(i);
            mics.push(Some(StreamReceiver::new(
                iface,
                group,
                DEFAULT_RTP_PORT,
                block,
                target_depth,
            )?));
        }

        let mut keys = Vec::with_capacity(engine.stream_count());
        for route in engine.routes() {
            let endpoint = &config.endpoints[route.endpoint];
            if !endpoint.kind.uses_aes67() {
                keys.push(None);
                continue;
            }
            let group = addressing::key_group(route.endpoint, route.slot);
            let ssrc = addressing::key_ssrc(route.endpoint, route.slot);
            keys.push(Some(StreamSender::new(
                iface,
                group,
                DEFAULT_RTP_PORT,
                block,
                96,
                ssrc,
                TTL,
            )?));
        }

        tracing::info!(
            receivers = mics.iter().filter(|m| m.is_some()).count(),
            senders = keys.iter().filter(|k| k.is_some()).count(),
            %iface,
            "AES67 transport bound"
        );

        Ok(Self {
            block,
            mics,
            keys,
            endpoint_ids: config.endpoints.iter().map(|e| e.id.0.clone()).collect(),
            send_errors: 0,
        })
    }

    /// Drain every socket into its jitter buffer. Call once per tick, before pulling.
    pub fn poll(&mut self) -> usize {
        let mut accepted = 0;
        for mic in self.mics.iter_mut().flatten() {
            match mic.poll() {
                Ok(n) => accepted += n,
                Err(err) => tracing::warn!(%err, "receive failed"),
            }
        }
        accepted
    }

    /// Fill the engine's input buffer, `endpoints * block` samples, endpoint-major.
    ///
    /// Every endpoint gets exactly one block whether or not a packet arrived — that is
    /// what puts them all on the same timeline, and it is the precondition the engine's
    /// mix-minus depends on.
    pub fn pull_inputs(&mut self, out: &mut [f32]) {
        debug_assert_eq!(out.len(), self.mics.len() * self.block);
        for (i, mic) in self.mics.iter_mut().enumerate() {
            let slice = &mut out[i * self.block..(i + 1) * self.block];
            match mic {
                Some(rx) => {
                    rx.pull(slice);
                }
                None => slice.fill(0.0),
            }
        }
    }

    /// Transmit one block of every key stream, all carrying the same RTP timestamp.
    ///
    /// One timestamp for the whole tick, not one per stream: these blocks *are* the
    /// same instant, and AES67 receivers use the timestamp to align streams from one
    /// sender. Letting each stream keep its own counter would make them mutually
    /// skewed by however long the send loop took.
    pub fn send_outputs_at(&mut self, engine_out: &[f32], timestamp: u32) -> usize {
        let mut sent = 0;
        for (i, key) in self.keys.iter_mut().enumerate() {
            let Some(tx) = key else { continue };
            let slice = &engine_out[i * self.block..(i + 1) * self.block];
            match tx.send_at(slice, timestamp) {
                Ok(_) => sent += 1,
                Err(err) => {
                    self.send_errors += 1;
                    // One log line per failure would itself become the problem at
                    // 1000 packets/sec, so this is sampled.
                    if self.send_errors.is_multiple_of(1000) {
                        tracing::warn!(%err, total = self.send_errors, "send failing");
                    }
                }
            }
        }
        sent
    }

    pub fn send_errors(&self) -> u64 {
        self.send_errors
    }

    pub fn health(&self) -> Vec<StreamHealth> {
        self.mics
            .iter()
            .enumerate()
            .filter_map(|(i, mic)| {
                let rx = mic.as_ref()?;
                let JitterStats { late, lost, resyncs, depth, .. } = rx.jitter().stats();
                Some(StreamHealth {
                    id: self.endpoint_ids.get(i).cloned().unwrap_or_default(),
                    depth,
                    lost,
                    late,
                    resyncs,
                    drift: rx.jitter().drift(),
                    foreign: rx.foreign_packets(),
                })
            })
            .collect()
    }
}

/// Whether a pull produced real audio, for metering purposes.
pub fn is_audio(pull: Pull) -> bool {
    matches!(pull, Pull::Filled)
}
