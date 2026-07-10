# LowBand — Distribution & Go-to-Market Playbook

**Doc type:** GTM strategy · **Status:** Draft · **Companions:** PRD v1.0, `packaging/`

This is the concrete plan for (a) shipping signed, installable builds on every
platform, (b) which stores/channels to use — and which to deliberately skip —
and (c) how to actually acquire users, given who buys this product.

---

## 1. Reality check: what kind of product this is per channel

LowBand is a **privileged desktop daemon** (screen capture, input injection,
a system service) plus native UI shells. That shapes distribution more than
any marketing preference:

| Channel | Verdict | Why |
|---|---|---|
| Direct download (signed MSI / pkg / deb / rpm) | **Primary** | What every remote-control vendor does (TeamViewer, AnyDesk, RustDesk). Full control, silent-deploy friendly. |
| winget / Homebrew / Chocolatey / apt-yum repos | **Primary** | Where technicians actually install tools from. Near-zero cost. |
| RMM / MDM deployment (Intune, Datto, NinjaOne) | **Primary for revenue** | Mo deploys to fleets silently. `packaging/windows/mdm/lowband.admx` and the PPPC `mobileconfig` already anticipate this. |
| Mac App Store | **Skip for the daemon** | Sandbox forbids input injection, launch daemons, and the entitlements in `packaging/macos/entitlements/`. Also AGPL-3.0 conflicts with App Store terms (the VLC precedent). Use Developer ID + notarization outside the store. |
| Microsoft Store | **Optional, later** | Win32 apps are admitted, but the service install and ACL setup in `lowband.wxs` doesn't fit MSIX containers. Revisit for a viewer-only client. |
| iOS App Store / Google Play | **v1.1, viewer app only** | The PRD's assisted-side mobile viewer is the store-shaped artifact. iOS requires solving the AGPL problem first (§4). Google Play accepts AGPL. |
| Flathub / Snap / AppImage | **Yes, for reach on Linux** | Input injection via libei/XTest needs the RemoteDesktop portal on Wayland — already the plan per the architecture doc. Flathub listing doubles as discovery. |

**Takeaway:** app stores are not the launch vehicle — they're a v1.1 accessory
for the mobile viewer. The launch vehicle is signed direct downloads + package
managers + RMM silent deploy, because the buyer is an IT professional.

---

## 2. Build & signing pipeline (per platform)

### Windows — `packaging/windows/`
Already present: `lowband.wxs` (WiX, service account ACLs), `build_msi.ps1`, ADMX/ADML templates.
To ship:
1. **Code-signing certificate.** Prefer **Azure Trusted Signing** (cheap, cloud HSM, CI-friendly) or an EV Authenticode cert. Unsigned MSIs are dead on arrival with MSPs (SmartScreen + AV heuristics hate screen-capture + input-injection binaries — signing reputation is existential for this product category).
2. Sign **both** `lowbandd.exe`/shell binaries and the MSI (`signtool sign /fd SHA256 /tr <rfc3161> …`).
3. Publish: GitHub Releases → **winget manifest** (microsoft/winget-pkgs PR) → Chocolatey package. Document the silent install line MSPs will paste into their RMM: `msiexec /i lowband.msi /qn`.

### macOS — `packaging/macos/`
Already present: `build_pkg.sh`, `distribution.xml`, hardened-runtime entitlements, launchd plist, PPPC profile for MDM.
To ship:
1. **Apple Developer Program** ($99/yr). Two certs: *Developer ID Application* (binaries) and *Developer ID Installer* (the pkg).
2. `codesign --options runtime --entitlements packaging/macos/entitlements/lowbandd.entitlements …` → `productsign` the pkg → **`xcrun notarytool submit --wait`** → `xcrun stapler staple`. Un-notarized pkgs won't open on current macOS.
3. Publish: GitHub Releases + **Homebrew cask** (`brew install --cask lowband`).
4. Ship the PPPC `mobileconfig` alongside for MDM fleets so Screen Recording/Accessibility consent is pre-granted — a real differentiator for enterprise deploys.

### Linux — `packaging/linux/`
Already present: `lowbandd.service`, polkit policy, `postinstall.sh`.
To ship:
1. `cargo-deb` + `cargo-generate-rpm` for .deb/.rpm; host an apt/yum repo (Cloudsmith or packagecloud) so `apt install lowband` works and updates flow.
2. **Flathub** listing (portal-based capture/injection on Wayland) + an AppImage for the "just give me a file" crowd.
3. Sign packages and repo metadata (GPG); publish checksums + **minisign/Sigstore signatures** for every artifact — it feeds the trust story below.

### CI/CD (all platforms)
- **Implemented:** `.github/workflows/release.yml` builds on version tags — Linux x86_64/aarch64 (musl, via cargo-zigbuild), Windows x86_64/aarch64 (MSVC) + best-effort unsigned MSI, macOS arm64/x86_64 + `lipo` universal — then attaches archives, SHA256SUMS, and GitHub build-provenance attestations to the release.
- **Implemented:** `packaging/build-dist.sh` reproduces the same archives locally (zig as the cross C toolchain; macOS targets link against zig's bundled libSystem stubs).
- Still to add when signing lands: signtool/notarytool steps in the workflow, plus **SBOM** generation (`cargo auditable` / `cargo-cyclonedx`).
- Reproducible-ish builds + published hashes: for a tool whose pitch is "E2EE, no vendor can see your media," verifiable artifacts are marketing, not hygiene.

### Interim: distributing unsigned builds (alpha, before the certs exist)

There is a window — design-partner alpha, early beta — where builds ship before
Azure Trusted Signing / Apple Developer enrollment lands. Plan for it instead of
letting testers bounce off OS warnings. Three principles:

1. **Be honest in the copy.** The audience is IT professionals; "this build is
   not yet OS-signed, here's how to verify it instead" builds more trust than
   pretending the warning doesn't exist. Never instruct anyone to disable
   Gatekeeper globally (`spctl --master-disable`) or turn off Defender/AV — it's
   terrible advice generally and brand poison for a security-first product.
2. **Give a verification substitute.** Publish SHA-256 checksums and
   **minisign or Sigstore signatures** for every artifact, plus GitHub Actions
   **build provenance attestations** (`gh attestation verify`). "Unsigned by the
   OS, verifiable by you" is a coherent story for this audience; "just click
   through the warning" is not.
3. **Prefer install paths that never trigger the warnings** (below) over
   documented click-throughs.

**macOS.** Gatekeeper triggers on the `com.apple.quarantine` xattr, which
*browsers* set on downloads. Two consequences:

- The `curl -fsSL https://get.lowband.dev | sh` installer path avoids
  quarantine entirely (curl doesn't set the xattr) — make it the primary
  documented install for pre-release, which is what the marketing page already
  shows.
- For browser-downloaded pkgs: on current macOS the old right-click → Open
  trick no longer works for unsigned installers; the supported path is: attempt
  to open → System Settings → Privacy & Security → **"Open Anyway"**. Document
  exactly that, with a screenshot. Power users can `xattr -d
  com.apple.quarantine lowband.pkg` after verifying the checksum.
- Even without certs, **ad-hoc sign** (`codesign -s - --force`) every binary in
  CI — mandatory on Apple Silicon (unsigned arm64 binaries are killed on
  launch), and it keeps the eventual signed pipeline shape identical.
- Homebrew users: `brew install --cask --no-quarantine lowband` on a tap you
  control (the main cask repo is fine with unsigned apps; users opt into
  `--no-quarantine` themselves).

**Windows.** SmartScreen keys off the Mark-of-the-Web (MOTW) zone identifier,
which browsers attach — plus signer reputation:

- Browser-downloaded MSI: the honest path is SmartScreen's **"More info" →
  "Run anyway"**; document it with a screenshot and pair it with the checksum
  step. PowerShell-literate testers can `Unblock-File .\lowband.msi` after
  verifying the hash (that is Microsoft's supported MOTW-removal cmdlet).
- **RMM/MDM deployment sidesteps SmartScreen entirely** — files pushed by
  Intune/Datto/NinjaOne don't carry MOTW, and `msiexec /qn` from an admin shell
  doesn't hit the SmartScreen UX. Since design partners are MSPs, lead alpha
  docs with the RMM deploy path; it's both the realistic fleet path and the
  frictionless one.
- Submit every pre-release build to **Microsoft's malware-analysis portal**
  (accepts unsigned files) and pre-scan on VirusTotal — remote-access tools get
  heuristic flags, and you want false positives resolved before a tester sees
  them, not after.

**Linux.** No OS gatekeeping; checksums + GPG-signed repo metadata cover it.

**Sample download-page copy (pre-release):**

> Alpha builds are not yet code-signed — Windows SmartScreen and macOS
> Gatekeeper will warn you. That's expected; signing lands with the public
> beta. Until then, every build ships with a SHA-256 checksum and a minisign
> signature you can verify (`minisign -Vm lowband.msi -P <our key>`), and the
> build provenance is publicly attested on GitHub. If you'd rather wait for
> signed builds, join the beta list.

Sunset this section the day the certs land: the *only* acceptable long-term
answer to SmartScreen/Gatekeeper is real signing + notarization + reputation,
and lingering "click through the warning" docs actively damage the trust story
that sells this product.

### Mobile viewer (v1.1)
- Android: Kotlin viewer over the LBTP core via FFI → Play Console ($25 one-time), internal → closed → production tracks. AGPL is acceptable on Play.
- iOS: same core via Swift FFI → TestFlight → App Store ($99/yr) — **blocked on licensing (§4)** and on scoping the viewer to view-only (no injection needed, so it sandboxes cleanly).

---

## 3. Store-adjacent trust signals (do these regardless of channel)

- **VirusTotal pre-scan** every release; remote-access tools get flagged — catch it before users do, and pre-file vendor whitelisting requests (Microsoft, CrowdStrike, SentinelOne) with your signing cert.
- Publish a **security page**: E2EE design, external security review (GA gate per the PRD), signed audit logs, coordinated-disclosure policy. Mo reads this page before any store listing.
- **SOC 2 scope note**: only signaling + TURN fleet touch customer traffic (ciphertext only) — small, cheap audit surface. Huge sales unlock for MSP contracts.

---

## 4. The licensing landmine (decide early)

LowBand is **AGPL-3.0**. Consequences:

- **Apple App Store distribution of AGPL code is effectively incompatible** (App Store terms impose restrictions AGPL forbids; VLC was famously pulled). Options: (a) dual-license the mobile viewer under a proprietary license — requires a **CLA/DCO from all contributors from day one**, so set that up *now* while contributor count is ~1; (b) keep iOS as a thin proprietary viewer over a permissively-licensed protocol crate; (c) skip iOS.
- AGPL is a **feature** for the self-hosted/open-source audience (§5) — "the vendor can't rug-pull you" — and pairs naturally with the open-core model the PRD floats (per-technician seat pricing for the hosted signaling/TURN fleet + enterprise features like SSO/SCIM, while the core stays open).

---

## 5. Marketing: who buys, and where they hang out

**ICP (from the PRD):** Tan (MSP/internal-IT technician, chooses tools), Mo
(IT manager, signs), Ana (assisted user, must never be blocked). Technicians
discover; managers approve. So market to technicians, arm them to sell upward.

### Positioning
One line: **"Remote support that works where 64 kbps is a good day."**
Against the field: mainstream tools publish 600 kbps–1.8 Mbps floors; LowBand
completes real assist sessions at a quarter of that, E2EE, with no media server.
Never claim "better video" — claim **task completion on bad links**, which the
CI trace suite literally measures (PRD: "the comparison regenerates from CI,
not from marketing" — put that sentence on the website; it's the brand).

### Flagship content (build once, feeds everything)
1. **The benchmark page** — side-by-side vs. a stock WebRTC client on recorded 3G/ADSL2 traces, regenerated from CI, methodology public. This is the HN/press artifact.
2. **A 90-second screen recording** of a real assist session over an emulated 64 kbps 3G link (netem/mahimahi, corpus already planned in `bench/`): crisp terminal text, obedient cursor, file push surviving an IP change. Nothing sells this product like watching it not die.
3. **Engineering blog series** (each is front-page-of-HN material): "Making voice survive 5% burst loss with Opus DRED", "Your screen is not a video: text-delta lanes", "A 10 Hz governor instead of hope", "Why the remote cursor gets its own channel".

### Channels, in priority order
1. **MSP communities** — r/msp, MSPGeek (Slack/Discord), technician podcasts/YouTube (Lawrence Systems, 2.5 Admins). Design-partner case studies from the alpha (PRD M1) convert here: "rural clinic on ADSL2, session data under the prepaid cap."
2. **Show HN + lobste.rs** at public beta: "Show HN: LowBand – remote support that works at 64 kbps (Rust, P2P, E2EE)". Rust + novel transport + honest benchmarks is the exact HN profile. Product Hunt the same week.
3. **Self-hosted / open-source** — r/selfhosted, awesome-selfhosted and awesome-rust lists, Flathub discovery. AGPL + "no media server" is native to this audience; they become the RustDesk-comparison evangelists.
4. **SEO comparison pages** — "LowBand vs TeamViewer / AnyDesk / RustDesk / Chrome Remote Desktop on slow connections", "remote desktop over 3G", "remote support for satellite internet". Low volume, extremely high intent.
5. **Verticals where bad links are the norm** — maritime IT, mining/energy field ops, humanitarian/NGO (MSF-type field clinics), rural healthcare and school districts, government field services. Small conferences, direct outreach, one lighthouse customer each.
6. **Conference talks** — FOSDEM (real-time comms devroom), RustConf, RIPE/NANOG-adjacent meetups for the transport story.

### Launch sequencing (mapped to PRD milestones)
- **Alpha (M1, Windows):** private. 5–10 design-partner MSPs recruited from r/msp + MSPGeek. Ship signed MSI + silent-deploy docs on day one (they will test RMM deployment first). Collect the two numbers that matter: time-to-fix and data-per-session.
- **Beta (M2, +screen/file/macOS):** public. Show HN + Product Hunt + benchmark page + demo video. Open the winget/Homebrew taps. Start the blog series.
- **GA (M3, +Linux/audit/MSI-pkg admin):** press outreach to IT trade media (The Register loves this angle), SOC 2 scope note, security-review results published, pricing live (open-core, per-technician seat), Flathub.
- **v1.1 (M4, neural gears + mobile viewer):** Play Store launch of the Android viewer; iOS pending the §4 licensing decision. Second HN moment: "AI-reconstructed head video at 12 kbps — always labeled."

### Metrics that tell you it's working
Direct-P2P rate ≥ 85%, task-completion ≥ 95% on the trace suite (product), plus:
winget/brew install counts, design-partner → paid conversion, "sessions per
technician per week" (the PRD's retention metric), and inbound from comparison
pages. Vanity stars are not a KPI; weekly active technicians are.

---

## 6. First five concrete actions

1. Set up the release CI matrix (build + sign + notarize + SBOM on tag) — everything else hangs off it.
2. Buy the two signing identities (Azure Trusted Signing, Apple Developer Program) and wire them into CI secrets.
3. Adopt a CLA/DCO now to keep the dual-licensing door open for iOS.
4. Record the 64 kbps demo video against the netem harness; embed it in the `marketing-site/` hero.
5. Draft the winget manifest + Homebrew cask so they're ready the day the first public beta tag lands.
