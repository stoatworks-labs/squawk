//! AES67 session descriptions (RFC 4566 SDP, constrained by AES67 §7).
//!
//! Generation is strict — squawk emits exactly the lines an AES67 receiver expects.
//! Parsing is deliberately lenient, because it has to swallow whatever a Dante, Ravenna
//! or Merging device announces, and those differ in line order, in whether they give a
//! grandmaster id or just say `traceable`, and in which optional attributes they bother
//! to include. Unknown lines are ignored rather than rejected.

use std::fmt::Write as _;
use std::net::Ipv4Addr;

use thiserror::Error;

/// Default RTP port for AES67. Streams are separated by multicast group, not by port.
pub const DEFAULT_RTP_PORT: u16 = 5004;

/// Default multicast TTL. 32 keeps a stream inside the site but off the internet.
pub const DEFAULT_TTL: u8 = 32;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SdpError {
    #[error("no m=audio line, so this describes no audio stream")]
    NoAudioMedia,
    #[error("no c= connection line, so there is no address to receive from")]
    NoConnection,
    #[error("no a=rtpmap for payload type {0}")]
    NoRtpmap(u8),
    #[error("unsupported encoding '{0}'; AES67 mandates L16 or L24")]
    UnsupportedEncoding(String),
    #[error("malformed {field}: {value}")]
    Malformed { field: &'static str, value: String },
}

/// Sample format. AES67 mandates both; squawk sends L24 and accepts either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    L16,
    L24,
}

impl Encoding {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            Encoding::L16 => 2,
            Encoding::L24 => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::L16 => "L16",
            Encoding::L24 => "L24",
        }
    }
}

/// What the sender says its media clock is locked to.
///
/// A receiver that cannot lock to the same grandmaster cannot be sample-accurate with
/// this sender, however good its jitter buffer is — so this is worth surfacing rather
/// than discarding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefClock {
    /// A named grandmaster, e.g. `00-1D-C1-FF-FE-12-34-56`, plus the PTP domain.
    Ptp { gmid: String, domain: u8 },
    /// Locked to something traceable to international time, grandmaster unnamed.
    PtpTraceable,
    /// The sender did not say. Common on cheap gear, and a reason to distrust it.
    Unspecified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    SendOnly,
    RecvOnly,
    SendRecv,
}

/// One AES67 audio stream.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamDescription {
    pub session_name: String,
    pub origin_addr: Ipv4Addr,
    pub session_id: u64,
    pub session_version: u64,
    pub address: Ipv4Addr,
    pub ttl: u8,
    pub port: u16,
    pub payload_type: u8,
    pub encoding: Encoding,
    pub sample_rate: u32,
    pub channels: u16,
    /// Packet time in milliseconds. AES67 mandates 1 ms; 0.125, 0.25, 0.333 and 4 are
    /// permitted but optional, so a receiver must not assume a whole number.
    pub ptime_ms: f32,
    pub refclk: RefClock,
    pub direction: Direction,
}

impl StreamDescription {
    /// A mono L24 stream at 48 kHz / 1 ms — what squawk sends for every key.
    pub fn mono_l24(
        session_name: impl Into<String>,
        origin_addr: Ipv4Addr,
        address: Ipv4Addr,
        payload_type: u8,
        session_id: u64,
    ) -> Self {
        Self {
            session_name: session_name.into(),
            origin_addr,
            session_id,
            session_version: session_id,
            address,
            ttl: DEFAULT_TTL,
            port: DEFAULT_RTP_PORT,
            payload_type,
            encoding: Encoding::L24,
            sample_rate: 48_000,
            channels: 1,
            ptime_ms: 1.0,
            refclk: RefClock::PtpTraceable,
            direction: Direction::SendOnly,
        }
    }

    /// Samples per packet per channel, which is also the engine's block size when the
    /// two are lined up correctly.
    pub fn ptime_samples(&self) -> usize {
        ((self.sample_rate as f32) * self.ptime_ms / 1000.0).round() as usize
    }

    /// Payload bytes per packet, all channels.
    pub fn payload_bytes(&self) -> usize {
        self.ptime_samples() * self.channels as usize * self.encoding.bytes_per_sample()
    }

    /// Render as SDP text with CRLF line endings, as RFC 4566 requires.
    pub fn to_sdp(&self) -> String {
        let mut s = String::with_capacity(400);
        let _ = writeln!(s, "v=0\r");
        let _ = writeln!(
            s,
            "o=- {} {} IN IP4 {}\r",
            self.session_id, self.session_version, self.origin_addr
        );
        let _ = writeln!(s, "s={}\r", self.session_name);
        let _ = writeln!(s, "c=IN IP4 {}/{}\r", self.address, self.ttl);
        let _ = writeln!(s, "t=0 0\r");
        let _ = writeln!(s, "m=audio {} RTP/AVP {}\r", self.port, self.payload_type);
        let _ = writeln!(
            s,
            "a=rtpmap:{} {}/{}/{}\r",
            self.payload_type,
            self.encoding.as_str(),
            self.sample_rate,
            self.channels
        );
        // Whole milliseconds render without a decimal point; sub-millisecond packet
        // times must not be rounded away, so keep three places when they are in use.
        if (self.ptime_ms.fract()).abs() < f32::EPSILON {
            let _ = writeln!(s, "a=ptime:{}\r", self.ptime_ms as u32);
        } else {
            let _ = writeln!(s, "a=ptime:{:.3}\r", self.ptime_ms);
        }
        match &self.refclk {
            RefClock::Ptp { gmid, domain } => {
                let _ = writeln!(s, "a=ts-refclk:ptp=IEEE1588-2008:{gmid}:{domain}\r");
            }
            RefClock::PtpTraceable => {
                let _ = writeln!(s, "a=ts-refclk:ptp=IEEE1588-2008:traceable\r");
            }
            RefClock::Unspecified => {}
        }
        // direct=0 means the RTP timestamp is the media clock itself, with no offset.
        let _ = writeln!(s, "a=mediaclk:direct=0\r");
        let _ = writeln!(
            s,
            "a={}\r",
            match self.direction {
                Direction::SendOnly => "sendonly",
                Direction::RecvOnly => "recvonly",
                Direction::SendRecv => "sendrecv",
            }
        );
        s
    }

    /// Parse an SDP document. Tolerates CRLF or LF, reordered lines and unknown
    /// attributes; rejects only what makes the stream unusable.
    pub fn parse(text: &str) -> Result<Self, SdpError> {
        let mut session_name = String::new();
        let mut origin_addr = Ipv4Addr::UNSPECIFIED;
        let mut session_id = 0u64;
        let mut session_version = 0u64;
        let mut address = None;
        let mut ttl = DEFAULT_TTL;
        let mut port = None;
        let mut payload_type = None;
        let mut rtpmap: Option<(u8, String, u32, u16)> = None;
        let mut ptime_ms = 1.0f32;
        let mut refclk = RefClock::Unspecified;
        let mut direction = Direction::SendOnly;

        for raw in text.lines() {
            let line = raw.trim_end_matches('\r');
            let Some((key, value)) = line.split_once('=') else { continue };
            match key {
                "o" => {
                    // o=<user> <id> <version> <nettype> <addrtype> <address>
                    let f: Vec<&str> = value.split_whitespace().collect();
                    if f.len() >= 6 {
                        session_id = f[1].parse().unwrap_or(0);
                        session_version = f[2].parse().unwrap_or(0);
                        origin_addr = f[5].parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
                    }
                }
                "s" => session_name = value.to_owned(),
                "c" => {
                    // c=IN IP4 <address>[/<ttl>[/<count>]]
                    let f: Vec<&str> = value.split_whitespace().collect();
                    if f.len() >= 3 {
                        let mut parts = f[2].split('/');
                        let addr = parts.next().unwrap_or_default();
                        address = Some(addr.parse().map_err(|_| SdpError::Malformed {
                            field: "connection address",
                            value: addr.to_owned(),
                        })?);
                        if let Some(t) = parts.next() {
                            ttl = t.parse().unwrap_or(DEFAULT_TTL);
                        }
                    }
                }
                "m" => {
                    // m=audio <port> RTP/AVP <fmt>
                    let f: Vec<&str> = value.split_whitespace().collect();
                    if f.first() == Some(&"audio") && f.len() >= 4 {
                        port = Some(f[1].parse().map_err(|_| SdpError::Malformed {
                            field: "media port",
                            value: f[1].to_owned(),
                        })?);
                        payload_type = Some(f[3].parse().map_err(|_| SdpError::Malformed {
                            field: "payload type",
                            value: f[3].to_owned(),
                        })?);
                    }
                }
                "a" => parse_attribute(value, &mut rtpmap, &mut ptime_ms, &mut refclk, &mut direction),
                _ => {}
            }
        }

        let port = port.ok_or(SdpError::NoAudioMedia)?;
        let payload_type = payload_type.ok_or(SdpError::NoAudioMedia)?;
        let address = address.ok_or(SdpError::NoConnection)?;

        let (_, enc, sample_rate, channels) =
            rtpmap.ok_or(SdpError::NoRtpmap(payload_type))?;
        let encoding = match enc.to_ascii_uppercase().as_str() {
            "L24" => Encoding::L24,
            "L16" => Encoding::L16,
            other => return Err(SdpError::UnsupportedEncoding(other.to_owned())),
        };

        Ok(Self {
            session_name,
            origin_addr,
            session_id,
            session_version,
            address,
            ttl,
            port,
            payload_type,
            encoding,
            sample_rate,
            channels,
            ptime_ms,
            refclk,
            direction,
        })
    }
}

fn parse_attribute(
    value: &str,
    rtpmap: &mut Option<(u8, String, u32, u16)>,
    ptime_ms: &mut f32,
    refclk: &mut RefClock,
    direction: &mut Direction,
) {
    if let Some(rest) = value.strip_prefix("rtpmap:") {
        // rtpmap:<pt> <encoding>/<rate>[/<channels>]
        let Some((pt, spec)) = rest.split_once(char::is_whitespace) else { return };
        let Ok(pt) = pt.trim().parse::<u8>() else { return };
        let mut parts = spec.trim().split('/');
        let enc = parts.next().unwrap_or_default().to_owned();
        let rate = parts.next().and_then(|r| r.parse().ok()).unwrap_or(48_000);
        // Channel count is optional in rtpmap and defaults to 1 — omitting it is not
        // an error, and treating it as one rejects perfectly valid mono senders.
        let ch = parts.next().and_then(|c| c.parse().ok()).unwrap_or(1);
        *rtpmap = Some((pt, enc, rate, ch));
    } else if let Some(rest) = value.strip_prefix("ptime:") {
        if let Ok(v) = rest.trim().parse::<f32>() {
            *ptime_ms = v;
        }
    } else if let Some(rest) = value.strip_prefix("ts-refclk:") {
        *refclk = parse_refclk(rest.trim());
    } else if value == "sendonly" {
        *direction = Direction::SendOnly;
    } else if value == "recvonly" {
        *direction = Direction::RecvOnly;
    } else if value == "sendrecv" {
        *direction = Direction::SendRecv;
    }
}

fn parse_refclk(spec: &str) -> RefClock {
    let Some(rest) = spec.strip_prefix("ptp=") else {
        return RefClock::Unspecified;
    };
    // ptp=<version>:<gmid>[:<domain>] or ptp=<version>:traceable
    let mut parts = rest.split(':');
    let _version = parts.next();
    match parts.next() {
        None => RefClock::Unspecified,
        Some("traceable") => RefClock::PtpTraceable,
        Some(gmid) => {
            let domain = parts.next().and_then(|d| d.parse().ok()).unwrap_or(0);
            RefClock::Ptp { gmid: gmid.to_owned(), domain }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StreamDescription {
        StreamDescription::mono_l24(
            "squawk Stage Left K0",
            Ipv4Addr::new(192, 168, 1, 10),
            Ipv4Addr::new(239, 69, 0, 1),
            96,
            1311738121,
        )
    }

    #[test]
    fn a_generated_description_round_trips() {
        let d = sample();
        let parsed = StreamDescription::parse(&d.to_sdp()).unwrap();
        assert_eq!(parsed, d);
    }

    #[test]
    fn generated_sdp_carries_the_lines_aes67_requires() {
        let text = sample().to_sdp();
        for want in [
            "v=0",
            "c=IN IP4 239.69.0.1/32",
            "m=audio 5004 RTP/AVP 96",
            "a=rtpmap:96 L24/48000/1",
            "a=ptime:1",
            "a=ts-refclk:ptp=IEEE1588-2008:traceable",
            "a=mediaclk:direct=0",
            "a=sendonly",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
        assert!(text.contains("\r\n"), "RFC 4566 wants CRLF line endings");
    }

    #[test]
    fn a_whole_millisecond_ptime_has_no_decimal_point() {
        // Receivers have been known to reject "a=ptime:1.000".
        assert!(sample().to_sdp().contains("a=ptime:1\r\n"));
    }

    #[test]
    fn a_sub_millisecond_ptime_survives_the_round_trip() {
        let mut d = sample();
        d.ptime_ms = 0.125;
        assert!(d.to_sdp().contains("a=ptime:0.125"));
        let parsed = StreamDescription::parse(&d.to_sdp()).unwrap();
        assert_eq!(parsed.ptime_ms, 0.125);
        assert_eq!(parsed.ptime_samples(), 6);
    }

    #[test]
    fn packet_geometry_matches_the_aes67_default() {
        let d = sample();
        assert_eq!(d.ptime_samples(), 48, "1 ms at 48 kHz");
        assert_eq!(d.payload_bytes(), 144, "48 mono samples of L24");
    }

    #[test]
    fn parses_a_third_party_announcement_with_a_named_grandmaster() {
        // Line order differs between vendors, and a stereo L24 sender is common.
        let text = "v=0\r\n\
                    o=- 3745 3745 IN IP4 10.0.0.42\r\n\
                    s=Console Out 1-2\r\n\
                    i=an informational line we should ignore\r\n\
                    c=IN IP4 239.254.10.5/15\r\n\
                    t=0 0\r\n\
                    a=recvonly\r\n\
                    m=audio 5004 RTP/AVP 97\r\n\
                    a=rtpmap:97 L24/48000/2\r\n\
                    a=mediaclk:direct=0\r\n\
                    a=ts-refclk:ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0\r\n\
                    a=ptime:1\r\n";
        let d = StreamDescription::parse(text).unwrap();

        assert_eq!(d.session_name, "Console Out 1-2");
        assert_eq!(d.address, Ipv4Addr::new(239, 254, 10, 5));
        assert_eq!(d.ttl, 15);
        assert_eq!(d.payload_type, 97);
        assert_eq!(d.encoding, Encoding::L24);
        assert_eq!(d.channels, 2);
        assert_eq!(d.direction, Direction::RecvOnly);
        assert_eq!(
            d.refclk,
            RefClock::Ptp { gmid: "00-1D-C1-FF-FE-12-34-56".into(), domain: 0 }
        );
        assert_eq!(d.payload_bytes(), 288, "48 frames x 2 channels x 3 bytes");
    }

    #[test]
    fn an_rtpmap_without_a_channel_count_means_mono() {
        // Optional in RFC 4566. Rejecting it would refuse valid mono senders.
        let text = "v=0\nc=IN IP4 239.1.2.3\nm=audio 5004 RTP/AVP 96\na=rtpmap:96 L16/48000\n";
        let d = StreamDescription::parse(text).unwrap();
        assert_eq!(d.channels, 1);
        assert_eq!(d.encoding, Encoding::L16);
        assert_eq!(d.payload_bytes(), 96, "48 mono samples of L16");
    }

    #[test]
    fn bare_lf_line_endings_parse() {
        let text = "v=0\nc=IN IP4 239.1.2.3\nm=audio 5004 RTP/AVP 96\na=rtpmap:96 L24/48000/1\n";
        assert!(StreamDescription::parse(text).is_ok());
    }

    #[test]
    fn a_missing_connection_or_media_line_is_rejected() {
        let no_media = "v=0\r\nc=IN IP4 239.1.2.3\r\n";
        assert_eq!(StreamDescription::parse(no_media), Err(SdpError::NoAudioMedia));

        let no_conn = "v=0\r\nm=audio 5004 RTP/AVP 96\r\na=rtpmap:96 L24/48000/1\r\n";
        assert_eq!(StreamDescription::parse(no_conn), Err(SdpError::NoConnection));
    }

    #[test]
    fn a_compressed_payload_is_rejected_rather_than_mangled() {
        let text = "v=0\nc=IN IP4 239.1.2.3\nm=audio 5004 RTP/AVP 96\na=rtpmap:96 opus/48000/2\n";
        assert_eq!(
            StreamDescription::parse(text),
            Err(SdpError::UnsupportedEncoding("OPUS".into()))
        );
    }

    #[test]
    fn a_sender_that_names_no_clock_is_reported_as_unspecified() {
        // Worth surfacing: without a shared grandmaster nothing can be sample-accurate.
        let text = "v=0\nc=IN IP4 239.1.2.3\nm=audio 5004 RTP/AVP 96\na=rtpmap:96 L24/48000/1\n";
        assert_eq!(StreamDescription::parse(text).unwrap().refclk, RefClock::Unspecified);
    }
}
