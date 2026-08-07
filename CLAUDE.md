# squawk

Open partyline intercom (Green-Go / Bolero / Clear-Com shaped). Server mixes, endpoints
talk and listen; up to 10 keys per endpoint, each key its own stream carrying a
partyline mix-minus or a direct path. Rust workspace, MIT, public.

**Read `AGENTS.md` before changing the engine** — it lists the traps, and most of them
fail silently.

## Status
Five crates built and tested. Audio goes in and out over **real AES67 multicast**,
mix-minus holds sample-exact through the round trip, and **PTP drives the media clock**
so RTP timestamps are PTP time in samples. But all of it only over `lo0` against a
*synthetic* grandmaster — never a real NIC, switch, or another vendor's device. No SAP,
no client, no audio device, no hardware. Don't describe it as working intercom.

## Commands
```bash
cargo test --workspace
cargo clippy --workspace --all-targets
# UI on :8477. Without --interface it runs on synthesised tones and says so.
cargo run --release -p squawk-server -- --config squawk.example.toml --interface 127.0.0.1
cargo run --release -p squawk-rtp --example tone -- --iface 127.0.0.1 --group 239.69.128.0
cargo run --release -p squawk-ptp --example grandmaster -- --iface 127.0.0.1  # bench clock
cargo run --release -p squawk-ptp --example ptpmon -- --iface <ip> --domain 0  # what's out there
cargo test --release -p squawk-engine --test scale -- --nocapture   # timing
```
**Always run the server in release** — debug cannot hold the 1 ms tick (9802 blocks and
1.1% late ticks over 10 s, vs 10030 and none in release).

## Layout
- `crates/squawk-core` — data model, TOML config, validation
- `crates/squawk-engine` — mix-minus engine, per-stream limiter
- `crates/squawk-ptp` — PTPv2 messages, BMCA, servo, media clock
- `crates/squawk-rtp` — RTP/L24, AES67 SDP, jitter buffer, UDP sockets
- `crates/squawk-server` — engine host thread, REST + WebSocket, `static/index.html`
- `crates/diag` — vendored fleet diagnostics
- `squawk.example.toml` — worked system, parsed by a test so it can't rot

## The six things that fail silently
- **Two keys at one target** breaks mix-minus (validation error — keep it an error).
- **Bus trim** is folded into each contribution, never applied to the summed bus.
- **Limiter** stays per-stream and post-subtraction.
- **`contributes_to: None`** ≠ "produces nothing" — direct keys produce a contribution
  with no bus. Production is gated on `can_talk`.
- **Talk intent lives in the host**, keyed by id+slot, and is re-applied after every
  rebuild. In the engine it would be wiped by any config edit.
- **`ProblemKind` stays adjacently tagged** (`tag` + `content`). Internal tagging
  compiles and then 500s — but only when there is a problem to report, so a clean
  config hides it.
- **Jitter buffer indexes on RTP timestamp**, and tracks its ring slot alongside the
  playout point. Deriving the slot from the timestamp breaks at the 2^32 wrap.
- **Multicast groups allocate sequentially** from one base — IPv4 multicast only puts
  the low 23 bits into the MAC, so a scattered scheme makes two streams share one.
- **Jitter capacity ≠ latency.** Depth is the delay; capacity is burst headroom for a
  descheduled receive thread.
- **Multicast groups derive from `(endpoint index, slot)`**, never the engine's stream
  index — that shifts whenever a key is added anywhere before it.
- **Blocks per wake stays 1 on the network.** Bursting costs every receiver jitter
  buffer depth.
- **PTP measures on the corrected clock** (`ptp_now`, not `local_now`) or the servo's
  input never responds to its output and it never converges.
- **Lock has hysteresis** — one outlier must not drop it.
- **PTP binds the wildcard, RTP binds the group** — opposite, both right. On macOS a
  privileged-port bind is denied on a specific address but allowed on the wildcard.
- **PTP runs on its own thread**, never the audio loop: polling at audio rate quantises
  Sync timestamps and PTP reads that asymmetry as offset (~150 us, never locks).
- **Pace the audio loop from PTP**, not just the timestamps, or a local crystal error
  accumulates into a snap-back glitch every couple of minutes.
- **`PtpPort` must carry its own domain** into Delay_Reqs — sending them on domain 0
  works by accident there and fails silently everywhere else.

## Committed by physics
Phones/browsers get Opus/WebRTC, never AES67 (no PTP, wifi multicast is unreliable).
Packet rate is the scaling limit, not bandwidth — batch with `sendmmsg` per tick. Block
size *is* the packet time (48 samples @ 48 kHz); never re-block.

## Notes
Public repo. "Commit" = commit **and** push. Server tray app should wrap `av-launcher`
rather than building its own tray.

## Diagnostics
Log via `tracing`; `crates/diag` adds a rotating file, in-memory ring and a panic hook
writing a JSON crash report. Wire it as the **first** thing in `main` and **hold the
returned guard** — dropping it silently stops the log file being written.
