//! Send a paced AES67 tone to a multicast group.
//!
//! A test signal generator for the audio network — useful for feeding a squawk server
//! without a client, and for checking that a third-party AES67 receiver hears what this
//! crate produces.
//!
//! ```text
//! cargo run -p squawk-rtp --example tone -- --iface 127.0.0.1 --group 239.69.128.0 --hz 440
//! ```
//!
//! Packets are paced by spinning to a deadline rather than by sleeping. `thread::sleep`
//! overshoots a 1 ms period by a large and variable fraction, and a receiver would read
//! the result as a slow sender and drift against it.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use clap::Parser;
use squawk_rtp::sdp::DEFAULT_RTP_PORT;
use squawk_rtp::StreamSender;

#[derive(Parser)]
#[command(about = "Send an AES67 L24 tone to a multicast group")]
struct Args {
    /// Local address of the NIC to send from. Not optional: without it the OS picks,
    /// and it usually picks wrong on a multi-homed machine.
    #[arg(long)]
    iface: Ipv4Addr,

    /// Destination multicast group.
    #[arg(long)]
    group: Ipv4Addr,

    #[arg(long, default_value_t = DEFAULT_RTP_PORT)]
    port: u16,

    /// Tone frequency. 0 sends silence, which is useful for holding a stream open.
    #[arg(long, default_value_t = 440.0)]
    hz: f32,

    /// Peak level, linear. 0.4 is about -8 dBFS.
    #[arg(long, default_value_t = 0.4)]
    level: f32,

    #[arg(long, default_value_t = 48_000)]
    rate: u32,

    /// Samples per packet. 48 is the AES67 default 1 ms at 48 kHz.
    #[arg(long, default_value_t = 48)]
    block: usize,

    #[arg(long, default_value_t = 0x7E57_0001)]
    ssrc: u32,

    /// Seconds to send for. 0 runs until interrupted.
    #[arg(long, default_value_t = 0.0)]
    seconds: f32,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();

    let mut tx = StreamSender::new(
        args.iface, args.group, args.port, args.block, 96, args.ssrc, 32,
    )?;

    let period = Duration::from_nanos(args.block as u64 * 1_000_000_000 / args.rate as u64);
    let step = std::f32::consts::TAU * args.hz / args.rate as f32;
    let mut phase = 0.0f32;
    let mut buf = vec![0.0f32; args.block];

    println!(
        "sending {} Hz at {:.2} to {}:{} from {} — {} samples every {:?}",
        args.hz, args.level, args.group, args.port, args.iface, args.block, period
    );

    let start = Instant::now();
    let mut next = Instant::now();
    let mut sent = 0u64;

    loop {
        for s in buf.iter_mut() {
            *s = if args.hz == 0.0 { 0.0 } else { args.level * phase.sin() };
            phase += step;
            if phase > std::f32::consts::TAU {
                phase -= std::f32::consts::TAU;
            }
        }
        tx.send(&buf)?;
        sent += 1;

        if args.seconds > 0.0 && start.elapsed().as_secs_f32() >= args.seconds {
            println!("sent {sent} packets");
            return Ok(());
        }

        next += period;
        let now = Instant::now();
        if next <= now {
            next = now;
            continue;
        }
        if next - now > Duration::from_micros(400) {
            std::thread::sleep(next - now - Duration::from_micros(400));
        }
        while Instant::now() < next {
            std::hint::spin_loop();
        }
    }
}
