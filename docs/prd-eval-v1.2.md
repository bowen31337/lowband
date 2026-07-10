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
| NFR-3 | ViSQOL ≥ 3.5 under GE loss | **MODEL-ONLY** — score computed by formula (`audio_quality.rs:263-265`), not the ViSQOL tool. Real GE estimator exists and is consumed by congestion control. Not in CI. |
| NFR-4 | OCR ≥ 99.5% | **MODEL-ONLY** — arithmetic capacity model (`ocr_accuracy.rs:185-203`); no OCR engine, no decoded frames. Not in CI. |
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

**Still open (the dominant blockers):** real codecs
(libopus/SVT-AV1/dav1d/H.264) and speaker playback — so no media actually
flows over the now-established secure channel; the media data-plane loop in
the daemon (the channel stands up but carries no audio/screen/input yet); an
ICE agent for real NAT traversal (candidates are exchanged but not gathered
from STUN); per-monitor capture selection; the neural runtime behind its
gates; the model-based verification gates replaced with real ViSQOL/OCR/VMAF
over netem; the NFR-8 CPU gate; and of the two v1.2 (M5) headline features,
**mesh group calls remain greenfield** (clipboard file sync now has its
capability-gated metadata handshake, though it still needs wiring to the OS
clipboard and the `xfer` pull on both ends).

## Honest summary

The library layer is genuinely strong — transport CC, DSP chain, xfer, and
crypto are real engineering with dense test coverage, and the bench suite
encodes every PRD threshold with correct traceability. But the system layer
is absent: no linked codecs, no session assembly, no network client, no CI
enforcement. Version numbering should reflect reality: the crates declare
0.1.0, the released tag is v0.1.0, and the PRD ladder places the work at
pre-Alpha. A v1.2 release claim today would be a label, not a milestone.
