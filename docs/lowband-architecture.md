# LowBand — Technical Architecture Document

**System:** P2P remote IT-assist + voice/video conferencing for low-bandwidth networks (3G, ADSL2)
**Doc type:** Architecture specification (companion to the Design Document v1.0) · **Status:** Draft for review
**Audience:** Engineering — implementers of the core daemon, codecs, and transport

---

## 1. Purpose and Scope

This document specifies *how the system is built*: component decomposition, threading and data-flow architecture, the wire protocol, and — in the greatest depth — the algorithm specifications that constitute the system's competitive core. Where the Design Document argued *what* and *why*, this document is the implementation contract.

The controlling requirement, restated: a full session (voice + legible screen + responsive remote control) must be **functional at 64 kbps and pleasant at 150 kbps per direction**, on paths with 80–400 ms RTT and 1–5% bursty loss, degrading and recovering without user-visible stalls. Mainstream conferencing stacks (Zoom-class) publish ~600 kbps–1.8 Mbps recommendations for 1:1 video and are architected around server-side media routing and H.264/SVC-era coding with partial AV1 adoption. Every architectural choice below is justified against that baseline, layer by layer (§14).

## 2. Quality Attributes (Architecture Drivers)

| Attribute | Requirement | Primary architectural response |
|---|---|---|
| Uplink efficiency | Usable at 64 kbps total | Semantic coding (§9–11), zero-cost stillness, compact framing (§6.2) |
| Latency | Input-to-photon ≤ RTT + 60 ms; mouth-to-ear ≤ RTT/2 + 100 ms | P2P (no media server), pacing, no-keyframe recovery, cursor-as-metadata |
| Loss resilience | Transparent through 5% bursty loss | Codec-native redundancy (DRED), adaptive FEC (§7), SVC layering |
| Queue discipline | Self-induced queuing < 50 ms | Delay-gradient congestion control (§6.4), single pacer |
| Trust | E2EE, scoped consent, no silent synthesis | Noise-IK, capability tokens, labeled neural modes (§13) |
| Portability | Win/macOS/Linux; x86-64 & ARM64; degrade on weak CPU/no NPU | Gear system + capability probe (§8.3, §12) |

## 3. System Context

```
                        ┌────────────────────────────┐
                        │  Rendezvous service         │   stateless WebSocket:
                        │  (signaling + directory)    │   session codes, SDP-like
                        └──────────┬─────────────────┘   blobs, ICE candidates
                                   │
        ┌──────────────────────────┼──────────────────────────┐
        │                          │                          │
┌───────▼────────┐         ┌──────▼───────┐          ┌───────▼────────┐
│ Peer: technician│         │ STUN (addr    │          │ Peer: assisted │
│  lowbandd core  │         │ discovery)    │          │  lowbandd core │
│  + UI shell     │         └──────────────┘          │  + UI shell    │
└───────┬────────┘                                     └───────┬────────┘
        │            LBTP over UDP  (direct, hole-punched)     │
        └══════════════════════ E2EE media ════════════════════┘
                     ╲                                   ╱
                      ╲   fallback: TURN relay (blind   ╱
                       ╲  ciphertext forwarding only)  ╱
```

No media server exists. For 1:1 sessions this removes an encode/decode/route hop that server-centric architectures pay in both latency and infrastructure cost, and it is what frees the complexity budget for the codec layer. Group calls ≤ 4 use full mesh; beyond that an optional E2EE-preserving packet forwarder (no decryption, no transcoding) is specified in Appendix B.

## 4. Component Architecture

The peer runs as a privileged-minimal core daemon (`lowbandd`, Rust) plus a per-platform UI shell communicating over a local IPC socket (capnp/flatbuffer schema). The daemon owns network, crypto, codecs, capture, and injection; the shell owns rendering, consent dialogs, and settings. This split keeps the attack surface auditable and lets the UI crash without dropping the call.

```
lowbandd
├── lbtp/          transport: sockets, ICE, pacing, CC/BWE, FEC, ARQ, migration
├── crypto/        Noise-IK handshake, AEAD framing, rekey, capability tokens
├── gov/           governor: tier state machine, per-stream budget allocation
├── audio/         capture/playout, AEC/NS/AGC, Opus, neural vocoder, jitter buffer
├── video/         camera capture, Gear A (neural head), Gear B/C (AV1), rate ctrl
├── screen/        damage acquisition, classifier, palette/AV1 coders, refinement
├── control/       input events, cursor channel, clipboard, consent enforcement
├── xfer/          FastCDC chunking, BLAKE3 index, dedup cache, zstd, scheduler
├── nn/            ONNX Runtime host: model registry, EP probe (CoreML/NNAPI/
│                  DirectML/CPU), warm pools, watchdog
└── obs/           metrics, tracing, QoE probes (VMAF sample, OCR legibility)
```

Module dependency rule: everything may depend on `lbtp` and `gov` interfaces; media modules never depend on each other. The governor is the *only* component that allocates bitrate; encoders expose `set_budget(bps)` / `set_gear(g)` and are otherwise autonomous.

## 5. Concurrency and Data-Flow Architecture

The transport is a **single-threaded event loop** (io_uring on Linux, IOCP/kqueue elsewhere) owning the socket, the pacer wheel, congestion control, FEC, and (de)framing. Single-threading the hot path eliminates lock contention where it matters most and makes pacing exact. Everything else is workers:

```
capture threads          encode workers            transport loop (1 thread)
 mic ──► [SPSC ring] ──► audio enc ──► [SPSC] ──►┐
 cam ──► [SPSC ring] ──► video enc ──► [SPSC] ──►├─► framer ─► pacer ─► UDP
 scrn ─► [SPSC ring] ──► tile enc  ──► [SPSC] ──►┘        ▲
 input ─────────────────────────────────► [SPSC] ─────────┘ (priority lane)

 UDP ─► deframe ─► route ─► [SPSC per stream] ─► decode workers ─► render/playout
```

All inter-stage queues are lock-free SPSC rings carrying pooled, reference-counted buffers — the payload path is zero-copy from encoder output to AEAD input. Encode workers are a small pool pinned away from the transport core; the neural runtime gets its own pool with a watchdog (a stalled model must never stall the loop). The governor runs as a 10 Hz timer on the transport thread (it is cheap) so its decisions are synchronous with the rate estimate.

Backpressure rule: rings never block producers. A full media ring drops the *oldest superseded* unit (e.g., a stale screen tile replaced by a newer version of the same tile) — freshness beats completeness for realtime lanes; reliable lanes are flow-controlled explicitly.

## 6. LBTP — Wire Protocol Specification

### 6.1 Session establishment

Key agreement is **Noise-IK** bound to the out-of-band session code: the initiator learns the responder's static public key via signaling, message 1 carries `e, es, s, ss`, message 2 carries `e, ee, se`; traffic keys derive via HKDF and rekey every 2³⁰ datagrams or 15 minutes, whichever first. The session is identified by a random 64-bit **Connection ID**, not the 5-tuple: when a 3G handset changes IP mid-call, the peer validates the new path with a challenge/response and the session migrates without renegotiation (QUIC-style), typically inside one RTT.

### 6.2 Datagram framing

One datagram = one AEAD unit (ChaCha20-Poly1305, nonce = direction bit ‖ 47-bit sequence). Everything after the 11-byte public envelope is encrypted; the 16-byte tag is paid once per datagram, which is why the framer aggregates.

```
Datagram (≤1200 B):
  [1B  ver/flags][8B connection-id (short form: 0B after path stable)]
  [2B  seq16 lower bits]            ← full 47-bit seq reconstructed at receiver
  ── AEAD boundary ──
  Frame*  where Frame :=
  [1B  channel(5b) | type(3b)] [varint len] [payload]

Channels: 0 ctrl/ACK · 1 audio · 2 cursor · 3 input · 4 screen-rt · 5 video-rt
          6 screen-rel · 7 xfer · 8 probe/pad
Types:    DATA · FEC · ACKBLOCK · PATH_CHALLENGE/RESPONSE · PING
```

Worst-case overhead at survival tier (60 ms audio + cursor + ACK coalesced into one datagram per tick): IP/UDP 28 B + envelope 3 B + tag 16 B + frame headers ~6 B ≈ 53 B per 90–110 B of payload — ~10–12% tax versus the 45–60% a naive one-RTP-packet-per-frame stack pays at the same bitrate. That reclaimed overhead is ~20 kbps handed back to the codecs, for free.

### 6.3 Reliability classes

Channels 0/2/3 are reliable-ordered via a compact SACK stream (cumulative + up to 3 ranges, piggybacked on any outbound datagram; RTO from SRTT with Karn's rule). Channels 6/7 are reliable-unordered: receiver ACKs object *ranges* (tile IDs, chunk IDs), sender retransmits only unacked objects — a superseded screen tile is silently dropped from the retransmit queue. Channels 1/4/5 are never retransmitted; they are protected by §6.5 and codec redundancy.

### 6.4 Congestion control and bandwidth estimation

Delay-gradient primary, loss backstop, cellular-aware — specified precisely:

1. Receiver timestamps arrival of packet groups (~5 ms bins) and reports `(recv_time, bytes)` vectors on channel 0 every 50 ms.
2. Sender computes per-group one-way-delay variation `d(i) = (t_i − t_{i−1}) − (T_i − T_{i−1})`, smooths it, and runs a **trendline estimator**: least-squares slope `m̂` over the last ~20 smoothed samples (the GCC lineage).
3. Overuse is declared when `m̂ · N > γ(i)` persists ≥ 10 ms, where the threshold adapts `γ(i+1) = γ(i) + Δt · k(m̂) · (|m̂·N| − γ(i))`, `k_up ≫ k_down` — this adaptive γ is what prevents self-starvation when competing with loss-based flows on the same ADSL uplink.
4. Rate control state machine (Increase/Hold/Decrease): multiplicative increase ≈ +8%/s while underusing; on overuse, `rate ← 0.85 × measured_received_rate`; hold while the queue drains.
5. **Cellular mode** (entered on RAN-like jitter signatures: bimodal OWD spikes uncorrelated with our send rate): SCReAM-inspired — widen γ, cap decrease frequency, and gate increases on *queue-delay trend* rather than instantaneous spikes, so 3G scheduler jitter doesn't crash the rate every few seconds.
6. Loss backstop: if loss > 10% sustained, `rate ← rate × (1 − 0.5·loss)`.
7. Headroom discovery: paced probe trains on channel 8 (padding, 2× current rate for 15 ms bursts) fund tier upgrades with evidence; probes are the first thing the pacer drops.

The pacer releases bytes on a token bucket at the controlled rate with ≤ 5 ms burst tolerance; channel priority order inside the pacer is 0 > 3 > 2 > 1 > 4 > 5 > 6 > 7 > 8. Input beats media, always.

### 6.5 Forward error correction

Loss on radio links is bursty, so FEC is sized from a **Gilbert–Elliott** fit (states G/B, estimated `p_GB, p_BG, loss_B` from run-length statistics of the ACK stream), not from mean loss. Realtime channels use systematic Reed–Solomon over short blocks (audio: RS over 4–6 datagram groups; screen/video: per frame-batch), choosing the smallest `(n, k)` such that `P[losses > n−k in block | GE model] < 10⁻³`. Interleaving depth is bounded by the channel's latency budget (audio ≤ 1 frame duration). Bulk transfer (channel 7) uses RaptorQ-class fountain coding — the sender streams repair symbols until the object ACKs, which is optimal on high-RTT paths where per-gap ARQ round trips are ruinous. FEC bandwidth is charged to each stream's governor budget, so redundancy and media quality trade off explicitly rather than by accident.

## 7. Latency Budgets (per stage, Constrained tier)

| Stage | Audio | Screen delta | Remote input |
|---|---|---|---|
| Capture/acquire | 60 ms frame | 4–8 ms (damage event) | <1 ms |
| Process/encode | 5 ms | 8–15 ms (classify+code) | <1 ms |
| Frame+pace+send | ≤5 ms | ≤5 ms | immediate lane |
| Network OWD | path | path | path |
| Jitter buffer / reorder | adaptive 10–40 ms | 0 (paint-as-arrives) | 0 |
| Decode+present | 3 ms | 3–8 ms | inject <1 ms |

Design consequence: on a 150 ms-RTT path, input-to-photon ≈ RTT + ~35 ms — the architecture adds almost nothing to physics. Recovery actions (FEC decode, layer drop, tile resend) are all sub-RTT or single-RTT by construction; nothing anywhere waits for a keyframe.

## 8. Algorithm Specification — Audio Subsystem

### 8.1 Per-tier codec configuration

| Tier | Codec / mode | Frame | Net rate | Redundancy |
|---|---|---|---|---|
| Survival (NPU/CPU-ok) | Neural vocoder (SoundStream-lineage, RVQ latents @ 50 Hz) | 20–40 ms | 3.2–6 kbps | packet-pair repetition + PLC |
| Survival (fallback) | Opus SILK-WB | 60 ms | 9–12 kbps | LBRR + **DRED** + neural PLC |
| Constrained | Opus SILK/hybrid WB | 40 ms | 16 kbps | LBRR + DRED (loss-adaptive depth) |
| Comfortable | Opus hybrid SWB | 20 ms | 24 kbps | LBRR; DRED on loss>1% |
| Full | Opus CELT FB (stereo opt.) | 20 ms | 32–48 kbps | LBRR |

DTX runs at every tier (silence ≈ 0 kbps; comfort-noise updates ~400 ms). DRED depth (how much history is redundantly carried, up to ~1 s) is driven by the Gilbert–Elliott burst estimate, so redundancy tracks the channel instead of being a fixed tax. Decoder-side, Opus 1.5's learned enhancement (LACE/NoLACE-class) is enabled when CPU allows — it measurably improves low-rate SILK with zero bitstream cost, i.e., free quality on exactly the tiers we care about.

### 8.2 Adaptive playout

NetEQ-style jitter buffer: maintain an inter-arrival delay histogram; target delay = P95 + half frame; converge via time-scale modification (WSOLA accelerate/decelerate ≤ 15% — inaudible on speech) rather than skips/gaps. Loss concealment order: FEC decode → DRED reconstruction → neural PLC → energy-faded comfort noise. Acceptance test: 5% GE-bursty loss at 300 ms RTT must produce zero audible gaps ≤ 1 s burst length.

## 9. Algorithm Specification — Camera Video

### 9.1 Gear A: neural talking-head codec (10–30 kbps)

Research lineage: one-shot reenactment (face-vid2vid / LivePortrait class; productized precedent: NVIDIA Maxine). Pipeline:

*Sender:* face detect/track → on (re)acquire, transmit **reference frame** (AV1 intra, 256–384 px face crop, ~15–30 kB, channel 6 reliable) → per frame extract `K ≈ 20` implicit 3D keypoints + 6-DoF head pose + expression latents → quantize (10-bit coords), delta-code against previous frame, entropy-code with an adaptive arithmetic coder over learned priors. Budget math: 20 kp × 3 × 10 b = 600 b/frame raw → ~200–350 b after delta+entropy → **5–9 kbps at 25 fps**, plus amortized reference refresh.

*Receiver:* appearance encoder (run once on reference) → warp-field generator conditioned on transmitted motion → synthesis network → 256–384 px output, upsampled for display. Runs on NPU/GPU via `nn/`; capability probe gates the gear.

*Integrity guardrails (hard requirements):* fallback detector runs on the **sender** — keypoint-tracking confidence, occlusion/hand classifier, second-face detector, non-face-pixel ratio; any trip forces Gear B within 200 ms (encoder pre-warmed, intra-refresh start, no keyframe burst). The UI badges the stream "AI-reconstructed" whenever Gear A is live. The codec must never invent what it cannot see; in a trust-critical product this is as much a spec as the bitrate.

### 9.2 Gear B: AV1 low-rate (60–300 kbps)

SVT-AV1, preset selected 10–12 by live CPU/thermal telemetry. Mandatory configuration, not defaults: temporal SVC `L1T2/L1T3` (congestion response = drop T-layer, decoder-transparent); **periodic intra refresh** (column sweep, ~2 s cycle) instead of key frames — a keyframe at 200 kbps is a 1–2 s uplink stall, so key frames are banned after stream start; face-ROI ΔQP −4…−8 from the tracker; temporal denoise pre-filter (sensor noise is entropy — a denoised 100 kbps stream beats a noisy 150 kbps one on VMAF and on eyes); `tune=psychovisual`, screen-content tools off. Resolution/fps ladder: 640×360@15 → 640×360@24 → 848×480@24, chosen by governor budget with encoder feedback (if achieved-QP > ceiling for 2 s, step the ladder down before starving FEC).

Rate control is one-frame-lookahead CBR with a hard per-frame byte cap = `budget/fps × 1.5`; overshoot triggers immediate re-quantization of the frame rather than queue growth — the pacer must never inherit encoder bursts.

### 9.3 Gear C (≥300 kbps)

480p→720p ladder; hardware AV1 (NVENC/QSV) when present, else SVT-AV1; OpenH264 as legacy-CPU fallback. Nothing exotic — the interesting engineering is below 300 kbps.

*(Watch item, deliberately out of v1 critical path: full neural video codecs of the DCVC-FM class now beat VVC/H.266 in lab R-D but are not realtime-on-consumer-CPU; the architecture reserves a Gear D slot behind the same `set_gear` interface for when inference cost crosses the line.)*

## 10. Algorithm Specification — Screen/Remote-Desktop Codec

The screen path is LowBand's largest single advantage over video-codec screen sharing. Formal pipeline, per captured damage event:

```
damage rects ─► rect merge ─► scroll/pan detect ─► tile split (32×32)
   ─► per-tile classifier ─► {TEXT/UI, FLAT, PICTURE, VIDEO}
   ─► per-class encoder ─► priority scheduler ─► channels 4 (rt) / 6 (rel)
```

**10.1 Damage + merge.** OS dirty rects (DXGI / ScreenCaptureKit / PipeWire) or xxHash3 per-tile diff fallback; merge rects when merged-area/sum-area < 1.3. Static screen ⇒ 0 B + 1 Hz heartbeat.

**10.2 Scroll/pan detection.** On large damage: phase correlation over ⅛-scale luma of prev/cur windows; a dominant peak (>0.6 energy) yields motion vector `v`; verify with sparse SAD; emit `BLIT{rect, v}` (16 B) + encode only the exposed strip. Converts scrolling code — the modal action of a support session — from hundreds of kbps to tens.

**10.3 Tile classifier.** Features per 32×32 tile: distinct-color count, gradient-magnitude histogram (2 bins), edge density, temporal change frequency over a 2 s window. Depth-4 decision tree, <1 µs/tile. `VIDEO` = sustained change frequency > 10 Hz over a coherent region ⇒ hand the region to a confined Gear-B AV1 sub-stream at its own fps; the rest of the desktop stays lossless.

**10.4 TEXT/UI + FLAT coder (the crown jewel's crown).** Palette extraction (≤16 colors, else escape tile to PICTURE); index map coded with left/above context modeling into an adaptive range coder — architecturally equivalent to AV1's screen-content palette + intra-block-copy toolset, which is precisely the tool class H.264-era screen shares lack. Always **4:4:4**: chroma subsampling is what makes red-on-blue text bleed on incumbent products. Engineering fallback for v1: serialized index maps through zstd-with-dictionary (≥80% of the win, one afternoon of code); the context-coded path replaces it in v1.1. PICTURE tiles: AV1 intra at tier-appropriate quality.

**10.5 Progressive build-to-lossless.** Per-tile state machine `DIRTY → COARSE (≤50 ms, cheap pass) → LOSSLESS (idle-budget refinement)`; scheduler is a priority queue keyed (saliency: cursor-proximity + recency, age, class), funded by a governor token bucket. Perceptual contract: text legible in the first pass, pixel-exact ≈ 1 s later at 3G rates. "Instant, then crisp" subjectively dominates "delayed and blurry" at equal average bitrate.

**10.6 Cursor.** Never in pixels: 60 Hz position deltas on channel 2 (~0.3 kbps), shapes cached by hash on channel 6. The remote cursor stays fluid even when tiles are late — the top contributor to *perceived* responsiveness, at near-zero cost.

**Reference rates (acceptance targets):** static desktop ≈ 0; typing 5–20 kbps; code/log scrolling 30–80 kbps; against generic H.264 4:2:0 fixed-fps screen share on the same content: **3–10× less bitrate with strictly better text legibility**, verified by the OCR harness (§15).

## 11. Algorithm Specification — File Transfer

FastCDC content-defined chunking (8–64 kB target), BLAKE3 chunk IDs, per-peer persistent dedup index (re-sending last week's installer or a slightly changed log bundle moves metadata + deltas only), zstd per chunk (level 3 foreground / 19 background) with a pre-trained dictionary for IT payload classes (logs, registry exports, configs — dictionaries roughly double small-file compression). Transport: channel 7 under RaptorQ. Scheduling: strictly inside governor-granted headroom; one rule is absolute — bulk may never add a millisecond of queuing ahead of voice or input.

## 12. Governor — Tier State Machine and Allocation

Inputs at 10 Hz: BWE, RTT/jitter, GE loss params, CPU/thermal, battery, per-encoder achieved-rate feedback. Output: tier ∈ {Survival, Constrained, Comfortable, Full} + per-stream budgets + gear selections. Transitions: downgrade on `BWE < 0.8 × tier_floor` for 1 control interval (acts in <200 ms); upgrade on `BWE > 1.3 × tier_ceiling` sustained 5 s *and* probe-validated. Allocation is strict-priority with floors: audio (floor 6 kbps) → input/cursor (floor 3 kbps) → screen COARSE lane → camera → screen refinement → xfer → probes. Every stream is contractually droppable/layerable mid-flight (T-layers, refinement passes, probe padding), which is why no transition anywhere requires a keyframe or a renegotiation round trip.

Worked contract (from the design doc, now testable): 400 kbps → 64 kbps collapse ⇒ within one control interval camera Gear B→A/off, refinements suspend (COARSE lane continues), audio 24→12 kbps with DRED deepened, xfer frozen. Assertions in CI: zero audio gap, cursor cadence unbroken, first-pass screen latency < 200 ms throughout the transition.

## 13. Security Architecture

Handshake and transport crypto per §6.1–6.2 (Noise-IK, X25519, ChaCha20-Poly1305, scheduled rekey); E2EE holds across TURN, which forwards ciphertext only. Above transport sits the **consent layer**: remote capabilities (`view`, `control`, `file`, `clipboard`) are separate signed capability tokens minted by the assisted peer's UI on explicit grant, checked by `control/` on every injected event, revocable instantly, and expiring with the session. The assisted machine shows a persistent indicator while any capability is live; both sides have a panic key that severs injection in <50 ms (transport stays up so the humans can keep talking). Sessions emit a signed local audit log (peer identity key, capabilities, timestamps). Neural reconstruction (Gear A audio/video) is always UI-labeled — a trust product does not silently synthesize media. `lowbandd` runs least-privilege: capture and injection rights are brokered per-OS (UAC/TCC/portal) and dropped when consent lapses.

## 14. SOTA Positioning — Layer-by-Layer vs. a Zoom-Class Baseline

Claims are scoped to published baselines (Zoom's stated bandwidth guidance; H.264/SVC-era coding with partial AV1 adoption; server-routed media) and to the measured literature behind each component. The competitive thesis is *compounding*, not a single silver bullet:

| Layer | Zoom-class baseline | LowBand architecture | Expected edge |
|---|---|---|---|
| Topology (1:1) | Server-routed media (MMR/SFU) | Direct P2P, E2EE; relay = blind forwarder | −1 hop latency; no server-side floor on adaptation |
| Congestion control | Proprietary; behaves loss-reactive under bufferbloat | Delay-gradient (GCC-lineage) + GE-aware FEC + cellular mode | Stable <50 ms self-queue on ADSL; no rate crashes on 3G jitter |
| Audio | ~60–100 kbps typical; concealment-based loss handling | Opus 1.5 @ 8–24 kbps with DRED + neural PLC; 3.2–6 kbps neural gear | 4–10× lower rate; bursts *reconstructed* ≤ ~1 s |
| Camera video | H.264/SVC lineage, ~600 kbps recommended floor | AV1 SVC (~50% vs H.264 at parity) + neural head gear @ 10–30 kbps | usable video at ¼–1/20 the rate |
| Screen share | Fixed-fps video codec, 4:2:0, scrolls re-encoded | Damage semantics + BLIT + palette/IBC 4:4:4 + build-to-lossless | 3–10× lower rate *and* sharper text; 0 kbps when static |
| Wire overhead | Standard RTP stack | Aggregated compact framing, 60 ms low-tier audio | ~10% vs 45–60% overhead at 12 kbps |
| Files | Generic transfer | CDC dedup + dictionary zstd + fountain codes | ≫ on repeated IT payloads; no ARQ stalls at high RTT |

Honesty clauses carried into engineering: neural gears require NPU/GPU or CPU headroom and *degrade to conventional gears*, never to silence; AV1 encode on very old CPUs falls back to H.264 and forfeits that layer's edge; and every row above is a **CI-gated benchmark**, not a slide (§15).

## 15. Verification Architecture

The claim "beats the incumbent at low bandwidth" is a test suite. `obs/` + CI run every build over recorded real-network traces (3G drive traces, ADSL2 with induced cross-traffic; netem/mahimahi replay) and gate on: **VMAF** (camera), **ViSQOL/POLQA-class** (audio), mouth-to-ear and input-to-photon latency distributions, post-FEC residual loss, and — for the screen codec — the **OCR-legibility metric**: OCR over decoded frames scored for character accuracy against source. PSNR forgives illegible text; OCR does not, and a technician reading a stack trace is the product. Reference clients (stock WebRTC/H.264 profile emulating incumbent behavior) run the same traces in the same harness so the comparison rows in §14 regenerate from data on every release.

## 16. Technology Stack and Repository Layout

Rust core (`lowbandd`); C libraries via FFI: libopus ≥1.5, SVT-AV1 + dav1d, zstd, BLAKE3, RaptorQ lib; ONNX Runtime for `nn/` with CoreML/NNAPI/DirectML/CPU execution providers; capture/injection: DXGI + SendInput (Win), ScreenCaptureKit + CGEvent (macOS), PipeWire + libei/XTest (Linux); UI shells native per platform over IPC. Infra: stateless WS signaling, coturn. Repo: `/core` (daemon crates as in §4), `/models` (versioned ONNX + eval cards), `/proto` (LBTP + IPC schemas, golden vectors), `/bench` (trace corpus + harness of §15), `/shells`.

```
Milestones: M1 lbtp+audio+input (a shippable voice+control assist tool)
            M2 screen codec        M3 AV1 camera gears B/C
            M4 neural gears A + vocoder      M5 mesh ≤4 / E2EE forwarder
```

---

## Appendix A — LBTP Golden Numbers

MTU 1200 B · pacer burst ≤5 ms · CC report interval 50 ms · governor 10 Hz · downgrade <200 ms, upgrade ≥5 s probed · audio floor 6 kbps · cursor 60 Hz · tile 32×32 · COARSE pass ≤50 ms · FEC residual target 10⁻³ · rekey 2³⁰ dgrams/15 min · keyframes after stream start: forbidden.

## Appendix B — Group Calls Beyond Mesh (deferred)

≤4 peers: full mesh with per-link governors (uplink budget divides; simulcast of Gear-A motion codes is nearly free since keypoints are receiver-agnostic). >4: an optional forwarding node relays ciphertext per-layer (SVC-aware dropping on encrypted layer IDs in the envelope) — no decryption, no transcoding, E2EE preserved. Out of v1 scope; the envelope reserves the layer-ID bits now so the forwarder needs no protocol change later.
