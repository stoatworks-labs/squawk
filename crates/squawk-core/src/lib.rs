//! Data model, configuration and validation for the squawk partyline intercom.
//!
//! This crate holds the *authored* state of a system: which partylines exist, which
//! endpoints exist, and which key on which endpoint points at what. It deliberately
//! holds no runtime state — talk buttons, meters and connection status live in the
//! engine and the server, not here.
//!
//! # The one modelling decision worth knowing
//!
//! **Partyline membership is derived from key assignments.** There is no separate
//! member list on [`Partyline`]. An endpoint is a member of a partyline if and only if
//! one of its keys targets that partyline.
//!
//! This is on purpose. A membership list *and* a key list is two sources of truth for
//! the same fact, and they drift the moment anything edits one without the other — the
//! classic intercom-config bug where a panel shows a key that routes nowhere. The UI
//! still offers "assign this endpoint to that partyline"; it just implements it by
//! allocating a free key slot.

mod model;
mod validate;

pub use model::{
    db_to_linear, Config, Endpoint, EndpointId, EndpointKind, Key, KeyTarget, Partyline,
    PartylineId, SystemConfig, TalkMode,
};
pub use validate::{Problem, ProblemKind, Severity};

/// Maximum keys per endpoint — one per AES67 stream a client may receive.
pub const MAX_KEYS: usize = 10;

/// The only sample rate AES67 requires every device to support.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// Samples per packet at the AES67 default 1 ms packet time, 48 kHz.
///
/// The mix engine ticks at exactly this block size so that one engine block is one RTP
/// packet. Re-blocking between the mixer and the network would add a buffer and a
/// latency wobble for no benefit.
pub const DEFAULT_PTIME_SAMPLES: u32 = 48;
