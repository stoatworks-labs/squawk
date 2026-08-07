//! Watch a PTP domain and report what is on it.
//!
//! ```text
//! cargo run -p squawk-ptp --example ptpmon -- --iface 192.168.1.90 --domain 0
//! ```
//!
//! Answers the questions that come up on site before anything else: is there a
//! grandmaster, which one, on which domain, and how far off are we from it. A domain
//! with no announces at all reports exactly that, rather than sitting silent — "no
//! grandmaster" and "listening on the wrong domain" look identical from the outside,
//! and this is the tool that tells them apart.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use clap::Parser;
use squawk_ptp::message::{ClockIdentity, PortIdentity};
use squawk_ptp::servo::LockState;
use squawk_ptp::PtpPort;

#[derive(Parser)]
#[command(about = "Watch a PTP domain")]
struct Args {
    /// Local address of the NIC on the audio network.
    #[arg(long)]
    iface: Ipv4Addr,

    /// PTP domain. AES67's default is 0; SMPTE 2059-2 usually uses 127.
    #[arg(long, default_value_t = 0)]
    domain: u8,

    /// Seconds to watch for. 0 runs until interrupted.
    #[arg(long, default_value_t = 30.0)]
    seconds: f32,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();

    // A locally-administered identity, so this cannot be mistaken for real gear or
    // collide with anything on the domain.
    let identity = PortIdentity {
        clock: ClockIdentity::from_mac([0x02, 0x73, 0x71, 0x77, 0x6B, 0x01]),
        port: 1,
    };

    let mut port = PtpPort::new(args.iface, identity, args.domain)?;
    println!("watching PTP domain {} on {}\n", args.domain, args.iface);

    let start = Instant::now();
    let mut last_report = Instant::now();
    let mut reported_nothing = false;

    loop {
        for event in port.poll()? {
            println!("[{:6.1}s] {event:?}", start.elapsed().as_secs_f32());
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            let s = port.status();
            match &s.grandmaster {
                Some(gm) => {
                    reported_nothing = false;
                    let lock = match s.state {
                        LockState::Unlocked => "unlocked",
                        LockState::Locking => "locking",
                        LockState::Locked => "LOCKED",
                    };
                    println!(
                        "[{:6.1}s] gm {gm}  {lock}  offset {:+.3} us  delay {:.3} us  \
                         steps {}  (sync {} follow-up {} delay-resp {} unmatched {})",
                        start.elapsed().as_secs_f32(),
                        s.offset_nanos as f64 / 1000.0,
                        s.delay_nanos as f64 / 1000.0,
                        s.steps,
                        s.stats.syncs,
                        s.stats.follow_ups,
                        s.stats.delay_resps,
                        s.stats.unmatched,
                    );
                }
                None if !reported_nothing => {
                    reported_nothing = true;
                    let s = port.status();
                    println!(
                        "[{:6.1}s] no grandmaster on domain {}. \
                         {} announce(s), {} message(s) from other domains — \
                         if the latter is climbing, try --domain 127.",
                        start.elapsed().as_secs_f32(),
                        args.domain,
                        s.stats.announces,
                        s.stats.wrong_domain,
                    );
                }
                None => {}
            }
            last_report = Instant::now();
        }

        if args.seconds > 0.0 && start.elapsed().as_secs_f32() >= args.seconds {
            let s = port.status();
            println!("\nfinished: {:?}", s);
            return Ok(());
        }

        // Sleeping between drains costs timestamp accuracy, so keep it short.
        std::thread::sleep(Duration::from_millis(2));
    }
}
