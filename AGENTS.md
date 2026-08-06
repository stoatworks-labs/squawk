# AGENTS.md — bringing an LLM up to speed on squawk

Orientation for an AI assistant (or a new human) picking this project up cold.
`CLAUDE.md` holds the short command reference; this file explains the model and the
traps.

---

## 1. What this is

An open **partyline intercom** — Green-Go / Bolero / Clear-Com shaped. A server mixes,
endpoints talk and listen. Each endpoint has up to 10 keys; each key is its own stream
carrying either a partyline (mix-minus) or a direct point-to-point path.

Rust workspace. Currently **two crates and no binaries** — the engine exists, nothing
that plays audio or sends a packet does.

## 2. What is actually verified

Be precise about this in anything you write for the user. The engine's behaviour is
covered by tests that assert *sample-exact* mix-minus, and they pass. Beyond that:

- No RTP packet has ever been sent or received.
- No PTP clock has been locked.
- No audio device has been opened.
- No hardware exists.

Do not describe this project as working intercom. It is a mixer with a test bench.

## 3. The model, in one paragraph

`squawk-core` holds *authored* state: partylines, endpoints, keys. `squawk-engine`
compiles that into a routing table of integer indices and mixes blocks. Runtime state
(talk buttons, meters) lives in the engine, never in the config. The engine emits one
stream per key and knows nothing about transports.

### Partyline membership is derived from keys

**There is no member list on a partyline.** An endpoint is a member if and only if one
of its keys targets that partyline. `Config::members_of()` derives it.

This is deliberate. A membership list *and* a key list is two sources of truth for one
fact, and they drift the instant anything edits one without the other — the classic
intercom-config bug where a panel shows a key that routes nowhere. The UI still offers
"assign endpoint to partyline"; it implements it as `Endpoint::assign()`, which
allocates a free key slot.

## 4. The traps

### Two keys on one endpoint pointing at the same target breaks mix-minus

Each key's feed subtracts only *its own* contribution. Two keys at the same target means
the endpoint hears itself back through the other one. This is a **validation error**,
not a warning, and `Engine::new` refuses to build. Don't relax it.

### Bus trim is folded into the contribution, not applied to the bus

`stage_contributions` multiplies the trim in as it writes each contribution, so the
buffer mix-minus subtracts is byte-for-byte the buffer that was added. If you ever move
the trim to the summed bus, every talker gets `(1 − trim)` of themselves back. It is
silent at unity gain, which is exactly why it would ship. Test:
`bus_trim_does_not_leak_the_talker_into_their_own_feed`.

### The limiter must stay per-stream and post-subtraction

Limiting the bus before subtraction makes the subtraction wrong — gain reduction applies
to the sum but not to the contribution being removed, so the talker leaks into their own
ear precisely when the bus is busiest.

### `contributes_to` is not the test for "does this key produce audio"

`contributes_to: None` means two different things: a listen-only key (nothing to
produce) and a **direct key** (produces a contribution, but has no bus to sum it onto —
the far end reads that contribution buffer as its source). Production is gated on
`can_talk`. Getting this wrong makes every direct key silent, and only the direct-key
test catches it.

### The engine must stay aligned-input-only

Mix-minus is exact because it subtracts the same samples that were added. Whatever
feeds `process()` must already have resampled and aligned every input into the server's
clock domain. That is the jitter buffer's job, and it does not exist yet. An engine fed
unaligned inputs fails quietly.

### Stream index ordering is load-bearing

Streams are allocated endpoint-major then slot-order, and the RTP layer will pin
destinations to those indices for the life of a config. Test:
`stream_indices_are_allocated_in_endpoint_then_slot_order`.

## 5. Physics the design is already committed to

- **Phones and browsers cannot do AES67.** No PTP, and wifi multicast is sent at the
  basic rate with no retries. They get Opus/WebRTC and a server-side fold-down. Do not
  "add AES67 support to the mobile app".
- **Packet rate, not bandwidth, is the scaling limit.** 320 streams at 1 ms ptime is
  320k pps. The transport must batch per tick (`sendmmsg`) from its first commit.
- **Block size is the packet time.** 48 samples at 48 kHz = 1 ms = one RTP packet. Never
  re-block between the mixer and the network.

## 6. Commands

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo test --release -p squawk-engine --test scale -- --nocapture   # timing
```

## 7. Where this is going

See the task list in the README status table. Next up is the AES67 transport, then the
server tray app (which should wrap [`av-launcher`](../av-launcher) rather than build its
own tray — that is the established fleet pattern for "tray app with a browser UI", used
by srt-router, flock and RFutils).
