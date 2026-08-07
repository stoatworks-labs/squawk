//! Sockets for a PTP slave port.
//!
//! # Where the timestamps come from
//!
//! Immediately after `recv` returns, in userspace. That includes NIC interrupt latency,
//! kernel scheduling and whatever else the machine was busy with — tens of microseconds
//! of noise where hardware timestamping would give tens of nanoseconds.
//!
//! At 48 kHz one sample is 20.8 us, so this lands around **±1 sample**. Adequate for
//! speech on an intercom; not adequate for phase-coherent summing. macOS exposes no
//! hardware timestamping at all, so on that platform there is no better option; on
//! Linux, `SO_TIMESTAMPING` with a capable NIC is the upgrade path and is not
//! implemented here.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use socket2::{Domain, Protocol, Socket, Type};

use crate::message::{write_delay_req, Message, PortIdentity, Timestamp};
use crate::servo::LockState;
use crate::slave::{Event, SlaveState, SlaveStats};
use crate::{PORT_EVENT, PORT_GENERAL, PTP_PRIMARY};

/// PTP is not meant to be routed: the default profile uses a TTL of 1 so a stray
/// grandmaster on another subnet cannot silently become yours.
const PTP_TTL: u32 = 1;

/// What to show an operator.
#[derive(Debug, Clone)]
pub struct PtpStatus {
    pub domain: u8,
    pub grandmaster: Option<String>,
    pub masters_seen: usize,
    pub state: LockState,
    pub offset_nanos: i64,
    pub delay_nanos: i64,
    pub steps: u64,
    pub stats: SlaveStats,
}

pub struct PtpPort {
    event: UdpSocket,
    general: UdpSocket,
    state: SlaveState,
    buf: Vec<u8>,

    /// Monotonic base, and the PTP-format time at that instant.
    epoch_instant: Instant,
    epoch_nanos: u128,

    delay_seq: u16,
    last_delay_req: Instant,
    delay_interval: Duration,
}

/// Bind one of the PTP ports and join the PTP group.
///
/// # Why this binds the wildcard when the RTP receivers bind the group
///
/// Two reasons, and they point the same way.
///
/// The RTP side binds the group address because hundreds of streams share port 5004 and
/// the kernel's destination filtering is what stops every receiver waking for every
/// stream. PTP has exactly one group on each of two ports, so there is nothing to
/// demultiplex and nothing to gain.
///
/// More importantly, it would not work. On macOS the privileged-port check applies when
/// binding a **specific** address but not the wildcard: `bind(0.0.0.0:319)` succeeds
/// unprivileged while `bind(224.0.1.129:319)` returns `EACCES`. That is the opposite of
/// the intuition, and it presents as a bare "Permission denied" that looks exactly like
/// needing root — which squawk does not, on either platform.
fn bind_multicast(iface: Ipv4Addr, group: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())?;
    socket.join_multicast_v4(&group, &iface)?;
    socket.set_multicast_if_v4(&iface)?;
    socket.set_multicast_ttl_v4(PTP_TTL)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

impl PtpPort {
    /// Join the PTP domain on `iface`.
    pub fn new(iface: Ipv4Addr, identity: PortIdentity, domain: u8) -> io::Result<Self> {
        let event = bind_multicast(iface, PTP_PRIMARY, PORT_EVENT)?;
        let general = bind_multicast(iface, PTP_PRIMARY, PORT_GENERAL)?;

        // The absolute base does not matter: the servo measures our offset from the
        // master, so any consistent local timeline works. Starting from the system
        // clock just makes the numbers readable in logs.
        let epoch_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        tracing::info!(%iface, domain, %identity.clock, "PTP slave listening");

        Ok(Self {
            event,
            general,
            state: SlaveState::new(identity, domain),
            buf: vec![0u8; 1500],
            epoch_instant: Instant::now(),
            epoch_nanos,
            delay_seq: 0,
            last_delay_req: Instant::now(),
            // The AES67 media profile's default. Faster buys little: path delay changes
            // far more slowly than offset, and every request costs the master a
            // multicast response that every slave on the domain has to filter.
            delay_interval: Duration::from_secs(1),
        })
    }

    /// Local time, undisciplined.
    fn local_now(&self) -> Timestamp {
        let total = self.epoch_nanos + self.epoch_instant.elapsed().as_nanos();
        Timestamp {
            seconds: (total / 1_000_000_000) as u64,
            nanos: (total % 1_000_000_000) as u32,
        }
    }

    /// Local time corrected by the servo — PTP time as we currently believe it.
    ///
    /// This is what RTP timestamps are derived from, and it is also what every
    /// measurement is taken with. That second part is what closes the servo's loop: a
    /// measurement taken on the *undisciplined* clock reports the same raw offset
    /// forever no matter what the servo does, so the servo steps repeatedly and never
    /// converges. Timestamping with corrected time makes each measurement the residual
    /// error, which is what a PI controller is for.
    pub fn ptp_now(&self) -> Timestamp {
        let local = self.local_now();
        let corrected = local.seconds as i128 * 1_000_000_000
            + local.nanos as i128
            + self.state.servo().local_to_ptp_offset() as i128;
        let corrected = corrected.max(0) as u128;
        Timestamp {
            seconds: (corrected / 1_000_000_000) as u64,
            nanos: (corrected % 1_000_000_000) as u32,
        }
    }

    /// How often to send Delay_Reqs. The default of one second is the AES67 media
    /// profile's; path delay changes far more slowly than offset, and every request
    /// costs the master a multicast reply that every slave on the domain must filter.
    pub fn set_delay_interval(&mut self, interval: Duration) {
        self.delay_interval = interval;
    }

    pub fn lock_state(&self) -> LockState {
        self.state.servo().state()
    }

    pub fn status(&self) -> PtpStatus {
        let m = self.state.last_measurement();
        PtpStatus {
            domain: 0,
            grandmaster: self.state.master().map(|m| m.grandmaster.to_string()),
            masters_seen: self.state.masters_seen(),
            state: self.state.servo().state(),
            offset_nanos: m.map(|m| m.offset_nanos).unwrap_or(0),
            delay_nanos: m.map(|m| m.delay_nanos).unwrap_or(0),
            steps: self.state.servo().steps(),
            stats: self.state.stats(),
        }
    }

    /// Drain both sockets and service the delay-request timer. Call regularly.
    pub fn poll(&mut self) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        let now = Instant::now();

        // Event port first: those are the messages whose timing matters, and draining
        // the general port ahead of them would add its processing time to their
        // timestamps.
        for from_event in [true, false] {
            loop {
                let socket = if from_event { &self.event } else { &self.general };
                let len = match socket.recv(&mut self.buf) {
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                };
                // Timestamp before parsing. Every microsecond between arrival and this
                // call is an error the servo cannot distinguish from path delay.
                //
                // Corrected time, not raw local time — see `ptp_now`. Measuring on the
                // undisciplined clock leaves the servo with an input that never
                // responds to its output.
                let rx = self.ptp_now();

                match Message::parse(&self.buf[..len]) {
                    Ok(msg) => {
                        if let Some(ev) = self.state.on_message(&msg, rx, now) {
                            events.push(ev);
                        }
                    }
                    Err(err) => tracing::trace!(%err, "unparseable PTP message"),
                }
            }
        }

        if let Some(ev) = self.state.tick(now) {
            events.push(ev);
        }
        self.maybe_send_delay_req(now)?;
        Ok(events)
    }

    fn maybe_send_delay_req(&mut self, now: Instant) -> io::Result<()> {
        if !self.state.ready_for_delay_req() {
            return Ok(());
        }
        if now.duration_since(self.last_delay_req) < self.delay_interval {
            return Ok(());
        }

        let mut packet = [0u8; 44];
        self.delay_seq = self.delay_seq.wrapping_add(1);
        write_delay_req(self.state.identity(), self.status().domain, self.delay_seq, &mut packet);

        let dest = SocketAddrV4::new(PTP_PRIMARY, PORT_EVENT);
        self.event.send_to(&packet, dest)?;
        // t3 is taken immediately after the syscall returns, which is the closest
        // approximation available to when the packet actually left. On the same
        // corrected timeline as the receive timestamps, or the two do not combine.
        let t3 = self.ptp_now();

        self.state.on_delay_req_sent(self.delay_seq, t3);
        self.last_delay_req = now;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ClockIdentity;

    #[test]
    fn a_port_can_join_the_ptp_groups_on_loopback() {
        // Proves the socket options are acceptable to the platform — not that any
        // grandmaster exists.
        let identity = PortIdentity {
            clock: ClockIdentity::from_mac([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]),
            port: 1,
        };
        // This must succeed without root. If it starts returning EACCES, the bind
        // address has been changed back to the group — see `bind_multicast`.
        let mut port = PtpPort::new(Ipv4Addr::new(127, 0, 0, 1), identity, 0)
            .expect("joining the PTP groups must not need privileges");

        assert_eq!(port.lock_state(), LockState::Unlocked);
        assert!(port.status().grandmaster.is_none());
        // Draining an empty domain must be harmless and must not invent a master.
        assert_eq!(port.poll().unwrap(), vec![]);
        assert_eq!(port.status().masters_seen, 0);
    }

    #[test]
    fn ptp_now_tracks_local_time_until_the_servo_moves_it() {
        let identity = PortIdentity { clock: ClockIdentity([1; 8]), port: 1 };
        let Ok(port) = PtpPort::new(Ipv4Addr::new(127, 0, 0, 1), identity, 0) else {
            eprintln!("skipping: could not join PTP groups");
            return;
        };
        let a = port.ptp_now();
        std::thread::sleep(Duration::from_millis(20));
        let b = port.ptp_now();
        let elapsed = b.diff_nanos(a);
        assert!(
            (15_000_000..100_000_000).contains(&elapsed),
            "ptp_now should advance with real time, moved {elapsed} ns"
        );
    }
}
