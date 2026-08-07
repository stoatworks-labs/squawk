//! UDP sockets: sending the engine's streams and receiving endpoints' microphones.
//!
//! # Choosing an interface is not optional
//!
//! Every constructor here takes an explicit interface address. On a machine with more
//! than one NIC — which describes every AV server ever built — letting the OS pick
//! means multicast leaves whichever interface the routing table prefers, which is
//! usually the office LAN rather than the audio network. The stream then exists, the
//! sender reports no error, and nothing hears it. Making the interface a required
//! argument turns that into a decision rather than an accident.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};

use crate::jitter::{JitterBuffer, Pull, Push};
use crate::packet::{self, RtpHeader, L24_BYTES, RTP_HEADER_LEN};

/// Receive buffer to request per stream.
///
/// The default on most systems is around 64 kB, which at 1000 packets/sec is well under
/// a second of slack. A scheduling hiccup then overflows the socket queue and the loss
/// appears in the audio, having never touched the jitter buffer that exists to prevent
/// exactly that.
const RECV_BUFFER_BYTES: usize = 1 << 20;

/// Default first group for squawk's own streams, in the AES67-conventional 239/8 space.
pub const DEFAULT_GROUP_BASE: Ipv4Addr = Ipv4Addr::new(239, 69, 0, 0);

/// Allocate the multicast group for a stream index.
///
/// # The 23-bit trap
///
/// IPv4 multicast maps to Ethernet by copying only the **low 23 bits** of the address
/// into the MAC. So 239.69.1.1 and 239.197.1.1 — and 30 other addresses — all become
/// the same MAC. A switch doing IGMP snooping filters on MAC, so a receiver that joined
/// one of them is delivered all of them, and on a busy audio network that is enough
/// unwanted traffic to swamp a small endpoint's NIC.
///
/// Allocating sequentially from a single base keeps every stream's low 23 bits distinct,
/// which sidesteps the whole problem. Anything that changes this scheme has to preserve
/// that property.
pub fn stream_group(base: Ipv4Addr, index: usize) -> Ipv4Addr {
    let n = u32::from(base).wrapping_add(index as u32);
    Ipv4Addr::from(n)
}

/// Sends one stream: RTP header bookkeeping plus the socket.
pub struct StreamSender {
    socket: UdpSocket,
    dest: SocketAddrV4,
    payload_type: u8,
    ssrc: u32,
    sequence: u16,
    timestamp: u32,
    block: usize,
    scratch: Vec<u8>,
}

impl StreamSender {
    /// Bind a sender for `block` samples per packet.
    ///
    /// `iface` is the local address of the NIC the audio network is on. `dest` may be
    /// a multicast group or a unicast address; multicast options are only applied when
    /// it is actually multicast.
    pub fn new(
        iface: Ipv4Addr,
        dest: Ipv4Addr,
        port: u16,
        block: usize,
        payload_type: u8,
        ssrc: u32,
        ttl: u32,
    ) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.bind(&SocketAddrV4::new(iface, 0).into())?;
        if dest.is_multicast() {
            // The whole reason socket2 is here: without this the stream leaves by
            // whichever NIC the routing table prefers, reports success, and is heard
            // by nothing.
            socket.set_multicast_if_v4(&iface)?;
            socket.set_multicast_ttl_v4(ttl)?;
        }
        let socket: UdpSocket = socket.into();
        Ok(Self {
            socket,
            dest: SocketAddrV4::new(dest, port),
            payload_type,
            ssrc,
            // A random-ish start is what RFC 3550 asks for; deriving it from the SSRC
            // keeps it deterministic for tests while still differing between streams.
            sequence: ssrc as u16,
            timestamp: ssrc,
            block,
            scratch: vec![0u8; RTP_HEADER_LEN + block * L24_BYTES],
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The timestamp the *next* packet will carry.
    pub fn next_timestamp(&self) -> u32 {
        self.timestamp
    }

    /// Send one block. The RTP timestamp advances by the block size, not by byte count.
    pub fn send(&mut self, samples: &[f32]) -> io::Result<usize> {
        debug_assert_eq!(samples.len(), self.block);
        let header = RtpHeader::new(self.payload_type, self.sequence, self.timestamp, self.ssrc);
        let n = packet::write_packet(&header, samples, &mut self.scratch);
        let sent = self.socket.send_to(&self.scratch[..n], self.dest)?;

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.block as u32);
        Ok(sent)
    }
}

/// Receives one stream into a jitter buffer.
pub struct StreamReceiver {
    socket: UdpSocket,
    jitter: JitterBuffer,
    buf: Vec<u8>,
    scratch: Vec<f32>,
    /// Locked to the first SSRC seen, so a stray sender on the same group cannot
    /// interleave its audio into this endpoint's timeline.
    ssrc: Option<u32>,
    foreign_packets: u64,
}

impl StreamReceiver {
    /// Bind and, if `group` is multicast, join it on `iface`.
    ///
    /// # Why the bind address depends on the platform
    ///
    /// squawk puts every stream on port 5004 and tells them apart by multicast group,
    /// so one host binds that port many times. On a socket bound to the wildcard
    /// address, the kernel delivers datagrams for **every** group joined on that port,
    /// not just the ones this socket joined — so each of 320 receivers would wake for
    /// all 320 streams and discard 319 of them. That is quadratic work, and it arrives
    /// as mysterious CPU load rather than as an error.
    ///
    /// Binding to the group address instead makes the kernel filter by destination.
    /// That works on Unix and is rejected on Windows, which has to bind the wildcard
    /// and filter in userspace — the SSRC lock below does that, correctly but at the
    /// cost this bind exists to avoid.
    pub fn new(
        iface: Ipv4Addr,
        group: Ipv4Addr,
        port: u16,
        block: usize,
        target_depth: usize,
    ) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        // Every stream shares port 5004 and is told apart by multicast group, so a
        // client subscribing to several keys binds this port several times. Without
        // address reuse the second bind fails and the design does not work at all.
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        let bind_to = if group.is_multicast() && cfg!(unix) {
            SocketAddrV4::new(group, port)
        } else {
            SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)
        };
        socket.bind(&bind_to.into())?;
        // Best-effort: a system that refuses the size still works, just with less slack.
        let _ = socket.set_recv_buffer_size(RECV_BUFFER_BYTES);
        if group.is_multicast() {
            socket.join_multicast_v4(&group, &iface)?;
        }
        socket.set_nonblocking(true)?;
        let socket: UdpSocket = socket.into();
        Ok(Self {
            socket,
            jitter: JitterBuffer::new(block, target_depth),
            // Big enough for 4 ms of 8-channel L24 plus headers, so an oversized packet
            // from a misconfigured sender is truncated rather than panicking.
            buf: vec![0u8; 2048],
            scratch: vec![0.0; block],
            ssrc: None,
            foreign_packets: 0,
        })
    }

    pub fn jitter(&self) -> &JitterBuffer {
        &self.jitter
    }

    pub fn foreign_packets(&self) -> u64 {
        self.foreign_packets
    }

    /// Drain everything waiting on the socket into the jitter buffer.
    ///
    /// Returns how many packets were accepted. Call this once per tick, before pulling:
    /// leaving packets in the socket queue is what turns a brief scheduling hiccup into
    /// a receive-buffer overflow and a burst of loss.
    pub fn poll(&mut self) -> io::Result<usize> {
        let mut accepted = 0;
        loop {
            let len = match self.socket.recv(&mut self.buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            };

            let Ok((header, offset)) = RtpHeader::parse(&self.buf[..len]) else {
                continue;
            };
            match self.ssrc {
                None => self.ssrc = Some(header.ssrc),
                Some(known) if known != header.ssrc => {
                    self.foreign_packets += 1;
                    continue;
                }
                _ => {}
            }

            let Ok(payload) = packet::payload_range(&self.buf[..len], offset) else {
                continue;
            };
            if payload.len() != self.scratch.len() * L24_BYTES {
                // Wrong packet time or channel count for what we were told to expect.
                continue;
            }
            if packet::decode_l24(payload, &mut self.scratch).is_err() {
                continue;
            }
            if self.jitter.push(header.timestamp, &self.scratch) == Push::Accepted {
                accepted += 1;
            }
        }
        Ok(accepted)
    }

    /// Take the next aligned block onto the server's timeline.
    pub fn pull(&mut self, out: &mut [f32]) -> Pull {
        self.jitter.pull(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const BLOCK: usize = 48;
    const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

    /// Pick a free UDP port by binding to 0 and reading back what we got.
    fn free_port() -> u16 {
        UdpSocket::bind((LOOPBACK, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn group_allocation_keeps_the_low_23_bits_distinct() {
        // The property that stops two streams sharing an Ethernet multicast MAC.
        let groups: Vec<Ipv4Addr> = (0..1000).map(|i| stream_group(DEFAULT_GROUP_BASE, i)).collect();
        let mut low23: Vec<u32> = groups.iter().map(|g| u32::from(*g) & 0x007f_ffff).collect();
        low23.sort_unstable();
        let before = low23.len();
        low23.dedup();
        assert_eq!(low23.len(), before, "two streams would share a multicast MAC");
        assert!(groups.iter().all(|g| g.is_multicast()));
    }

    #[test]
    fn audio_survives_a_real_round_trip_over_udp() {
        // Unicast loopback rather than multicast: this exercises the whole path —
        // sockets, RTP framing, L24, jitter buffer — without depending on multicast
        // routing being available wherever the tests happen to run.
        let port = free_port();
        let mut rx = StreamReceiver::new(LOOPBACK, LOOPBACK, port, BLOCK, 2).unwrap();
        let mut tx = StreamSender::new(LOOPBACK, LOOPBACK, port, BLOCK, 96, 0x5157_4B01, 32).unwrap();

        // A ramp per block, so a misplaced block is obvious rather than plausible.
        let blocks: Vec<Vec<f32>> = (0..16)
            .map(|b| (0..BLOCK).map(|i| ((b * BLOCK + i) as f32 / 2048.0) - 0.25).collect())
            .collect();
        for b in &blocks {
            tx.send(b).unwrap();
        }

        // Give the loopback stack a moment, then drain.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let accepted = rx.poll().unwrap();
        assert_eq!(accepted, 16, "all 16 packets should have arrived on loopback");

        let mut out = vec![0.0f32; BLOCK];
        for _ in 0..2 {
            assert_eq!(rx.pull(&mut out), Pull::Priming);
        }
        for (n, expected) in blocks.iter().enumerate() {
            assert_eq!(rx.pull(&mut out), Pull::Filled, "block {n}");
            for (a, b) in out.iter().zip(expected) {
                assert!((a - b).abs() < 1e-6, "block {n}: {a} != {b}");
            }
        }
        assert_eq!(rx.jitter().stats().lost, 0);
    }

    #[test]
    fn multicast_delivers_only_the_group_a_receiver_joined() {
        // The load-bearing property of the whole addressing scheme: many receivers
        // share one port and each gets only its own stream. Run over the loopback
        // interface, which is MULTICAST-capable, so this needs no audio network.
        //
        // Note this test would pass even with the interface selection broken on a
        // single-homed host. It is not a substitute for trying it on real hardware.
        let port = free_port();
        let group_a = Ipv4Addr::new(239, 69, 200, 1);
        let group_b = Ipv4Addr::new(239, 69, 200, 2);

        let mut rx_a = StreamReceiver::new(LOOPBACK, group_a, port, BLOCK, 1).unwrap();
        let mut rx_b = StreamReceiver::new(LOOPBACK, group_b, port, BLOCK, 1).unwrap();

        let mut tx_a = StreamSender::new(LOOPBACK, group_a, port, BLOCK, 96, 0xAAAA_0001, 1).unwrap();
        tx_a.send(&[0.5; BLOCK]).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(80));
        let got_a = rx_a.poll().unwrap();
        let got_b = rx_b.poll().unwrap();

        assert_eq!(got_a, 1, "the joined group's receiver got nothing");
        assert_eq!(got_b, 0, "a receiver was delivered another group's audio");

        let mut out = vec![0.0f32; BLOCK];
        rx_a.pull(&mut out); // priming
        assert_eq!(rx_a.pull(&mut out), Pull::Filled);
        assert!((out[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_stray_sender_on_the_same_port_cannot_inject_audio() {
        // Two senders, two SSRCs. The receiver locks to the first and must ignore the
        // second rather than interleaving both into one endpoint's timeline.
        let port = free_port();
        let mut rx = StreamReceiver::new(LOOPBACK, LOOPBACK, port, BLOCK, 1).unwrap();
        let mut wanted = StreamSender::new(LOOPBACK, LOOPBACK, port, BLOCK, 96, 0x1111_1111, 32).unwrap();
        let mut stray = StreamSender::new(LOOPBACK, LOOPBACK, port, BLOCK, 96, 0x2222_2222, 32).unwrap();

        wanted.send(&[0.25; BLOCK]).unwrap();
        stray.send(&[-0.75; BLOCK]).unwrap();
        wanted.send(&[0.25; BLOCK]).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        rx.poll().unwrap();
        assert_eq!(rx.foreign_packets(), 1, "the stray SSRC should have been counted and dropped");

        let mut out = vec![0.0f32; BLOCK];
        rx.pull(&mut out); // priming
        assert_eq!(rx.pull(&mut out), Pull::Filled);
        assert!((out[0] - 0.25).abs() < 1e-6, "stray audio leaked in: {}", out[0]);
    }

    #[test]
    fn a_packet_with_the_wrong_geometry_is_dropped_not_decoded() {
        // A sender using a different packet time on the same port. Decoding it would
        // put the wrong number of samples on the timeline.
        let port = free_port();
        let mut rx = StreamReceiver::new(LOOPBACK, LOOPBACK, port, BLOCK, 1).unwrap();
        let mut wrong = StreamSender::new(LOOPBACK, LOOPBACK, port, 96, 96, 0x3333_3333, 32).unwrap();

        wrong.send(&vec![0.5; 96]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(rx.poll().unwrap(), 0, "a 2 ms packet must not be accepted as 1 ms");
    }

    #[test]
    fn measures_what_one_socket_can_actually_push() {
        // Not a pass/fail bound — a number. squawk's design implies 1000 packets per
        // second per stream, so this says how many streams one unbatched sending
        // thread can carry before `sendmmsg`-style batching stops being optional.
        let port = free_port();
        let _rx = StreamReceiver::new(LOOPBACK, LOOPBACK, port, BLOCK, 2).unwrap();
        let mut tx = StreamSender::new(LOOPBACK, LOOPBACK, port, BLOCK, 96, 0x9999_0001, 32).unwrap();
        let block = vec![0.1f32; BLOCK];

        let count = 20_000;
        let start = Instant::now();
        for _ in 0..count {
            // Loopback receive buffers fill; a dropped send is fine, we are timing the
            // syscall path rather than proving delivery.
            let _ = tx.send(&block);
        }
        let elapsed = start.elapsed();
        let pps = count as f64 / elapsed.as_secs_f64();
        println!(
            "\nunbatched send_to: {:.0} packets/sec  ({:.2} us per packet)\n\
             at 1 ms ptime that is ~{:.0} streams from one thread\n",
            pps,
            elapsed.as_secs_f64() * 1e6 / count as f64,
            pps / 1000.0
        );
        assert!(pps > 1000.0, "a socket that slow would not carry one stream");
    }
}
