# LowBand — Product Requirements Document (PRD)

**Product:** LowBand — P2P remote IT assistance + voice/video conferencing for low-bandwidth networks
**Doc type:** PRD · **Status:** Draft for review · **Companions:** Design Document v1.0, Technical Architecture Document
**Priorities:** P0 = launch-blocking · P1 = launch-important · P2 = post-launch fast-follow

---

## 1. Overview

LowBand is a peer-to-peer remote-support and conferencing product engineered for the networks the incumbents gave up on: 3G, ADSL2, congested rural broadband, satellite, and metered mobile data. It combines voice calling, screen sharing, remote control, and file transfer in a single session that remains **fully functional at 64 kbps and pleasant at 150 kbps per direction** — roughly a quarter of the bandwidth floor that mainstream conferencing tools publish for 1:1 video.

The product wedge is remote IT assistance, where the pain is sharpest: the person who most needs help is disproportionately the person on the worst connection, and today's tools (video-codec screen sharing over broadband-era stacks) fail exactly there. Conferencing capability comes along structurally, because the same engine that carries a support session carries a call.

## 2. Problem Statement

A managed-service technician supporting a rural clinic on ADSL2, a field engineer on a 3G dongle, or a family member helping a parent on congested Wi-Fi all hit the same wall: mainstream tools recommend 600 kbps–1.8 Mbps for 1:1 video, treat the screen as a video stream (blurry text, continuous bandwidth burn even when nothing changes), tie the remote cursor to late video frames (control feels drunk at 300 ms RTT), and fall apart under the bursty 1–5% packet loss characteristic of radio links. The result is support sessions conducted by phone call plus screenshots over email — in 2026.

The uplink is the binding constraint (3G: 0.3–1.5 Mbps up; ADSL2: often ≤1 Mbps up), and since support is bidirectional, the *weaker peer's uplink* bounds the whole session. A product that wins here must be engineered for that number, not adapted down to it.

## 3. Goals and Non-Goals

**Goals (v1):** deliver a complete assist session — clear voice, legible screen, responsive control, file push — over a 64 kbps link; make screen text *sharper* than video-codec competitors while using 3–10× less bandwidth; survive 5% bursty loss with no audible voice gaps; establish sessions through NATs without any user networking knowledge; make consent, control scoping, and auditability first-class so the product is deployable by security-conscious IT teams; keep all media end-to-end encrypted with no server-side processing.

**Non-goals (v1):** unattended/always-on access (a different trust model — v2 candidate); session recording; calls larger than 4 participants; webinars and PSTN dial-in; a cloud SFU; a mobile technician app (P2); replacing full-fidelity broadband conferencing where bandwidth is abundant — we win the constrained regime first.

## 4. Target Users

**The technician ("Tan", MSP help-desk / internal IT).** Runs 8–15 sessions a day, many to poorly connected endpoints. Cares about: time-to-fix, crisp terminal/log text, a cursor that obeys, file push that resumes, and an audit trail their compliance team accepts. Success for Tan: "I stopped asking customers to read error messages to me over the phone."

**The assisted user ("Ana", low technical confidence, weak connection).** Needs: join with a short code, one obvious consent screen, a visible "you are being helped" indicator, a panic key that instantly cuts control, and a session that doesn't eat her mobile data cap. Success for Ana: it worked the first time and she never felt out of control.

**The IT manager ("Mo", buys and deploys).** Cares about: E2EE with no media ever touching vendor servers, per-capability consent policy, signed audit logs, silent-deploy MSI/pkg, and predictable data usage on metered links. Success for Mo: passes security review without exceptions.

## 5. Primary Use Cases

**UC-1 Fix-over-3G (core).** Tan sends Ana a 9-digit code; Ana joins on a 3G dongle. Voice connects in seconds; Tan views Ana's screen with pixel-crisp text, requests control, fixes a config, pushes a 40 MB installer that survives an IP change when Ana's dongle re-attaches. Total session data stays within a prepaid data budget.

**UC-2 Guided walkthrough (view-only).** Ana grants *view* but not *control*; Tan talks her through steps, pointing with his own overlay cursor. Voice quality holds even when a household upload saturates the ADSL line mid-call.

**UC-3 Field-to-expert call.** A field engineer on 3G video-calls a senior engineer: survival-tier voice plus AI-reconstructed head video (clearly labeled), switching to screen share of a diagnostics laptop without renegotiating the session.

**UC-4 Compliance-grade assist.** Mo's policy requires scoped consent and logs: the session enforces view/control/file/clipboard as separate grants, and exports a signed audit record of who did what, when.

## 6. Functional Requirements

Verification for every row is the automated harness defined in Architecture §15 plus manual UX review; traceability column points into the companion docs.

| ID | Requirement | Priority | Acceptance criteria (summary) | Trace |
|---|---|---|---|---|
| FR-1 | Join by 9-digit code or link; no account required for the assisted side | P0 | p95 code-to-connected ≤ 5 s incl. NAT traversal; zero networking questions asked of user | Arch §3, §6.1 |
| FR-2 | Full-duplex voice call | P0 | Meets NFR-1/-3 voice bars at every tier; DTX active | Design §4 |
| FR-3 | Screen view (full screen or per-monitor/window selection) | P0 | Text legibility per NFR-4; static screen per NFR-5 | Design §6 |
| FR-4 | Remote keyboard/mouse control | P0 | Input-to-photon ≤ RTT + 60 ms; injection only with live `control` grant | Arch §7, §10.6 |
| FR-5 | Scoped consent: view / control / file / clipboard as separate, revocable grants; persistent on-screen indicator; panic key severs control < 50 ms both sides | P0 | Capability tokens enforced per event; indicator cannot be suppressed | Arch §13 |
| FR-6 | File transfer with resume, dedup, and background priority | P0 | Survives IP migration and app restart; never adds queuing ahead of voice/input | Arch §11 |
| FR-7 | Session survives network change (3G IP re-attach, Wi-Fi↔cellular) | P0 | Media resumes ≤ 1 RTT after new path validated; no re-join | Arch §6.1 |
| FR-8 | Camera video with gear system (AV1; neural head mode where hardware allows) | P1 | Gear transitions seamless; neural mode always UI-labeled ("AI-reconstructed") | Design §5 |
| FR-9 | Clipboard sync (text v1; files P2) under `clipboard` grant | P1 | Round-trip ≤ 1 s at constrained tier | Arch §11 |
| FR-10 | In-session text chat | P1 | Delivered reliably even at survival tier | Arch §6.3 |
| FR-11 | Live quality indicator: current tier, bitrate, RTT, loss — honest, not decorative | P1 | Matches governor state within 1 s | Arch §12 |
| FR-12 | Signed session audit log, exportable (JSON) | P1 | Log covers identity keys, grants, timestamps; tamper-evident | Arch §13 |
| FR-13 | Windows elevation (UAC/secure-desktop) handling for control sessions | P1 | Documented per-platform behavior; no silent privilege escalation | — |
| FR-14 | Group calls up to 4 (mesh) | P2 | Uplink budget divides per governor; no server dependency | Arch App. B |
| FR-15 | Technician annotation/pointer overlay in view-only mode | P2 | Overlay rides cursor channel; ≤ 0.5 kbps | Arch §10.6 |

## 7. Non-Functional Requirements

| ID | Requirement | Priority | Bar |
|---|---|---|---|
| NFR-1 | **Tier contract.** Product remains functional at 48–64 kbps (clear voice, legible screen deltas, responsive control), pleasant at 128–200 kbps, full-featured at ≥ 300 kbps | P0 | CI trace suite: task-completion scenarios pass at 64 kbps emulated 3G |
| NFR-2 | **Latency.** Mouth-to-ear ≤ RTT/2 + 100 ms; input-to-photon ≤ RTT + 60 ms; first legible screen pass ≤ 200 ms + OWD after change | P0 | Latency distributions gated in CI per Arch §7 |
| NFR-3 | **Loss resilience.** At 5% bursty (Gilbert–Elliott) loss, 300 ms RTT: zero audible voice gaps for bursts ≤ 1 s; control channel unaffected; screen converges | P0 | ViSQOL ≥ 3.5 at constrained tier under loss |
| NFR-4 | **Text legibility.** OCR-accuracy metric on decoded screen ≥ 99.5% after refinement, first pass readable; 4:4:4 text always | P0 | OCR harness, Arch §15 |
| NFR-5 | **Idle economy.** Static screen ≈ 0 kbps (heartbeat only); silence ≈ 0 kbps (DTX); typical 30-min assist session ≤ 15 MB total at constrained tier | P0 | Session data meter in bench |
| NFR-6 | **Security.** E2EE always on, no opt-out; TURN relays see ciphertext only; consent enforced per event; neural media always labeled | P0 | Arch §13; external security review before GA |
| NFR-7 | **Platforms.** Windows 10+ (P0), macOS 13+ (P0), Linux X11/Wayland (P1), assisted-side mobile viewer (P2) | mixed | Install-to-first-session ≤ 2 min |
| NFR-8 | **Resource ceilings.** Constrained-tier session ≤ 35% CPU on a 2015-class dual-core laptop; graceful gear degradation on thermal pressure; no NPU required for any P0 feature | P1 | Telemetry on reference hardware matrix |
| NFR-9 | **Privacy & telemetry.** Media content never leaves the peers; QoS telemetry opt-in, aggregate-only | P0 | Data-flow review |
| NFR-10 | **Reliability.** Crash-free session rate ≥ 99.5%; UI-shell crash must not drop the call | P1 | Arch §4 process split |

## 8. Success Metrics (first 6 months post-GA)

Primary: **task-completion rate on constrained networks** — scripted assist tasks over recorded 3G/ADSL2 traces, LowBand ≥ 95% completion, measured side-by-side against a stock WebRTC/H.264 reference client on identical traces (the comparison regenerates from CI, not from marketing). Supporting product KPIs: median time-to-first-legible-screen; session-drop rate on trace suite < 1%; direct-P2P connection rate ≥ 85%; median data consumed per 30-min assist session; technician weekly retention and "sessions per technician per week"; assisted-user one-question CSAT ≥ 4.5/5; percentage of sessions where the panic key or consent revocation was exercised (a *trust health* metric we want visible, not hidden).

## 9. Competitive Positioning (product-level)

Scoped honestly: comparisons are against published bandwidth guidance and observable product behavior of mainstream tools, verified by our own harness rather than asserted.

| Dimension | Zoom-class conferencing / video-codec screen share | LowBand |
|---|---|---|
| Minimum viable session | Degrades sharply below ~150 kbps; recommends ≥ 600 kbps for 1:1 video | Functional at 64 kbps; pleasant at 150 kbps |
| Screen text | Lossy 4:2:0 video — soft text, worse when scrolling | Lossless 4:4:4 text, build-to-lossless; sharper *and* 3–10× cheaper |
| Idle screen cost | Continuous stream regardless of change | ≈ 0 kbps when nothing changes |
| Remote control feel | Cursor tied to video frames; degrades with RTT | Cursor as 60 Hz metadata; input priority-queued |
| Burst loss on radio links | Concealment artifacts | Reconstruction (DRED) — bursts ≤ 1 s inaudible |
| Data cap friendliness | ~100+ MB per 30-min video session at recommended rates | ≤ 15 MB typical assist session (NFR-5) |
| Trust model | Cloud-routed media, vendor-side processing | P2P E2EE, scoped consent, signed audit, labeled AI media |

Positioning sentence for all product surfaces: *"Remote support that works on the connections your users actually have."*

## 10. Release Plan

**Alpha (M1).** Voice + remote control + consent + roaming, Windows-first. Already a usable assist tool — voice plus a responsive cursor is the irreducible core. Audience: 5–10 design-partner MSPs. Exit: UC-1 minus screen/file completes on live 3G.

**Beta (M2 + xfer).** Screen codec (view, legibility bar NFR-4), file transfer, macOS. Exit: full UC-1 on trace suite at 64 kbps; OCR gate green.

**GA v1.0 (M3).** AV1 camera gears B/C, Linux, audit export, quality indicator, admin deployment (MSI/pkg), external security review. Exit: all P0s, NFR suite gated in CI.

**v1.1 (M4).** Neural gears (survival-tier voice codec, AI head video with labeling), assisted-side mobile viewer (P2 pull-in if metrics demand). **v1.2 (M5).** Mesh group calls ≤ 4; clipboard files.

## 11. Dependencies and Assumptions

Depends on: royalty-free codec stack (Opus ≥ 1.5, SVT-AV1/dav1d — licensing reviewed), ONNX Runtime execution providers on target platforms, STUN/TURN fleet (coturn; TURN egress is the main COGS — assumed ≤ 15% of sessions relay), OS capture/injection APIs (DXGI/ScreenCaptureKit/PipeWire — Wayland portals are the schedule risk on Linux). Assumes: assisted users can receive a code out-of-band (phone/SMS/ticket); design-partner MSPs provide real 3G/ADSL field traces for the bench corpus.

## 12. Risks and Open Questions

Top product risks: **(R1)** Windows UAC/secure-desktop interrupts control sessions — mitigation: explicit UX for elevation hand-back, documented limits (FR-13). **(R2)** Neural head video could erode trust if it ever misrepresents — mitigation: sender-side fallback triggers, permanent labeling, off-by-default until v1.1 telemetry proves it (Design §5 guardrails). **(R3)** Old-CPU endpoints can't sustain AV1 encode — mitigation: gear governor + H.264 fallback, accepting reduced edge on that leg. **(R4)** TURN relay costs scale with enterprise NAT strictness — mitigation: TCP-443 last resort, relay-rate KPI watched from alpha.

Open questions for stakeholder review: licensing model (open-core vs. proprietary; per-technician seat pricing assumed); enterprise SSO/SCIM timing; compliance roadmap (SOC 2 scope for the signaling/TURN fleet only, since media is E2EE); whether session recording (a frequent MSP ask) can ever coexist with the E2EE trust story — current position: consent-gated *local* recording only, v2 discussion.

## 13. Out of Scope (v1) — restated for clarity

Unattended access; cloud recording; >4-party calls; webinars/PSTN; SFU infrastructure; browser-only client (WebRTC-gateway compatibility mode is a v2 investigation — it would forfeit part of the codec edge and must not dilute the core).

---

*Traceability: FR/NFR IDs above map to Design Document §§3–10 and Architecture §§6–15; the CI bench (Arch §15) is the single source of truth for every quantitative bar in this PRD.*
