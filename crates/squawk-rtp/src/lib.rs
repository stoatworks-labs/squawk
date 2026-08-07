//! AES67 transport for the squawk intercom: RTP packetisation, SDP, and jitter
//! buffering.
//!
//! # What this crate is responsible for
//!
//! The mix engine's whole correctness argument rests on one precondition: **every input
//! block it receives is already aligned into the server's clock domain.** Mix-minus
//! subtracts the same buffer it added, and that is only exact if the samples line up.
//!
//! Satisfying that precondition is this crate's job, and it is the hard part. The RTP
//! coding is arithmetic; the jitter buffer is where the actual engineering is, because
//! it has to absorb network delay variation, reordering and loss *and* reconcile a
//! sender's idea of 48 kHz with the receiver's, which are never the same 48 kHz.
//!
//! # Layout
//!
//! - [`packet`] — RTP header and L24 coding, byte-exact against RFC 3550.
//! - [`sdp`] — AES67 session descriptions: strict generation, lenient parsing.
//! - [`jitter`] — the receive buffer that turns a packet stream into aligned blocks.
//! - [`transport`] — the UDP sockets, and the multicast group allocation.

pub mod addressing;
pub mod jitter;
pub mod packet;
pub mod sdp;
pub mod transport;

pub use addressing::{key_group, key_ssrc, mic_group, mic_ssrc, KEY_BASE, MIC_BASE};
pub use jitter::{JitterBuffer, JitterStats, Pull, Push};
pub use packet::{RtpError, RtpHeader, RTP_HEADER_LEN};
pub use sdp::{Direction, Encoding, RefClock, SdpError, StreamDescription};
pub use transport::{stream_group, StreamReceiver, StreamSender, DEFAULT_GROUP_BASE};
