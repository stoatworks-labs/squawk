# squawk

Open partyline intercom (Green-Go / Bolero / Clear-Com shaped). Server mixes, endpoints
talk and listen; up to 10 keys per endpoint, each key its own stream carrying a
partyline mix-minus or a direct path. Rust workspace, MIT, public.

**Read `AGENTS.md` before changing the engine** — it lists the traps, and most of them
fail silently.

## Status
`squawk-core` + `squawk-engine` built and tested. **No transport, no binaries, no
audio I/O, no hardware.** Nothing has touched a network card or a clock. Don't describe
it as working intercom.

## Commands
```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo test --release -p squawk-engine --test scale -- --nocapture   # timing
```

## Layout
- `crates/squawk-core` — data model, TOML config, validation
- `crates/squawk-engine` — mix-minus engine, per-stream limiter
- `crates/diag` — vendored fleet diagnostics
- `squawk.example.toml` — worked system, parsed by a test so it can't rot

## The four things that fail silently
- **Two keys at one target** breaks mix-minus (validation error — keep it an error).
- **Bus trim** is folded into each contribution, never applied to the summed bus.
- **Limiter** stays per-stream and post-subtraction.
- **`contributes_to: None`** ≠ "produces nothing" — direct keys produce a contribution
  with no bus. Production is gated on `can_talk`.

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
