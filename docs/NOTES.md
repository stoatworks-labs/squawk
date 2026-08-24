# Notes

Working notes for this repo: status, decisions, and the traps that have actually bitten.
Migrated out of Claude Code's memory on 2026-08-24, so they are written in the first
person and dated by when each thing was learned — that date is usually the useful part.

Cross-cutting notes that are not specific to this repo live in
[fleet-notes](https://github.com/stoatworks-labs/fleet-notes).

*squawk — open partyline intercom (Green-Go/Bolero shaped), Rust workspace; core + mix-minus engine built and tested, no transport or binaries yet*

**squawk** (`~/Projects/squawk`, started 2026-08-06) — open partyline intercom system.
Server mixes, endpoints talk/listen; up to 10 keys per endpoint, each key its own AES67
stream carrying a partyline mix-minus or a direct point-to-point path. **PUBLIC MIT**,
`stoatworks-labs/squawk`, branch `main`, pushed 2026-08-06. Still needs adding to the
[attributions system](https://github.com/stoatworks-labs/fleet-notes/blob/main/notes/project_attributions_system.md) run — it has no ATTRIBUTIONS.md or README marker block.

**Built and tested:** `squawk-core` (model/config/validation), `squawk-engine`
(mix-minus, per-stream limiter), `squawk-server` (engine host thread, REST + WebSocket,
browser UI at `:8477` — assignment matrix with per-key talk and meters), `squawk-rtp`
(RTP/L24, AES67 SDP, jitter buffer, sockets — tested over real UDP loopback). 90 tests,
clippy clean. Launch config `squawk-server` is in the central `~/.claude/launch.json`.
**Transport IS wired in** (2026-08-07): real AES67 multicast in and out of the running
server, mix-minus sample-exact through the round trip (talker's own ear 0.000000,
listener exactly the level sent, depth held at target, zero loss/resync). **But only
over `lo0` against itself** — never a real NIC, switch, or another vendor's device.
**PTP drives the media clock** (2026-08-07): RTP timestamps are PTP time in samples,
within 2.4 ms of the grandmaster with 55 samples of wander; the audio loop is paced from
PTP too. Locks in ~0.6 s at **±40 us**. But only ever against
`squawk_ptp::testing::Grandmaster` on loopback — **no real grandmaster has ever been
seen**. Missing: SAP, send batching, tray packaging, desktop client, WebRTC leg,
hardware. Without `--interface` it falls back to synthesised tones and says so;
`--ptp-domain` is separate and without it the media clock is the server's own.

**Software timestamping ⇒ ±1 sample at 48 kHz**, not sub-microsecond. Fine for speech,
not for phase-coherent summing. macOS has NO hardware timestamping; Linux
`SO_TIMESTAMPING` is the upgrade path, unimplemented.

Measured: **292k packets/sec unbatched** from one thread (3.4 us each) vs a 320k pps
target for 32 endpoints × 10 keys — so per-tick send batching is a requirement, and
`sendmmsg` is Linux-only (macOS needs threads or `sendmsg_x`).

Measured: 320 simultaneous streams, every key talking, mixes at 29x realtime (3.4% of
one core) in release. **Packet rate is the scaling limit, not mixing or bandwidth** —
320 streams at 1 ms ptime is 320k pps, so the transport must batch `sendmmsg` per tick
from its first commit.

**Design decisions that are load-bearing (all documented in its AGENTS.md):**
- Partyline membership is **derived from key assignments** — no separate member list, so
  nothing can drift. "Assign endpoint to partyline" allocates a key slot.
- Mix-minus subtracts the *stored contribution buffer*, so bus trim is folded into each
  contribution (never applied to the summed bus) and the limiter is per-stream *after*
  the subtraction. Both alternatives leak the talker into their own ear silently.
- Two keys on one endpoint at the same target is a **validation error**, not a warning.
- Block size *is* the packet time (48 samples @ 48 kHz); never re-block.
- **Talk intent lives in the server host**, keyed by id+slot, re-applied after each
  engine rebuild. In the engine it would be wiped by any config edit.
- **`ProblemKind` must stay adjacently tagged** (`tag` + `content`). Internal tagging
  compiles and then 500s every endpoint that reports a problem — but *only when a
  problem exists*, so a clean config hides it entirely from browser checks.
- **Jitter buffer indexes on RTP timestamp** and tracks its ring slot alongside the
  playout point; deriving the slot from the timestamp breaks at the 2^32 wrap (~every
  24.8 h at 48 kHz). In `push`, the range check MUST precede the alignment check or a
  restarted sender reads as permanently misaligned and never recovers.
- **Jitter capacity ≠ latency** — depth is the delay, capacity (64 pkts) is burst
  headroom for a descheduled receive thread.
- **Multicast groups allocate sequentially from one base**: IPv4 multicast copies only
  the low 23 bits into the Ethernet MAC, so a scattered scheme makes two streams share
  a MAC and IGMP snooping delivers both to whoever joined either.
- **`socket2` is load-bearing**, not convenience — std cannot set `IP_MULTICAST_IF`
  (wrong NIC, silent success), `SO_REUSEADDR`/`SO_REUSEPORT` (every stream shares port
  5004 by design) or `SO_RCVBUF`.
- **A DEBUG build cannot hold the 1 ms tick** — 9802 blocks + 1.1% late over 10 s vs
  10030 and zero in release. Check the build profile FIRST on any drift/dropout report.
- **Multicast groups derive from `(endpoint index, key slot)`**, never the engine's
  stream index (which shifts when a key is added anywhere before it). Endpoint
  reordering still shifts groups — persisting per endpoint id is unfinished.
- **Receivers bind the group address on Unix**, not the wildcard: a wildcard-bound
  socket gets every group joined on that port, so 320 receivers would each wake for all
  320 streams — quadratic CPU, no error.
- **Blocks per wake stays 1 on the network** — bursting costs every receiver jitter depth.
- On this Mac the default route for **239/8 pointed at a VPN tunnel (`utun5`)** — a live
  example of why `--interface` is required rather than auto-detected.
- **PTP must NOT be polled on the audio thread** — polling at 1 ms quantises Sync receive
  timestamps while our Delay_Reqs go out immediately; PTP reads that asymmetry as offset
  (~150 us, never locks). Own thread @100 us ⇒ ±40 us. Audio thread reads a `SharedClock`
  (relaxed atomics).
- **Pace the audio loop from PTP, not just the timestamps** — else a local crystal error
  accumulates ~1 sample/s per 20 ppm into a snap-back glitch every couple of minutes.
- **`PtpPort` must carry its own domain into Delay_Reqs** — hardcoded 0 works by accident
  on domain 0 and fails SILENTLY on any other (SMPTE 2059-2 = domain 127).
- **Hold pending Syncs in a map, not one slot** — `poll` drains the event port before the
  general port, so a backlog otherwise loses every Follow_Up but the last.
- Test isolation: squawk's integration tests share multicast groups/ports. **Kill any
  running preview server before `cargo test`** or receivers lock onto its SSRC.
- **PTP servo must measure on the CORRECTED clock** (`ptp_now`, not `local_now`) or its
  input never responds to its output: same offset forever, steps repeatedly, never locks.
- **PTP lock needs hysteresis** (credit 1 / debit 4) — one outlier otherwise drops lock
  and everything gating on lock flaps.
- **macOS: a privileged-port bind is DENIED on a specific address but ALLOWED on the
  wildcard** — `bind(224.0.1.129:319)` = EACCES, `bind(0.0.0.0:319)` = OK. Presents as
  "Permission denied" and looks exactly like needing root. So PTP binds the wildcard
  while RTP binds the group — opposite choices, both correct.

**Forced by physics, not preference:** phones/browsers can never do AES67 (no PTP; wifi
multicast is sent at basic rate with no retries) so they get Opus/WebRTC and a
server-side fold-down — ~40–80 ms vs ~6–16 ms on the wired leg. That asymmetry is a
product constraint: fine for a producer, not for a followspot op taking a cue.

Server tray app should wrap [av launcher](https://github.com/stoatworks-labs/av-launcher/blob/main/docs/NOTES.md) (`av-launcher`) rather than build its own tray, per
the srt-router/flock/RFutils pattern. Related: [dante babelbox](https://github.com/stoatworks-labs/Dante-BabelBox/blob/main/docs/NOTES.md) (`Dante-BabelBox`) (workspace
shape), [leqtion](https://github.com/stoatworks-labs/LEQtion/blob/main/docs/NOTES.md) (`LEQtion`) (cpal audio I/O to lift for the client).
