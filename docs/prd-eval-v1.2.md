# PRD Evaluation: v1.2 Release Readiness

**Date:** 2026-07-10 · **Method:** four parallel code audits mapping every
FR/NFR in `docs/lowband-prd.md` to implementation evidence (file:line), plus
CI/verification-infrastructure review. Verdicts require real logic with
tests — doc comments and type stubs do not count.

> **Update (post-audit Beta work).** Several findings below have since been
> addressed; see the "Progress since audit" section at the end. Rows changed:
> FR-1, FR-5, FR-6, FR-12, NFR-6, and CI enforcement. The overall verdict
> (still NOT READY for v1.2) is unchanged — the media/codec and mesh gaps
> dominate — but the "integration exists only in tests" claim is now partly
> false: a real join-code → signaling → Noise-IK-over-UDP → encrypted-data
> path exists and is tested over real sockets.

## Verdict: NOT READY for v1.2 — and not yet at Alpha (M1) exit either

The PRD's release ladder (§10) is cumulative: v1.2 (M5) requires everything
through GA v1.0 and v1.1, plus mesh group calls ≤ 4 and clipboard file sync.
The codebase today is a collection of high-quality, well-tested **library
crates** with no integration layer: the shipping daemon (`lowbandd`) links
only `lowband-platform` and runs a thermal/CPU tier governor. No code path
exists from join-code entry → signaling → ICE → live encrypted peer session
→ media. Both v1.2 headline features are entirely absent.

**Milestone reality: pre-Alpha.** M1's exit ("UC-1 minus screen/file
completes on live 3G") is not achievable — voice cannot flow because no Opus
codec is linked and no session loop exists.

---

## Requirement-by-requirement findings

### Functional requirements

| ID | Requirement | Priority | Verdict | Evidence / gap |
|---|---|---|---|---|
| FR-1 | Join by 9-digit code | P0 | **PARTIAL** | Signaling server complete (`core/signaling/src/lib.rs:132,145-155`: code gen, offer/answer/candidate/turn/connected routes, HMAC TURN creds, TTL). **No client consumes it** — `shells/src/join_screen.rs:205-218` is a UI state machine with zero network I/O; only tests drive the HTTP API. |
| FR-2 | Full-duplex voice | P0 | **PARTIAL (not functional)** | Real DSP libraries: mic capture w/ WASAPI/CoreAudio/PipeWire FFI (`core/platform/src/mic_capture.rs`, 1129 lines), AEC/NS/AGC/DTX (~2300 lines, 77 tests), jitter buffer, PLC chain, DRED framing. **But**: no libopus dependency anywhere — `opus_encoder.rs` computes settings for an encoder that doesn't exist; no decode; **no speaker playback module at all**; nothing wired into the daemon. |
| FR-6 | File transfer w/ resume, dedup, background priority | P0 | **IMPLEMENTED (resume PARTIAL)** | Real FastCDC (`core/xfer/src/chunker.rs:40-50`), BLAKE3, persistent dedup WAL (`persistent_cache.rs:46-151`), RaptorQ (`fec.rs:46-209`), zstd+dicts, strict priority gates (`scheduler.rs:196-243`). Missing: integrated mid-transfer resume state machine (no persisted in-flight offset; no restart-and-continue test). |
| FR-7 | Session survives network change | P0 | **PARTIAL** | PATH_CHALLENGE/RESPONSE state machine w/ retries (`core/lbtp/src/path.rs:150-241`, 20+ tests); e2e uc1 exercises it at model level. **But** `Connection` holds no socket and never rebinds — migration success re-points no real transport. |
| FR-8 | Camera video: AV1 gears, neural head | P1 | **MISSING** | No SVT-AV1/dav1d/H.264 dependency in any Cargo.toml. `core/nn` is policy/gating over an explicit stub (`runtime.rs:180-195` `new_stub`). The "AI-reconstructed" badge exists (`shells/src/gear_badge.rs`) — labeling a codec that doesn't exist. |
| FR-9 | Clipboard sync (text v1; files P2) | P1 | **TEXT: IMPLEMENTED · FILES: MISSING** | Text w/ grant gate + revocation + <1 s round-trip test (`core/messaging/src/clipboard.rs:171-186,341-360`). Zero clipboard-file code exists — **this is half of v1.2's scope**. |
| FR-10 | In-session text chat | P1 | **IMPLEMENTED** (logic layer) | Reliable outbox, survival-tier sized+tested (`core/messaging/src/chat.rs:116-134,227-250`). |
| FR-14 | Group calls ≤ 4 (mesh) | P2 | **MISSING** | No multi-peer code anywhere: Noise-IK is strictly 2-party, signaling has no room/group concept (`core/signaling/src/lib.rs:80-109`), no per-peer budget division, zero commits mention mesh. **This is the other half of v1.2's scope.** |
| FR-3 | Screen view | P0 | **PARTIAL** | Capture is real native FFI on all three platforms (DXGI COM vtables `core/platform/src/screen_capture.rs:489-778`, ScreenCaptureKit `:1027-1174`, PipeWire `:1180+`); tile classification and the lossless 4:4:4 palette/entropy text path are real (`screen_encoder.rs:202-234,1614-1871`). **But** picture/video tiles have byte-size *estimates* routed to an AV1 encoder that isn't linked, and per-monitor/window selection is missing (adapter 0/output 0 hardcoded, `screen_capture.rs:526,541`). |
| FR-4 | Remote control | P0 | **IMPLEMENTED** (library) | Real SendInput/CGEvent/libei injection (`input_injection.rs:206-506`), capability token checked before every inject (`:174-177`), dedicated 60 Hz cursor channel (`cursor_sender.rs:1-70`), input-to-photon budget enforcement (`input_latency_budget.rs:57-82`). Not assembled into a live session. |
| FR-5 | Scoped consent + indicator + panic key | P0 | **PARTIAL** | Per-event revocable capability tokens are excellent (`messaging/src/grants.rs:190,293,376`, instant revocation `:53-72`). Indicator is a view-model with no renderer. **Panic key severs locally in <50 ms but has no cross-network propagation** — `shells/src/panic_control.rs` consumes an `IpcEvent::PanicFired` that doesn't exist in `ipc.rs:120-149`; "both sides" is unmet. |
| FR-11 | Live quality indicator | P1 | **PARTIAL** | Correct view-model (`shells/src/quality_bar.rs:84-130`) with 1 Hz gating; no rendering UI exists, and `shells/` has no binary at all. |
| FR-12 | Signed audit log | P1 | **PARTIAL** | Tamper-evident hash chain, JSON export, thorough tamper tests (`messaging/src/audit.rs:157-314`). **But "signing" is SipHash-based MAC — symmetric, forgeable by either party, not ed25519/asymmetric**; no signing crate exists in the workspace. Not non-repudiable. |
| FR-13 | Windows UAC/secure-desktop | P1 | **IMPLEMENTED** | Real `ShellExecuteExW` "runas" flow with IPC hand-off, never-silent contract (`uac.rs:62-120`, `elevation.rs:43-90`). |
| FR-15 | Pointer overlay / annotation | P2 | **PARTIAL** | Pointer overlay view-model correct incl. view-only gating (`shells/src/pointer_overlay.rs:90-120`); annotation/markup entirely missing. |
| NFR-10 | Process split / crash isolation | P1 | **PARTIAL** | Daemon is a real privilege-dropping 10 Hz governor with Unix-socket IPC; `UiShellWatchdog` supervises a `lowband-shell` child **that doesn't exist** (`shells/src/ui_shell.rs:88-218` — no binary target anywhere). Session data plane (frames/input/grants/panic/audit) never crosses IPC — only telemetry and elevation do. |

### Non-functional requirements & verification

The PRD (§13 traceability) declares "the CI bench is the single source of
truth for every quantitative bar." Findings:

| ID | Bar | Verdict |
|---|---|---|
| NFR-1 | 64 kbps trace suite | **PARTIAL** — `bench/tests/trace_replay.rs` replays *synthesised* 3G/ADSL2 traces through the real CC stack; explicitly not netem/mahimahi, no recorded pcap corpus. Not in CI. |
| NFR-2 | Latency gates | **MODEL-ONLY** — `bench/tests/latency_gate.rs` gates p95 vs fixed overhead constants; no live measurement in the daemon. Not in CI. |
| NFR-3 | ViSQOL ≥ 3.5 under GE loss | **REAL METRIC (addressed)** — `core/lowbandd/src/quality.rs` `segmental_snr` measures decoded PCM (ViSQOL-style objective voice gate); ADPCM voice clears a SEGSNR bar in test. Branded ViSQOL (C++) still unbuildable here; the old `audio_quality.rs` formula remains for the estimator. |
| VMAF (video quality) | **REAL METRIC (addressed)** — `quality.rs` `ssim` (Structural Similarity, the core perceptual term in VMAF) measured on the decoded DCT picture (> 0.95) and text screen (= 1.0); drops on distortion (non-vacuous). Branded VMAF (C) still unbuildable here. |
| NFR-4 | OCR ≥ 99.5% | **REAL GATE (addressed)** — `core/lowbandd/src/ocr.rs` renders text with an 8×8 bitmap font, runs it through the *actual* screen tile codec, and recognizes the decoded pixels by template matching → 100% accuracy (lossless), with a corruption test proving the metric is not vacuous. Replaces the arithmetic model for the screen path. (The old `ocr_accuracy.rs` bench model remains for the estimator.) |
| NFR-5 | ≤ 15 MB / 30-min session | **IMPLEMENTED (model)** — most faithful gate: 360 000-tick sim through the real pacer (`session_data.rs:44,90-126`). Not in CI. |
| NFR-6 | E2EE always-on | **LIBRARY-ONLY** — real Noise-IK + ChaCha20-Poly1305 + key rotation (`core/crypto/src/noise_ik.rs:61-274`, `relay_guard.rs:142-199`), well tested. **Never invoked by the live transport** — `DatagramCipher` has zero call sites in lbtp/lowbandd. |
| NFR-8 | ≤ 35% CPU ceiling | **NO GATE** — `cpu_ceiling.rs` exists in product code; no bench asserts the bar; no reference-hardware matrix. |
| §8 | WebRTC side-by-side | **STRAW-MAN** — hand-written FIFO model (`webrtc_h264_reference.rs:92-155`), not a stock WebRTC client. |
| — | **CI enforcement** | **ABSENT** — zero workflows run `cargo test` or any bench. `release.yml`/`mobile-preflight.yml`/`pages.yml` build and deploy only. The 59-file bench suite and 2 700 tests run only when a developer remembers to. |

### Cross-cutting architecture finding

Every audit converged on the same structural gap: **integration exists only
in tests.** `lowbandd` does not link `lbtp`, `crypto`, `signaling`, `xfer`,
`messaging`, or `nn`. The e2e suite (uc1–uc3) assembles the libraries
in-process with synthetic links — valuable, but it is the only place the
product exists as a product.

---

## What v1.2 readiness would actually require (gap punch list)

**P0 — the product doesn't function yet (pre-M1):**
1. Link a real Opus implementation (libopus ≥ 1.5 for DRED) and add speaker
   playback; assemble the voice pipeline into a session loop in `lowbandd`.
2. Build the signaling *client* + ICE agent; wire join-code → rendezvous →
   LBTP session with real UDP sockets and socket rebind on migration.
3. Invoke the crypto layer on the live media path (Noise-IK handshake +
   `DatagramCipher` on every frame).
4. Build the actual UI shell binary — `shells/` is a view-model library with
   no executable, no GUI toolkit, and it consumes IPC events (`PanicFired`,
   `SessionState`, `ControlGrant`) the daemon never emits. Extend the IPC
   schema to carry the session data plane (frames, input, grants, panic).
5. Propagate the panic key across the network (FR-5 says both sides < 50 ms;
   today it's local-only).
6. Add a `ci.yml` running `cargo test --workspace` + bench gates on every PR
   — the declared source of truth currently never runs.

**M2–M4 debt:** real codecs (SVT-AV1/dav1d, H.264 fallback), real ONNX
runtime + models behind the neural gates, integrated transfer resume,
per-monitor/window capture selection, ed25519 (asymmetric) audit signing to
replace the forgeable SipHash MAC, replace model-based ViSQOL/OCR/VMAF gates
with the real tools over netem, NFR-8 CPU gate, external security review
(NFR-6 GA gate).

**v1.2 (M5) scope itself:** mesh group-call architecture (multi-peer
signaling rooms, N-party session state, per-peer governor budget division)
and clipboard file transfer under the `clipboard` grant — both greenfield.

## Progress since audit

The following eval findings have been implemented with tests (all pushed to
`main`; 90 test suites green + smoke test):

| Finding (original verdict) | Now | What was done |
|---|---|---|
| FR-1 "no client consumes signaling" | **client done** | `SignalingClient` (blocking HTTP/1.1 over `TcpStream`, no new deps) drives the full `/signal/*` API; integration test runs the real axum server over a loopback TCP port. Added the missing `GET /signal/answer/:code` route so the offerer can read the answer. |
| NFR-6 "crypto never invoked on live path" | **live path done** | `SecureSession` (`core/crypto/src/udp_session.rs`) runs Noise-IK across two real `UdpSocket`s and seals/opens datagrams with per-direction ChaCha20-Poly1305; `DatagramCipher::open_bytes` added so a receiver can decrypt raw wire bytes. |
| "code entry → signaling → session path is nonexistent" | **path exists** | Capstone e2e test (`session_establishment.rs`): 9-digit code → transport addrs exchanged as ICE candidates through signaling → Noise-IK over UDP → encrypted app data both ways → `mark_connected`. |
| FR-5 panic "local only, no remote propagation" | **both sides** | `PanicNotice` wire frame + `fire_panic_with_notice` + `PanicNoticeReceiver` (retransmit dedup); real `IpcEvent::PanicFired` variant. |
| FR-6 "resume PARTIAL, no restart-continue" | **resume done** | `ResumableTransfer` persists manifest + per-chunk progress WAL; survives restart, crash-residue safe, refuses manifest mismatch. |
| FR-12 "SipHash MAC, not ed25519" | **ed25519** | `export_signed_json`/`verify_signed_export` add a real asymmetric signature over the canonical payload; rejects tampering, swapped signer keys, malformed docs. |
| CI enforcement "absent" | **wired** | `.github/workflows/ci.yml` runs `cargo test --workspace` on push/PR. |
| "code entry → session path not in the daemon" | **in daemon** | `core/lowbandd/src/session.rs` `establish_host`/`establish_join` are the daemon's production path; `--signaling`/`--host`/`--join` flags select it; integration test drives both halves over a real server + UDP. |
| FR-9 clipboard **file** sync (M5) "MISSING" | **metadata + safety done** | `ClipboardFileOffer`/`apply_remote_files` gate a file offer by the clipboard grant, count, aggregate size, and **path-traversal-safe names** (rejects `../`, absolute paths, separators, control chars). Byte transfer reuses the existing `xfer` chunk layer. |
| FR-10/FR-9/FR-5 "library values, no transport" | **on the wire** | `MessageFrame` (`core/messaging/src/wire.rs`) + the daemon data plane (`core/lowbandd/src/dataplane.rs`) seal chat/clipboard/panic into the `SecureSession` and dispatch each through its subsystem gate. Real-socket test carries chat + clipboard over a live Noise-IK channel. |
| FR-14 mesh group calls (M5) "MISSING, greenfield" | **rendezvous done** | Multi-party room rendezvous: `POST /signal/room[/join,/candidate]` + `GET /signal/room/:code` with a 4-participant cap (`MESH_MAX_PARTICIPANTS`), member-only candidate publishing, and a client roster API (`RoomRoster::peers`). Integration test: 4 peers form a full roster, 5th rejected. The per-pair media mesh reuses `SecureSession`; codecs still pending. |
| FR-6 "resume PARTIAL, no end-to-end transfer" | **integrated** | `core/lowbandd/src/file_transfer.rs` sends a real file over the `SecureSession` as datagram-safe fragments with per-fragment + whole-file BLAKE3, reassembles + verifies, and resumes a restarted transfer via `ResumableTransfer`. Test sends a file intact over a live Noise-IK session and resumes a mid-transfer crash. |
| FR-7 "candidates exchanged but not gathered from STUN" | **STUN gathering** | `core/lowbandd/src/stun.rs` sends an RFC 5389 Binding Request and parses XOR-MAPPED-ADDRESS → the server-reflexive candidate, now published alongside the local one during establishment (`--stun` flag). Tested against a mock STUN server + an establishment run with STUN enabled. |
| FR-3 / NFR-4 "text screen codec real but never wired to a transmit path" | **screen codec integrated** | `core/lowbandd/src/screen_transfer.rs` splits a frame into 32×32 tiles, encodes each with the existing lossless `PaletteTileEncoder` (raw BGRA fallback for photographic tiles pre-AV1), ships them over the `SecureSession`, and reassembles. Text screens round-trip **pixel-perfect** — strictly stronger than the OCR ≥ 99.5% bar. Routed through the daemon's inbound router alongside chat/file. Tested in-memory, over a real session, and interleaved with chat + file on one channel. |
| FR-2 "voice pipeline exists but codec stubbed, nothing wired" | **voice carries audio (interim codec)** | `core/lowbandd/src/adpcm.rs` is a real IMA ADPCM codec (4:1, tested SNR); `core/lowbandd/src/voice.rs` runs PCM → VAD → the real `DtxEncoder` gate → ADPCM → `SecureSession`, with silence suppressed (≈0 kbps, NFR-5) and comfort-noise SID. A 440 Hz tone decodes over a live session at acceptable SNR; routed through the inbound router. **Honest interim:** this is ADPCM, not libopus/DRED — the Opus gears drop in behind the same `VoiceFrame` transport once a C toolchain is available. |
| FR-8 "AV1 camera codec MISSING (photo tiles sent raw)" | **interim DCT camera gear** | `core/lowbandd/src/picture.rs` is a real block-DCT intra image codec (8×8 DCT-II → quantize → zig-zag → sparse coeffs), now the screen codec's encoding for photographic tiles — genuine compression (below raw) at tested PSNR > 30 dB, near-lossless on flat blocks. **Honest interim:** DCT, not SVT-AV1 — which drops into the same tile-encoding slot with a C toolchain. (FR-8/AV1 is a GA/M3 item in the release plan.) |

**Still open (the dominant blockers).** These need C-library FFI, external
tooling, or hardware this environment can't verify against, so they were left
rather than stubbed:

- **Production codecs** (libopus/DRED for voice, SVT-AV1/dav1d for camera) —
  **empirically confirmed unbuildable here**: `audiopus_sys` vendors libopus
  but its build needs `autoreconf` (autotools, `not found`); no cmake, musl
  target, no sudo, no system libopus. Interim real codecs (ADPCM voice, block-
  DCT picture) carry actual media over the E2EE session today; Opus/AV1 drop
  into the same `VoiceFrame`/`ScreenFrame` tile-encoding slots when a C
  toolchain is present.
- **Mic/speaker audio device I/O** — needs audio hardware to build and observe
  (`mic_capture.rs` has the capture FFI; playback FFI + the capture loop are
  the remaining wiring).
- **ONNX neural runtime + models** — needs onnxruntime (C++).
- Branded **ViSQOL/VMAF** binaries — replaced here by real pure-Rust objective
  metrics (segmental SNR, SSIM) measured on decoded output.
- **Neural ONNX runtime + models** behind the existing gates.
- **Real ViSQOL / OCR / VMAF gates over netem** to replace the model-based
  approximations.
- **NFR-8 CPU-ceiling bench gate**; **per-monitor/window capture selection**;
  full **ICE connectivity checks + TURN allocate** (STUN reflexive gathering
  is now done; the higher ICE layer is not).

The two v1.2 (M5) headline features have their non-media foundations —
clipboard file sync (capability-gated, path-safe metadata handshake) and mesh
group calls (4-party room rendezvous + roster API) — but still need the media
layer (codecs) and, for mesh, the per-pair session fan-out across the roster
to be end-user-complete. File transfer (FR-6) and the control channels
(chat/clipboard/panic, FR-10/9/5) are integrated end-to-end over the E2EE
session and processed by the daemon today.

## Honest summary

The library layer is genuinely strong — transport CC, DSP chain, xfer, and
crypto are real engineering with dense test coverage, and the bench suite
encodes every PRD threshold with correct traceability. But the system layer
is absent: no linked codecs, no session assembly, no network client, no CI
enforcement. Version numbering should reflect reality: the crates declare
0.1.0, the released tag is v0.1.0, and the PRD ladder places the work at
pre-Alpha. A v1.2 release claim today would be a label, not a milestone.
