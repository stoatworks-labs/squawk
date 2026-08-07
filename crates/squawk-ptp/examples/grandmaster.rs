//! Run a synthetic PTP grandmaster.
//!
//! ```text
//! cargo run --release -p squawk-ptp --example grandmaster -- --iface 127.0.0.1
//! ```
//!
//! A clock source for a bench with no real one. It announces a GPS-class clock and
//! answers Delay_Reqs, which is enough to lock a slave — but its time comes from the
//! host's system clock, so it is a *reference*, not an accurate one. Never leave this
//! running on a network with real gear on it: `priority1` of 128 and `clockClass` 6 will
//! win the BMCA against most equipment, and every device on the domain will follow a
//! laptop.

use std::net::Ipv4Addr;
use std::time::Duration;

use clap::Parser;
use squawk_ptp::testing::Grandmaster;

#[derive(Parser)]
#[command(about = "Run a synthetic PTP grandmaster (bench use only)")]
struct Args {
    /// Local address of the NIC to announce on.
    #[arg(long)]
    iface: Ipv4Addr,

    /// PTP domain.
    #[arg(long, default_value_t = 0)]
    domain: u8,

    /// Offset from the host's system clock, in milliseconds. A non-zero value makes it
    /// obvious whether a slave is actually following this clock or just happens to
    /// share the machine's.
    #[arg(long, default_value_t = 0.0)]
    offset_ms: f64,

    /// Seconds to run. 0 runs until interrupted.
    #[arg(long, default_value_t = 0.0)]
    seconds: f32,
}

fn main() {
    let args = Args::parse();
    let offset_nanos = (args.offset_ms * 1_000_000.0) as i64;

    let gm = Grandmaster::spawn(args.iface, args.domain, offset_nanos);
    println!(
        "synthetic grandmaster {} on domain {} via {} (offset {:+} ms)\n\
         bench use only — this will win the BMCA against most real equipment",
        gm.identity().clock,
        args.domain,
        args.iface,
        args.offset_ms,
    );

    if args.seconds > 0.0 {
        std::thread::sleep(Duration::from_secs_f32(args.seconds));
        println!("stopping");
        return;
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
