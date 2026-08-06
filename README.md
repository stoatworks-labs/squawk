# squawk

> **AI-assisted project.** This codebase was created with [Claude](https://claude.com/claude-code)
> (Anthropic), directed and reviewed by a human author. The mix engine is covered by
> behavioural tests that assert sample-exact mix-minus, and the server and browser UI
> have been exercised end to end against the real engine. **But every microphone is a
> synthesised tone.** Nothing here has yet touched a network card, a sound card, a PTP
> clock or an intercom panel — the AES67 transport, the clients and the hardware do
> not exist yet.

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
| Server + browser UI (`squawk-server`) | Built and tested, **on simulated audio** |
| AES67 transport — RTP, SDP/SAP, PTP | Not started |
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

```bash
cargo run -p squawk-server -- --config squawk.example.toml
```

Then open <http://localhost:8477>. Every microphone is a synthesised tone — see the
note at the foot of the page.

## Layout

```
crates/squawk-core      Data model, TOML config, validation
crates/squawk-engine    Mix-minus engine and per-stream limiter
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

## Licence

MIT — see [LICENSE](LICENSE).
