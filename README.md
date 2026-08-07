# squawk

> **AI-assisted project.** This codebase was created with [Claude](https://claude.com/claude-code)
> (Anthropic), directed and reviewed by a human author. Audio goes in and out of the
> server over real AES67 multicast, and mix-minus is asserted sample-exact through that
> full round trip. **But it has only ever run over the loopback interface, against
> itself.** There is no PTP, no SAP, no client, and no hardware; nothing here has met
> another vendor's device, a real switch, or a microphone.

An open **partyline intercom** system — the software half of a Green-Go / Bolero /
Clear-Com style comms rig. A server mixes; endpoints talk and listen.

Each endpoint gets up to **10 keys**, and each key is its own AES67 stream carrying
either a **partyline** (the bus, minus that endpoint's own voice) or a **direct**
point-to-point path. Because the keys arrive separately, a panel changes its own key
levels, mutes and ear placement instantly and locally, with no round trip to the server.

## Status

| Piece | State |
|---|---|
| Data model, config, validation (`squawk-core`) | Built and tested |
| Mix-minus engine (`squawk-engine`) | Built and tested |
| Server + browser UI (`squawk-server`) | Built and tested |
| AES67 packets, SDP, jitter buffer, sockets (`squawk-rtp`) | Built and tested over real UDP |
| Transport wired into the server | **Working** — audio in and out over real multicast |
| SAP discovery | Not started |
| PTP clock | Not started |
| Tray packaging (via `av-launcher`) | Not started |
| Desktop client station | Not started |
| Opus/WebRTC leg for phones and browsers | Not started |
| Hardware endpoint (ESP32 / Pi class) | Not started |

## The two transports, and why there have to be two

Wired clients and hardware panels get real **AES67**: L24 RTP at 1 ms packet time,
PTP-locked, one stream per key.

Phones, browsers and anything off-LAN get **Opus over WebRTC** instead. This is not a
shortcut — it is forced. No phone exposes PTP, and wifi multicast is transmitted at the
basic rate with no retries, so AES67 over wifi does not degrade gracefully, it falls
apart. The server is the bridge between the two domains.

The mix engine is deliberately unaware of any of this. It emits one stream per key; the
fold-down to a stereo Opus mix happens downstream. One mixing implementation, one set of
tests.

## Mix-minus

Every member of a partyline hears the bus minus their own voice. Read literally that is
a separate sum per member — O(members²). It isn't necessary:

```
contribution[key]  = mic × talk_ramp × bus_trim      (once per key)
bus[p]             = Σ contribution[keys on p]        (once per bus)
feed[key on p]     = bus[p] − contribution[key]       (once per key)
```

The subtraction is *exact*, not approximate, because it removes precisely the buffer
that was added — same samples, same gain, same block. Two things follow, and both are
load-bearing:

- **Every input must already be aligned into the server's clock domain.** An engine fed
  unaligned inputs leaks the talker back into their own ear, quietly and unfixably.
- **The bus trim is folded into each contribution, not applied to the summed bus.**
  Trimming after the sum would subtract an untrimmed buffer from a trimmed one and
  leave `(1 − trim)` of the talker in their own ear — inaudible at unity gain, and a
  mystery at any other setting. There is a test named for this.

The output limiter sits **per stream, after the subtraction**, for the same reason.

## Cost

Measured on an M-series Mac, release build, 32 endpoints × 10 keys = 320 streams, every
key talking at once — the worst case the engine can be put in:

```
1.000 s of audio mixed in 34.4 ms   (29x realtime, 3.4% of one core)
```

Mixing is not the constraint. **Packet rate is.** Those 320 streams at 1 ms are 320,000
packets per second outbound, which has to be batched per tick (`sendmmsg`) from the
start — it is not something that can be retrofitted. Bandwidth is the lesser problem at
roughly 1.6 Mbit/s per stream.

## The transport

`squawk-rtp` does RTP/L24 packetisation, AES67 session descriptions, the receive-side
jitter buffer, and the sockets. It is tested over real UDP, not against a mock: audio
goes out of one socket and comes back in through another, sample-identical.

Three things in there are worth knowing about, because each of them fails quietly:

- **The jitter buffer is indexed by RTP timestamp, not by sequence number.** The
  timestamp *is* the media clock; the sequence number is only a counter. A
  sequence-indexed ring scatters audio into the wrong slots at the 2^32 timestamp
  rollover — once every ~24.8 hours at 48 kHz, so reliably mid-show.
- **Multicast groups are allocated sequentially from one base.** IPv4 multicast copies
  only the **low 23 bits** of the address into the Ethernet MAC, so 239.69.1.1 and
  239.197.1.1 become the same MAC and an IGMP-snooping switch delivers both to anyone
  who joined either. Sequential allocation keeps those bits distinct.
- **The buffer's capacity is not its latency.** Depth sets the delay; capacity is
  headroom for the case where the receiving thread is descheduled and `poll` then hands
  over 40 packets at once. A ring sized to the depth throws away audio it had already
  received.

Measured on an M-series Mac, one thread, unbatched:

```
send_to: 292,000 packets/sec   (3.4 us per packet) — about 292 streams at 1 ms
```

Which lands just under the 320 streams a 32-endpoint system implies. That is the
measurement that turns per-tick send batching from an optimisation into a requirement.

### Addressing

Multicast groups are derived from `(endpoint index, key slot)`, not from the engine's
stream index. The engine numbers streams sequentially, so adding a key to endpoint 0
shifts every stream after it — addressing off that number would silently re-point every
multicast group in the building each time somebody added a key in the UI. Deriving from
identity means adding a key moves nothing else.

Reordering or deleting *endpoints* still shifts things. The real fix is to persist an
allocation per endpoint id, and it is worth doing before anyone deploys this.

## The browser UI

An assignment matrix — endpoints down, partylines across. Click a cell to give that
endpoint a key on that partyline; the key lands in the next free slot. Each cell carries
its own **talk** button and a meter showing what that key is actually hearing, so
mix-minus is visible rather than theoretical: key someone up and every other meter on the
bus rises while their own stays dark.

Direct lines get their own section, since a point-to-point pair is not a row-and-column
thing. Creating one makes **both** halves — a direct key with no key back carries
silence, and a UI that can produce one can produce a dead button.

Validation problems appear live at the top, errors and warnings distinguished. Errors
reject an edit; warnings do not, because an empty partyline or a half-patched direct
line is a normal thing to be in the middle of.

## Run it

Without an interface, every microphone is a synthesised tone and nothing reaches the
network — useful for looking at the UI, and the page says so in as many words:

```bash
cargo run --release -p squawk-server -- --config squawk.example.toml
```

With one, it receives and transmits real AES67:

```bash
cargo run --release -p squawk-server -- --config squawk.example.toml --interface 192.168.1.90
```

Either way the UI is at <http://localhost:8477>.

`--interface` is required rather than auto-detected, and that is deliberate. On a
multi-homed machine — which is every AV server — the routing table usually prefers the
office LAN, and multicast sent the wrong way reports success and is heard by nothing.
On the Mac this was developed on, the default route for 239.0.0.0/8 pointed at a **VPN
tunnel**.

**Run the server in release.** A debug build cannot hold the 1 ms tick: measured over
10 seconds with 16 streams, debug ran 9802 blocks with 1.1% of ticks late, release ran
10030 with none.

To feed a stream without a client — for testing against real gear, too:

```bash
cargo run --release -p squawk-rtp --example tone -- \
  --iface 127.0.0.1 --group 239.69.128.0 --hz 440
```

## Layout

```
crates/squawk-core      Data model, TOML config, validation
crates/squawk-engine    Mix-minus engine and per-stream limiter
crates/squawk-rtp       RTP/L24, AES67 SDP, jitter buffer, UDP sockets
crates/squawk-server    Engine host, HTTP/WebSocket API, browser UI
crates/diag             Vendored fleet diagnostics
squawk.example.toml     A worked four-partyline theatre system
docs/ARCHITECTURE.md    Signal path, clock domains, transport split
```

## Test it

```bash
cargo test --workspace
```

For the scale test's timing:

```bash
cargo test --release -p squawk-engine --test scale -- --nocapture
```

<!-- attributions:start -->
This project is built on other people's work — see [ATTRIBUTIONS.md](ATTRIBUTIONS.md).
<!-- attributions:end -->

## Licence

MIT — see [LICENSE](LICENSE).
