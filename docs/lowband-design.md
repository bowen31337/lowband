# LowBand — Technical Design
## A P2P Remote-IT-Assist and Conferencing System for Hostile Networks (3G / ADSL2)

**Version:** 1.0 draft · **Scope:** full-stack design — transport, compression, adaptation, security

---

## 1. Problem Statement and Operating Envelopes

The networks we must survive are defined by their *uplink*, not their downlink. Real-world 3G/HSPA delivers roughly 0.3–1.5 Mbps up with 80–400 ms RTT and 1–5% bursty packet loss. ADSL2+ advertises up to 24 Mbps down but typically carries only 0.4–1 Mbps up, with a deep bufferbloat-prone modem queue that turns sustained load into multi-second latency. Since conferencing and remote assist are bidirectional, the uplink of the *weaker* peer is the binding constraint for the entire session. Every design decision below flows from that fact.

We define four discrete operating tiers. The governor (§8) moves the session between them with hysteresis, and every subsystem is required to have a defined behavior at each tier.

| Tier | Total budget (per direction) | Experience contract |
|---|---|---|
| **Survival** | 48–64 kbps | Clear voice, legible screen text, responsive remote control. Camera off or AI-reconstructed head. |
| **Constrained** | 128–200 kbps | Voice + 360p/15 camera *or* crisp full screen share. This is the "typical 3G" tier. |
| **Comfortable** | 300–500 kbps | Voice + 480p/24 camera + screen share simultaneously. Typical ADSL2 uplink. |
| **Full** | ≥ 1 Mbps | 720p/30, high-fidelity audio, background bulk transfer. |

The product contract: **functional at 64 kbps, pleasant at 150 kbps.** For calibration, mainstream conferencing stacks (Zoom included) publish recommendations on the order of 600 kbps–1.8 Mbps for 1:1 video and degrade sharply below ~150 kbps. Our target is to deliver a usable audio-video-control session at roughly a quarter of that, and to make the remote-assist workload (screen + control) dramatically cheaper than any video-codec-based screen share.

A second, equally binding constraint is latency. Remote assistance dies above ~150 ms input-to-photon on top of network RTT, so every stage of the pipeline carries an explicit latency budget: capture-to-wire under 30 ms for screen deltas, one-RTT loss recovery, and no keyframe stalls, ever.

---

## 2. Architecture Overview

The system is peer-to-peer for all media and control traffic. Infrastructure exists only to introduce peers and, in the worst NAT cases, to blindly relay ciphertext.

```
   Peer A (technician)                              Peer B (user)
 ┌──────────────────────┐                        ┌──────────────────────┐
 │  UI / consent layer  │                        │  UI / consent layer  │
 │──────────────────────│                        │──────────────────────│
 │  Governor (§8)       │◄── telemetry loop ────►│  Governor            │
 │──────────────────────│                        │──────────────────────│
 │  Media engine        │                        │  Media engine        │
 │   audio (§4)         │                        │   audio              │
 │   camera (§5)        │                        │   camera             │
 │   screen (§6)        │                        │   screen             │
 │  Control plane (§7)  │                        │  Control plane       │
 │──────────────────────│                        │──────────────────────│
 │  Session crypto (§10)│                        │  Session crypto      │
 │  LBTP transport (§3) │◄══ UDP, P2P, E2EE ═══► │  LBTP transport      │
 └──────────────────────┘                        └──────────────────────┘
            │                                               │
            └────────► Signaling (WebSocket, stateless) ◄───┘
                       STUN for reflexive addresses
                       TURN relay only as last resort (sees ciphertext only)
```

The signaling server performs rendezvous, exchanges session descriptions and ICE-style candidates, and is then out of the loop. There is no media server, no SFU, no transcoding hop — for 1:1 remote assist this removes an entire encode/decode cycle of latency and cost, and it is the reason we can spend our complexity budget on the codecs instead.

---

## 3. Transport Layer (LBTP)

We run a purpose-built protocol over UDP — call it LBTP — borrowing the good parts of QUIC and RTP while discarding their generality. Because we control both endpoints, we do not pay for interoperability we don't need.

**Connectivity.** Standard ICE methodology: host, server-reflexive (STUN), and relayed (TURN) candidates, aggressive UDP hole punching, and keepalives every 15–25 s tuned to survive common NAT binding timeouts. Field data consistently shows ~85–95% of pairs achieve direct UDP; the remainder relay through TURN carrying end-to-end ciphertext. A TCP/TLS-443 fallback exists purely for pathological corporate networks, with the UI honestly flagging the latency penalty.

**Datagram discipline.** Maximum datagram size is 1200 bytes to avoid fragmentation across virtually all paths, with upward path-MTU probing when conditions allow. All packets are paced — never bursted — because a burst into a 512 kbps ADSL uplink queue *is* self-inflicted jitter.

**Three delivery classes**, multiplexed on one 5-tuple:

1. *Unreliable-realtime* — media frames. Late data is worthless; no retransmission, protected instead by FEC and codec-level redundancy.
2. *Reliable-unordered* — screen refinement passes, file chunks. Selective-ACK ARQ; order irrelevant, completeness eventual.
3. *Reliable-ordered* — control: input events, consent messages, codec negotiation. A tiny SACK-based stream with the highest scheduling priority in the pacer. Input events must beat media in every queue, always.

**Loss repair strategy.** Audio protects itself in-codec (§4). Video and screen media use adaptive block FEC — Reed–Solomon over short blocks for realtime frames, RaptorQ-style fountain coding (RFC 6330) for bulk — with the redundancy ratio driven by measured loss so that residual post-FEC loss stays under 0.1% without waiting a retransmission round-trip. On a 300 ms-RTT 3G path, ARQ is a last resort for realtime media; FEC ratio adaptation is the first line.

**Congestion control.** This is where low-bandwidth products live or die. The primary signal is the *delay gradient*: a trendline/Kalman filter over one-way-delay variation in the spirit of Google Congestion Control, which detects the ADSL modem queue filling hundreds of milliseconds before loss appears. A loss-based controller acts only as a backstop. On cellular paths we add SCReAM-inspired (RFC 8298) handling so that the scheduler-induced delay spikes characteristic of 3G/LTE radio links are not misread as congestion and do not trigger needless rate crashes. The controller targets under ~50 ms of self-induced queuing. Available bandwidth above the current operating point is discovered with short paced probe trains of padding, so tier upgrades are based on evidence rather than optimism.

**One brain, not five.** The congestion controller produces a single send-rate estimate; the governor (§8) divides it among streams. Individual encoders never fight each other for bandwidth.

---

## 4. Audio Pipeline — Degrades Last, Never Gaps

Voice is the highest-priority stream: an IT session can survive frozen video, but not garbled instructions. The pipeline is capture at 48 kHz → echo cancellation (AEC3-class adaptive filter) → neural noise suppression (RNNoise-class, ~0.1% CPU) → AGC → voice-activity detection → encode.

**Core codec: Opus 1.5+, SILK/hybrid mode.** Opus remains unbeaten as an engineering package for this problem, and its 1.5 release added exactly the machine-learning tools a lossy 3G link needs:

- Normal operation at 12–24 kbps wideband; under pressure down to 8–10 kbps. At the bottom tier we switch to 60 ms frames, cutting packet count — and therefore header overhead (§9) — by 3× versus 20 ms framing.
- **DTX** (discontinuous transmission): near-zero bitrate during silence, which in a typical support call is more than half the session.
- Loss armor, layered: in-band LBRR FEC for isolated losses; **DRED (Deep REDundancy)** — Opus 1.5's neural in-band redundancy that carries up to ~1 second of ultra-compressed speech history at a modest bitrate overhead, so that even multi-hundred-millisecond 3G loss bursts are *reconstructed* rather than concealed; and the neural PLC at the decoder for whatever still slips through. The audible result on a 5% bursty-loss link is occasional slight dullness instead of dropouts.

**Survival gear: neural vocoder at 3.2–6 kbps.** When the governor declares survival tier and the device has an NPU/GPU (or CPU headroom), we switch to a Lyra-v2-class codec (SoundStream lineage): a learned encoder produces quantized latents, a learned vocoder reconstructs speech. Subjective quality at ~3 kbps rivals classical codecs at roughly three times the rate for speech content. This is what keeps a call intelligible on a link that can barely carry anything — 6 kbps of voice inside a 48 kbps budget leaves 40 kbps for the screen.

**Playout.** An adaptive jitter buffer (NetEQ-style time-stretch/compress) tracks the delay distribution and keeps mouth-to-ear latency at the minimum the network allows — target ≤150 ms on ADSL, ≤250 ms on 3G where RTT dominates.

---

## 5. Camera Video — Three Gears

Camera video is the most compressible stream because for conferencing it is overwhelmingly one thing: a talking head. We exploit that with a three-gear codec strategy, switched seamlessly by the governor.

**Gear A — Neural talking-head codec (10–30 kbps, survival tier).** Based on one-shot face reenactment (the face-vid2vid / LivePortrait research lineage, productized in systems like NVIDIA Maxine). The sender transmits one high-quality reference keyframe (AV1 intra, ~15–30 kB, amortized over minutes and refreshed on appearance change), then per frame only a compact motion description: ~10–20 implicit 3D keypoints plus head pose and expression latents, quantized and entropy-coded to roughly 5–15 kbps at 20–25 fps. The receiver's warping/synthesis network reconstructs a 256–384 px head that tracks the speaker's actual motion. This is an order of magnitude below what any waveform codec can do for the same perceptual result — the research consistently shows ~10× bandwidth reduction versus H.264 at equivalent perceived quality for this content class.

Two hard guardrails, because remote IT assist runs on trust: scene-change, hand, occlusion, and second-face detectors trigger *instant* fallback to Gear B (the codec must never hallucinate content it cannot represent), and the UI permanently labels the stream "AI-reconstructed" while in this mode.

**Gear B — AV1 low-rate (60–300 kbps).** SVT-AV1 at realtime presets (≈10–12 depending on CPU telemetry), with the specific low-bitrate discipline that separates a good encoder integration from a poor one: temporal SVC layering (1–3 layers) so congestion response is "drop a layer" rather than "request a keyframe"; periodic intra-refresh columns instead of full keyframes, because a keyframe burst on a 384 kbps uplink is a two-second stall; face-ROI adaptive quantization spending 30–40% extra bits on the face tiles; and a temporal denoising pre-filter, since sensor noise is pure bitrate poison at these rates — a denoised 100 kbps stream routinely beats a noisy 150 kbps one. Optional background blur is offered to users as an aesthetic feature; it is really a bitrate feature.

**Gear C — Standard ladder (≥300 kbps).** 480p → 720p AV1, using hardware encoders (NVENC/QSV AV1) when present, SVT-AV1 otherwise. On legacy hardware that cannot sustain AV1 encode, OpenH264 remains as a compatibility fallback, accepting reduced compression.

**Why AV1 as the workhorse:** roughly 50% bitrate savings over H.264 and 25–30% over HEVC/VP9 at equal quality, royalty-free, and — critically — the dav1d decoder is fast enough that *decode* is essentially never the bottleneck even on old laptops and phones. Encode cost is the real constraint, which is why encoder preset selection is governed by live CPU/thermal telemetry rather than fixed at session start.

---

## 6. Screen Share and Remote Desktop — The Crown Jewel

The core insight: **a desktop is not a video.** It is a composited document that changes rarely, structurally, and often by pure translation. Feeding it to a video codec at a fixed framerate — which is what most conferencing products do — wastes bandwidth on stillness and destroys text with chroma subsampling. Our screen path is a content-aware pipeline in six stages.

**Stage 1 — Damage acquisition.** We take dirty rectangles from the OS where available (DXGI Desktop Duplication on Windows, ScreenCaptureKit on macOS, PipeWire/wlroots damage events on Linux), with a per-tile xxHash diff as the universal fallback. A static screen costs *zero bytes* on the wire beyond a 1 Hz heartbeat. Since IT-assist sessions spend most of their time watching an unchanging screen while someone talks, this alone is a massive structural win over fixed-framerate encoding.

**Stage 2 — Motion semantics before pixels.** A phase-correlation detector identifies scrolls, pans, and window drags. A scrolled document region becomes a ~16-byte *blit command* (rectangle + motion vector) plus a thin strip of newly exposed pixels — instead of a full-region re-encode. Scrolling through code, the single most common action in a remote debug session, drops from hundreds of kbps to tens.

**Stage 3 — Per-tile content classification.** Each 32×32 damaged tile is classified in well under a microsecond by a tiny decision tree over color-count and gradient statistics into TEXT/UI, FLAT, PICTURE, or VIDEO.

**Stage 4 — Per-class coding.**
- *TEXT/UI and FLAT tiles* are palette-indexed (UI content is typically ≤16 distinct colors per tile), spatially predicted, and entropy-coded losslessly (zstd-class backend, or equivalently AV1's screen-content tools — palette mode + intra block copy — in lossless configuration). Always in full 4:4:4 chroma: subsampling is precisely what makes red-on-blue text illegible on conventional screen shares. Lossless text costs surprisingly little when it is palette-coded and only sent on damage.
- *PICTURE tiles* (photos, gradients-rich content) go through AV1 lossy at the quality the current tier affords.
- *VIDEO tiles* — a region whose change-rate signature says "a video is playing here" — are confined into their own small AV1 stream at an appropriate framerate, so a YouTube window doesn't drag the whole desktop into video-codec economics. The surrounding screen stays lossless.

**Stage 5 — Progressive build-to-lossless.** On damage, ship a fast low-cost pass within ~50 ms so the viewer sees the change immediately, then spend idle bandwidth on residual refinements until the region is pixel-perfect. Text is legible in the first pass and exact within about a second at 3G rates. The technician experience is "instant and then crisp," which subjectively beats "delayed and blurry" at identical average bitrate.

**Stage 6 — Cursor as metadata, never as pixels.** Cursor position rides the reliable-ordered channel as 60 Hz deltas (~0.3 kbps); cursor shapes are cached by hash. The remote cursor therefore stays perfectly fluid even when frame updates are late — the single largest contributor to *perceived* responsiveness in remote control, and it costs almost nothing.

**Resulting rates for typical IT-assist content:** static desktop ≈ 0 kbps; live typing 5–20 kbps; scrolling source code or logs 30–80 kbps; full-screen video playback degenerates, correctly, to camera-video economics. Against a generic H.264-based screen share — which spends lossy 4:2:0 bits on stillness and re-encodes every scroll — the combination of damage semantics, scroll blitting, and screen-content coding tools is worth a conservative 3–10× on real support workloads, with *better* text legibility, not worse.

---

## 7. Control Plane — Input, Files, Clipboard

**Input events.** Keyboard and pointer events use a compact binary schema with varint delta encoding; mouse moves are coalesced to the remote display's refresh cadence. Worst-case cost is 0.5–2 kbps, carried on the reliable-ordered channel at top scheduling priority. Injection on the assisted machine is gated by the consent system (§10).

**File transfer** — the daily bread of IT support — is built for redundancy exploitation rather than raw compression alone. Files are cut by FastCDC content-defined chunking (8–64 kB average), chunks are identified by BLAKE3 hashes, and a per-peer persistent chunk cache means that re-sending an installer, driver package, or log bundle the technician already pushed last week (or that differs only slightly from one they did) transfers metadata plus the delta, not the file. Chunks compress with zstd — a fast level when the user is waiting, level 19 when running in the background — using a pre-trained dictionary for common IT payloads (text logs, registry exports, configs), where dictionaries typically double compression on small files. Crucially, bulk transfer runs strictly inside the headroom the governor grants it: it must never add queuing delay to voice or input, even by one packet.

**Clipboard sync, chat, and telemetry** ride the same reliable machinery with zstd framing; their cost is negligible.

---

## 8. The Governor — One Brain Allocates Every Bit

A single adaptation loop runs at ~10 Hz on each peer, consuming the transport's bandwidth estimate, RTT/jitter/loss statistics, CPU and thermal telemetry, and battery state; it outputs per-stream budgets and codec gear selections. Both governors exchange summaries so the session converges on the weaker peer's constraints without oscillation.

Allocation follows a strict priority order: audio first, then input/control, then screen text deltas, then camera video, then screen refinement passes, then bulk transfer and probing. Transitions between tiers are discrete and hysteretic — an upgrade requires several seconds of proven headroom, a downgrade happens within one RTT of detected congestion.

A worked example: the session is at Comfortable tier (≈400 kbps) on ADSL2 when a household upload saturates the link and the estimate collapses to 64 kbps. Within one control interval: camera drops from Gear B to Gear A (or off, per user preference), the screen stream suspends refinement passes but keeps shipping legible first-pass text deltas, audio steps from 24 to 12 kbps with DRED engaged, bulk transfer freezes. Voice never gaps; the cursor never stutters; the technician can keep working. That is the entire point of the design.

Every stream is engineered to be **droppable or layerable mid-flight** — temporal SVC layers, refinement passes, probe padding — so congestion response never requires a keyframe and never stalls the pipeline.

---

## 9. Wire Efficiency — Where Low-Bitrate Products Quietly Die

At 12 kbps of audio in 20 ms frames, the payload is 30 bytes per packet while IPv4 + UDP + a standard RTP header cost ~40 bytes — the plumbing outweighs the water. Naive stacks lose 45–60% of a survival-tier budget to overhead. Countermeasures:

The bottom tiers use 60 ms audio framing (3× fewer packets), and the sender coalesces concurrent small payloads — an audio frame, a cursor delta, an input event, an ACK — into a single datagram per pacing tick. Because both endpoints are ours, the wire header is a custom 3–5 byte compact frame (type/flags, sequence, optional timestamp delta) rather than a full RTP header. Encryption is ChaCha20-Poly1305, whose 16-byte AEAD tag is paid once per *datagram*, not per frame — another argument for aggregation. Net effect: protocol overhead at survival tier lands around 10–12% instead of half the budget, which is effectively a free 20 kbps we hand back to the codecs.

---

## 10. Security and Consent — Non-Negotiable for Remote Assist

Remote-control software is a high-value attack target and a social-engineering vector, so the security model is part of the compression story's credibility. Key agreement uses a Noise-IK (or DTLS 1.3) handshake bound to an out-of-band session code, X25519 key exchange, ChaCha20-Poly1305 for traffic. Encryption is end-to-end: a TURN relay forwards ciphertext it cannot read. An optional short-authentication-string flow lets the two humans verify the channel verbally — natural in a support call.

Control is capability-scoped and explicitly granted: *view*, *control*, *file transfer*, and *clipboard* are separate consents, revocable instantly, with a persistent on-screen indicator on the assisted machine and a panic key on both sides that severs input injection immediately. Sessions produce a signed local audit log (who connected, which capabilities, when). And as noted in §5, any neurally reconstructed media is labeled as such in the UI — a system built for trust does not silently show synthesized video.

---

## 11. Performance Targets vs. the Incumbent Baseline

These are design targets grounded in the published performance of the constituent techniques (AV1 vs. H.264 coding gains, Opus 1.5 loss-resilience results, neural-codec literature), to be validated by the harness in §12 — not marketing numbers.

| Scenario | Typical mainstream stack (Zoom-class) | LowBand target |
|---|---|---|
| 1:1 voice on poor 3G | ~60–100 kbps, artifacts under burst loss | 8–16 kbps Opus+DRED; 3–6 kbps neural gear; burst loss ≤ ~1 s inaudible |
| 1:1 camera video, small window | ~600 kbps recommended floor | 100–150 kbps (AV1 Gear B); 25–40 kbps (neural head) |
| Screen share, scrolling code/logs | 150–300 kbps, soft text (4:2:0) | 30–80 kbps, pixel-crisp 4:4:4 text, build-to-lossless |
| Watching a static screen | continuous stream regardless | ≈ 0 kbps idle |
| Remote control feel at 300 ms RTT | cursor tied to video frames | cursor fluid at 60 Hz metadata; input priority-queued |

The honest caveats: the neural gears require an NPU/GPU or spare CPU cycles and fall back gracefully when absent; AV1 encode on decade-old CPUs forces the H.264 fallback at reduced savings; and "beats X" claims only mean something under measurement, which is why the validation harness is a first-class deliverable.

---

## 12. Implementation Stack and Phasing

**Core:** Rust for transport, pipeline, and governor (memory safety in a network-facing daemon is not optional). **Codec libraries:** libopus 1.5+, SVT-AV1 + dav1d, zstd, BLAKE3. **Neural runtime:** ONNX Runtime with CoreML/NNAPI/DirectML/CPU execution providers, with capability probing at startup deciding which gears exist on this device. **Capture:** DXGI / ScreenCaptureKit / PipeWire. **Infrastructure:** a stateless WebSocket signaling service and coturn for TURN — deliberately boring.

**Validation harness (build first, not last):** network emulation over recorded real-world 3G and ADSL traces (netem/mahimahi), with CI gates on VMAF for video, ViSQOL/POLQA-class scoring for audio, glass-to-glass and input-to-photon latency, and — for the screen codec — an **OCR-legibility metric**: run OCR over decoded screen frames and score character accuracy against the source. If the OCR can't read the text, neither can the technician; this catches exactly the failure mode that pixel metrics like PSNR forgive.

**Phasing:** M1 ships transport + audio + input control — already a useful product (voice + remote control is the irreducible core of IT assist). M2 adds the screen codec, M3 the AV1 camera path, M4 the neural gears, M5 small-group calls (mesh up to ~4 peers; beyond that, an optional E2EE-preserving forwarding node, since pure mesh multiplies the precious uplink).

**Principal risks:** encoder CPU cost versus bitrate savings on weak hardware (mitigated by the gear governor and hardware encoders); neural-reconstruction artifacts eroding user trust (mitigated by conservative fallback triggers and explicit labeling); NAT traversal failures (TURN, then TCP-443 as the loud last resort); and IPR review for the screen-content techniques, where building on royalty-free AV1 tooling is a deliberate advantage.

---

## 13. Summary of the Compression Thesis

Beating incumbent conferencing at low bandwidth is not one magic algorithm; it is refusing to waste bits at every layer simultaneously: send *nothing* for stillness (damage tracking, DTX), send *semantics* instead of pixels where a model exists (scroll blits, cursor metadata, facial keypoints, speech latents), use the strongest royalty-free waveform codecs where pixels must flow (AV1 with screen-content tools, Opus 1.5 with DRED), armor everything against loss without retransmission stalls (FEC, DRED, SVC layers), keep self-inflicted queuing near zero (delay-gradient congestion control, pacing), and stop paying header tax on tiny packets (aggregation, compact framing). Each is worth 1.5–10× on its own axis; compounded, they turn a 64 kbps link from "unusable" into a working support session.
