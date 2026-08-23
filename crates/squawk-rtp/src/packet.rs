//! RTP header and L24 payload coding, per RFC 3550 and AES67.

use thiserror::Error;

/// Fixed RTP header length. squawk never sends CSRCs or header extensions, but the
/// parser handles both because other vendors' senders do.
pub const RTP_HEADER_LEN: usize = 12;

/// Bytes per sample for L24. AES67's other mandatory format is L16; squawk sends L24
/// because the extra byte costs 33% of a payload that is already small next to the
/// 54 bytes of Ethernet/IP/UDP/RTP framing wrapped around it.
pub const L24_BYTES: usize = 3;

/// Full-scale for 24-bit signed audio. Note this is the *negative* limit's magnitude:
/// the positive limit is one lower, which is the trap in the conversion below.
const SCALE_24: f32 = 8_388_608.0; // 2^23

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RtpError {
    #[error("packet is {0} bytes, shorter than the {RTP_HEADER_LEN}-byte RTP header")]
    TooShort(usize),
    #[error("RTP version is {0}, expected 2")]
    BadVersion(u8),
    #[error("header claims {claimed} bytes of CSRC/extension but the packet is {len} bytes")]
    TruncatedHeader { claimed: usize, len: usize },
    #[error("payload is {0} bytes, not a whole number of L24 samples")]
    PartialSample(usize),
    #[error("payload holds {samples} samples, not divisible by {channels} channels")]
    RaggedFrame { samples: usize, channels: usize },
}

/// An RTP header.
///
/// `timestamp` is in media-clock units — for AES67 that is the sample rate, so it
/// advances by the packet's sample count per channel, not by its byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub fn new(payload_type: u8, sequence: u16, timestamp: u32, ssrc: u32) -> Self {
        Self { marker: false, payload_type, sequence, timestamp, ssrc }
    }

    /// Write the 12-byte header. squawk always sends version 2, no padding, no
    /// extension, no CSRCs.
    pub fn write(&self, out: &mut [u8; RTP_HEADER_LEN]) {
        out[0] = 0b1000_0000; // V=2, P=0, X=0, CC=0
        out[1] = (u8::from(self.marker) << 7) | (self.payload_type & 0x7f);
        out[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }

    /// Parse a header, returning it with the offset at which the payload starts.
    ///
    /// Skips CSRC list and any header extension, so a stream from a sender that uses
    /// them still decodes. Padding is *not* stripped here — see [`payload_range`].
    pub fn parse(buf: &[u8]) -> Result<(Self, usize), RtpError> {
        if buf.len() < RTP_HEADER_LEN {
            return Err(RtpError::TooShort(buf.len()));
        }
        let version = buf[0] >> 6;
        if version != 2 {
            return Err(RtpError::BadVersion(version));
        }

        let csrc_count = (buf[0] & 0x0f) as usize;
        let has_extension = buf[0] & 0b0001_0000 != 0;
        let mut offset = RTP_HEADER_LEN + csrc_count * 4;

        if has_extension {
            // Extension header is 4 bytes: 16-bit profile id, 16-bit length in 32-bit
            // words, not counting those first 4 bytes.
            if buf.len() < offset + 4 {
                return Err(RtpError::TruncatedHeader { claimed: offset + 4, len: buf.len() });
            }
            let words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
            offset += 4 + words * 4;
        }

        if buf.len() < offset {
            return Err(RtpError::TruncatedHeader { claimed: offset, len: buf.len() });
        }

        let header = Self {
            marker: buf[1] & 0x80 != 0,
            payload_type: buf[1] & 0x7f,
            sequence: u16::from_be_bytes([buf[2], buf[3]]),
            timestamp: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            ssrc: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        };
        Ok((header, offset))
    }
}

/// The payload slice of a received packet, with padding removed.
///
/// RFC 3550 puts the pad count in the *last* byte of the packet when P is set, which is
/// easy to forget and produces a few samples of periodic noise at the end of every
/// packet when you do.
pub fn payload_range(buf: &[u8], payload_start: usize) -> Result<&[u8], RtpError> {
    if buf.len() < payload_start {
        return Err(RtpError::TruncatedHeader { claimed: payload_start, len: buf.len() });
    }
    let padded = buf[0] & 0b0010_0000 != 0;
    let end = if padded {
        let pad = *buf.last().unwrap_or(&0) as usize;
        if pad == 0 || buf.len() < payload_start + pad {
            return Err(RtpError::TruncatedHeader { claimed: payload_start + pad, len: buf.len() });
        }
        buf.len() - pad
    } else {
        buf.len()
    };
    Ok(&buf[payload_start..end])
}

/// Convert one float sample to 24-bit signed, saturating.
///
/// The asymmetry matters: 24-bit signed runs from -8388608 to +8388607, so scaling by
/// 2^23 maps 1.0 to 8388608, which is one past the top. Clamping *after* the multiply
/// rather than clamping the float to 1.0 first is what keeps full-scale positive audio
/// from wrapping to full-scale negative — a fault that is inaudible on a sine and
/// unmistakable on anything that actually reaches 0 dBFS.
#[inline]
fn f32_to_i24(sample: f32) -> i32 {
    let scaled = (sample * SCALE_24) as i32;
    scaled.clamp(-8_388_608, 8_388_607)
}

#[inline]
fn i24_to_f32(raw: i32) -> f32 {
    raw as f32 / SCALE_24
}

/// Encode float samples as big-endian L24 into `out`, which must be `3 * samples.len()`.
pub fn encode_l24(samples: &[f32], out: &mut [u8]) {
    debug_assert_eq!(out.len(), samples.len() * L24_BYTES);
    for (s, chunk) in samples.iter().zip(out.as_chunks_mut::<L24_BYTES>().0.iter_mut()) {
        let v = f32_to_i24(*s);
        // Big-endian, network order — AES67 audio is always most-significant byte first.
        chunk[0] = (v >> 16) as u8;
        chunk[1] = (v >> 8) as u8;
        chunk[2] = v as u8;
    }
}

/// Decode big-endian L24 into floats. `out` must hold `payload.len() / 3` samples.
pub fn decode_l24(payload: &[u8], out: &mut [f32]) -> Result<(), RtpError> {
    if !payload.len().is_multiple_of(L24_BYTES) {
        return Err(RtpError::PartialSample(payload.len()));
    }
    debug_assert_eq!(out.len(), payload.len() / L24_BYTES);
    for (chunk, o) in payload.as_chunks::<L24_BYTES>().0.iter().zip(out.iter_mut()) {
        // Sign-extend by assembling into the top 24 bits of an i32 and shifting back.
        let raw = ((chunk[0] as i32) << 24 | (chunk[1] as i32) << 16 | (chunk[2] as i32) << 8) >> 8;
        *o = i24_to_f32(raw);
    }
    Ok(())
}

/// How many frames (samples per channel) an L24 payload holds.
pub fn frames_in_payload(payload_len: usize, channels: usize) -> Result<usize, RtpError> {
    if !payload_len.is_multiple_of(L24_BYTES) {
        return Err(RtpError::PartialSample(payload_len));
    }
    let samples = payload_len / L24_BYTES;
    if channels == 0 || !samples.is_multiple_of(channels) {
        return Err(RtpError::RaggedFrame { samples, channels });
    }
    Ok(samples / channels)
}

/// Build a complete AES67 audio packet into `out`, returning the bytes written.
///
/// `out` must have room for [`RTP_HEADER_LEN`] plus `3 * samples.len()`.
pub fn write_packet(header: &RtpHeader, samples: &[f32], out: &mut [u8]) -> usize {
    let total = RTP_HEADER_LEN + samples.len() * L24_BYTES;
    debug_assert!(out.len() >= total);
    let mut head = [0u8; RTP_HEADER_LEN];
    header.write(&mut head);
    out[..RTP_HEADER_LEN].copy_from_slice(&head);
    encode_l24(samples, &mut out[RTP_HEADER_LEN..total]);
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let h = RtpHeader { marker: true, payload_type: 97, sequence: 0xBEEF, timestamp: 0x1234_5678, ssrc: 0xDEAD_C0DE };
        let mut buf = [0u8; RTP_HEADER_LEN];
        h.write(&mut buf);

        assert_eq!(buf[0], 0x80, "V=2, no padding, no extension, no CSRC");
        assert_eq!(buf[1], 0x80 | 97, "marker bit plus payload type");

        let (parsed, offset) = RtpHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(offset, RTP_HEADER_LEN);
    }

    #[test]
    fn rejects_a_short_or_wrong_version_packet() {
        assert_eq!(RtpHeader::parse(&[0u8; 8]), Err(RtpError::TooShort(8)));
        let mut buf = [0u8; RTP_HEADER_LEN];
        buf[0] = 0b0100_0000; // version 1
        assert_eq!(RtpHeader::parse(&buf), Err(RtpError::BadVersion(1)));
    }

    #[test]
    fn skips_csrcs_and_header_extensions() {
        // V=2, X=1, CC=2 → 12 + 8 CSRC bytes + 4 ext header + 8 ext body.
        let mut buf = vec![0u8; RTP_HEADER_LEN + 8 + 4 + 8 + 6];
        buf[0] = 0b1001_0010;
        let ext_at = RTP_HEADER_LEN + 8;
        buf[ext_at + 2..ext_at + 4].copy_from_slice(&2u16.to_be_bytes()); // 2 words
        let (_, offset) = RtpHeader::parse(&buf).unwrap();
        assert_eq!(offset, RTP_HEADER_LEN + 8 + 4 + 8);
    }

    #[test]
    fn strips_rfc3550_padding_from_the_end() {
        // The pad count lives in the last byte, not the header.
        let mut buf = vec![0u8; RTP_HEADER_LEN + 6 + 4];
        buf[0] = 0b1010_0000; // V=2, P=1
        *buf.last_mut().unwrap() = 4;
        let payload = payload_range(&buf, RTP_HEADER_LEN).unwrap();
        assert_eq!(payload.len(), 6, "padding should not be decoded as audio");
    }

    #[test]
    fn l24_round_trips_within_a_quantisation_step() {
        let samples: Vec<f32> = (0..64).map(|i| (i as f32 / 32.0) - 1.0).collect();
        let mut bytes = vec![0u8; samples.len() * L24_BYTES];
        encode_l24(&samples, &mut bytes);

        let mut back = vec![0.0f32; samples.len()];
        decode_l24(&bytes, &mut back).unwrap();

        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() < 1.0 / SCALE_24, "{a} != {b}");
        }
    }

    #[test]
    fn full_scale_saturates_instead_of_wrapping() {
        // The trap: 1.0 * 2^23 is 8388608, one past the 24-bit positive limit. Wrapping
        // would turn peak positive audio into peak negative.
        let samples = [1.0, 1.5, -1.0, -2.0, 0.999_999_9];
        let mut bytes = vec![0u8; samples.len() * L24_BYTES];
        encode_l24(&samples, &mut bytes);

        let mut back = vec![0.0f32; samples.len()];
        decode_l24(&bytes, &mut back).unwrap();

        assert!(back[0] > 0.99 && back[0] <= 1.0, "1.0 became {}", back[0]);
        assert!(back[1] > 0.99 && back[1] <= 1.0, "1.5 became {}", back[1]);
        assert!((back[2] + 1.0).abs() < 1e-6, "-1.0 became {}", back[2]);
        assert!((back[3] + 1.0).abs() < 1e-6, "-2.0 became {}", back[3]);
        assert!(back.iter().all(|s| *s >= -1.0 && s.abs() <= 1.0));
    }

    #[test]
    fn silence_encodes_to_all_zero_bytes() {
        let mut bytes = vec![0xffu8; 3 * L24_BYTES];
        encode_l24(&[0.0, 0.0, 0.0], &mut bytes);
        assert_eq!(bytes, vec![0u8; 3 * L24_BYTES]);
    }

    #[test]
    fn decode_rejects_a_partial_sample() {
        let mut out = [0.0f32; 1];
        assert_eq!(decode_l24(&[0, 1], &mut out), Err(RtpError::PartialSample(2)));
    }

    #[test]
    fn a_written_packet_parses_back_to_the_same_audio() {
        let header = RtpHeader::new(96, 42, 48_000, 0x1111_2222);
        let samples: Vec<f32> = (0..48).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();

        let mut buf = vec![0u8; RTP_HEADER_LEN + 48 * L24_BYTES];
        let n = write_packet(&header, &samples, &mut buf);
        assert_eq!(n, RTP_HEADER_LEN + 144, "1 ms of mono L24 is a 144-byte payload");

        let (parsed, offset) = RtpHeader::parse(&buf[..n]).unwrap();
        assert_eq!(parsed, header);
        let payload = payload_range(&buf[..n], offset).unwrap();
        assert_eq!(frames_in_payload(payload.len(), 1).unwrap(), 48);

        let mut back = vec![0.0f32; 48];
        decode_l24(payload, &mut back).unwrap();
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() < 1.0 / SCALE_24);
        }
    }

    #[test]
    fn frame_count_rejects_a_ragged_multichannel_payload() {
        // 48 samples across 5 channels is not a whole number of frames.
        assert_eq!(
            frames_in_payload(48 * L24_BYTES, 5),
            Err(RtpError::RaggedFrame { samples: 48, channels: 5 })
        );
        assert_eq!(frames_in_payload(96 * L24_BYTES, 2).unwrap(), 48);
    }
}
