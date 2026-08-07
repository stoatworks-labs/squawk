# squawk

Open partyline intercom (Green-Go / Bolero / Clear-Com shaped). Server mixes, endpoints
talk and listen; up to 10 keys per endpoint, each key its own stream carrying a
partyline mix-minus or a direct path. Rust workspace, MIT, public.

**Read `AGENTS.md` before changing the engine** — it lists the traps, and most of them
fail silently.

## Status
All four crates built and tested. Audio goes in and out over **real AES67 multicast**
and mix-minus holds sample-exact through the round trip — but only over `lo0`, against
itself. No PTP (media clock is the server's own), no SAP, no client, no audio device,
no hardware. Don't describe it as working intercom.

## Commands
```bash
cargo test --workspace
cargo clippy --workspace --all-targets
# UI on :8477. Without --interface it runs on synthesised tones and says so.
cargo run --release -p squawk-server -- --config squawk.example.toml --interface 127.0.0.1
cargo run --release -p squawk-rtp --example tone -- --iface 127.0.0.1 --group 239.69.128.0
cargo test --release -p squawk-engine --test scale -- --nocapture   # timing
```
**Always run the server in release** — debug cannot hold the 1 ms tick (9802 blocks and
1.1% late ticks over 10 s, vs 10030 and none in release).

## Layout
- `crates/squawk-core` — data model, TOML config, validation
- `crates/squawk-engine` — mix-minus engine, per-stream limiter
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
