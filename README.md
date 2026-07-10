# LowBand

**Peer-to-peer remote IT assistance and voice/video conferencing, engineered for low-bandwidth networks.**

LowBand targets the connections mainstream conferencing stacks leave behind: a full session — voice, legible screen share, and responsive remote control — is designed to be *functional at 64 kbps and pleasant at 150 kbps per direction*, on paths with 80–400 ms RTT and 1–5% bursty loss (3G, ADSL2, satellite, congested rural links). For comparison, Zoom-class stacks publish 600 kbps–1.8 Mbps recommendations for 1:1 video.

> **Status:** early development. The governor daemon, signaling service, transport, and codec subsystems build and pass the end-to-end suite; platform capture/injection backends are in progress.

## Key design points

- **True P2P, no media server.** Direct UDP between peers (hole-punched via a stateless rendezvous service), with a blind TURN relay as fallback — ciphertext forwarding only, no decryption or transcoding.
- **End-to-end encrypted by default.** Noise-IK key agreement bound to the out-of-band session code; ChaCha20-Poly1305 AEAD framing; automatic rekeying; scoped capability tokens for remote control consent.
- **LBTP, a purpose-built transport.** Delay-gradient congestion control tuned for self-induced queuing under 50 ms, adaptive FEC, connection migration by 64-bit Connection ID (a 3G handset changing IP mid-call resumes inside one RTT, QUIC-style).
- **A bitrate governor, not a free-for-all.** A single 10 Hz governor allocates every stream's budget from measured link capacity, thermal pressure, and CPU load, degrading through explicit quality tiers (Full → Comfortable → Constrained → Survival) without user-visible stalls.
- **Gear-based encoding.** Encoders expose discrete "gears" (neural head codec, AV1 SVC, palette/damage screen coding) selected by a startup capability probe — weak CPUs and NPU-less machines get a working session, not a slideshow.
- **Least-privilege architecture.** A minimal core daemon (`lowbandd`) owns network, crypto, and capture; the UI shell runs unprivileged and talks to it over a local IPC socket. The UI can crash without dropping the call.

## Architecture

```
┌─────────────────────────┐        stateless WebSocket/HTTP        ┌─────────────────────────┐
│  Peer A (technician)    │◄──────  signaling: session codes,  ───►│  Peer B (assisted)      │
│  lowbandd + UI shell    │         offers/answers, ICE, TURN      │  lowbandd + UI shell    │
└───────────┬─────────────┘                                        └───────────┬─────────────┘
            └────────────────── LBTP over UDP — E2EE media ────────────────────┘
```

| Crate | Purpose |
|---|---|
| `core/lowbandd` | Core daemon: governor loop, privilege drop, IPC event push |
| `core/lbtp` | Transport: pacing, congestion control, FEC, ARQ, migration |
| `core/crypto` | Noise-IK handshake, AEAD framing, rekey, capability tokens |
| `core/signaling` | Stateless rendezvous service (axum): session codes, SDP relay, TURN credentials |
| `core/platform` | Tier state machine, thermal/CPU monitoring, gear policy, IPC, capture/injection |
| `core/xfer` | File transfer: FastCDC chunking, BLAKE3 dedup, zstd, budget-aware scheduler |
| `core/messaging` | Reliable messaging lanes |
| `core/nn` | ONNX Runtime host: model registry, execution-provider probe, warm pools, watchdog |
| `core/obs` | Metrics, tracing, QoE probes |
| `shells` | UI shell components (join, consent, quality bar, session summary) |
| `tests/e2e` | End-to-end integration suite (signaling + use-case scenarios) |
| `bench` | Benchmarks |

Detailed specifications live in [`docs/`](docs/) — architecture, design, and PRD.

## Getting started

Requires a stable Rust toolchain. The workspace builds for `x86_64-unknown-linux-musl` by default (see `.cargo/config.toml`); binaries land in `target/x86_64-unknown-linux-musl/debug/`.

```bash
cargo build -p lowbandd -p lowband-signaling
```

Run the signaling service:

```bash
SIGNALING_BIND=127.0.0.1:3478 TURN_SHARED_SECRET=dev SIGNALING_DB=/tmp/lb-signaling-db \
  target/x86_64-unknown-linux-musl/debug/lowband-signaling
```

Run the core daemon:

```bash
target/x86_64-unknown-linux-musl/debug/lowbandd \
  --ipc-socket /tmp/lb.sock --data-dir /tmp/lb-data --link-bps 150000
```

### End-to-end smoke test

A single script builds both binaries, drives the full two-peer rendezvous flow (session code → offer/answer → ICE → TURN → connected), then attaches an IPC probe client to the daemon and verifies governor events:

```bash
.claude/skills/run-lowband/smoke.sh
```

### Tests

```bash
cargo test --workspace
```

## License

[AGPL-3.0-only](https://www.gnu.org/licenses/agpl-3.0.html). See individual crate manifests.
