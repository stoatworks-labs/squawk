//! IEEE 1588-2008 (PTPv2) for the squawk intercom.
//!
//! # What this is for
//!
//! AES67 devices agree on time by all slaving to one grandmaster, and their RTP
//! timestamps *are* that time counted in samples. Two senders locked to the same
//! grandmaster produce identical timestamps for the same instant, which is what lets a
//! receiver line up streams from different manufacturers without negotiating anything.
//!
//! Without it, squawk is a self-contained island: its own clock is the reference, which
//! works perfectly as long as nothing else on the network has an opinion.
//!
//! # Software timestamping, and what it costs
//!
//! Proper PTP takes its timestamps in the NIC, at the instant the packet crosses the
//! wire. This crate timestamps in userspace, when the kernel hands the packet over —
//! which includes interrupt latency, scheduling and whatever else the machine was doing.
//!
//! That is tens of microseconds of noise. At 48 kHz one sample is 20.8 us, so a
//! software-timestamped slave is roughly **±1 sample**, against the sub-microsecond a
//! hardware-timestamped one achieves. For speech on an intercom that is inaudible and
//! entirely adequate. For phase-coherent summing of the same source arriving by two
//! paths, it is not. Do not describe this as sample-accurate.
//!
//! macOS in particular exposes no hardware timestamping at all, so on the machine this
//! was developed on there is no better option available.
//!
//! # Layout
//!
//! - [`message`] — PTPv2 message coding.
//! - [`bmca`] — the Best Master Clock Algorithm.
//! - [`servo`] — offset and delay measurement, the servo, and the media clock.

pub mod bmca;
pub mod message;
pub mod port;
pub mod servo;
pub mod shared;
pub mod slave;
pub mod testing;

pub use bmca::{BestMaster, MasterDataset};
pub use port::{PtpHandle, PtpPort, PtpStatus};
pub use shared::SharedClock;
pub use slave::{Event, SlaveState, SlaveStats};
pub use message::{
    Body, ClockIdentity, ClockQuality, Header, Message, MessageType, PortIdentity, PtpError,
    Timestamp,
};
pub use servo::{measure, DelaySample, LockState, MediaClock, Measurement, Servo, SyncSample};

use std::net::Ipv4Addr;

/// The PTP primary multicast address (IEEE 1588-2008 annex D).
pub const PTP_PRIMARY: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);

/// Event port: Sync and Delay_Req, the messages whose transmission instant matters.
pub const PORT_EVENT: u16 = 319;

/// General port: Follow_Up, Delay_Resp, Announce.
pub const PORT_GENERAL: u16 = 320;

/// The AES67 media profile's default domain.
///
/// Note this differs from the IEEE 1588 default of 0, and from SMPTE 2059-2's 127. A
/// slave listening on the wrong domain sees a network full of PTP traffic and concludes
/// there is no grandmaster, which is a confusing way to spend an afternoon.
pub const AES67_DEFAULT_DOMAIN: u8 = 0;
