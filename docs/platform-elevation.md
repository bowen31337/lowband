# Per-Platform Privilege Escalation Reference

Feature 155 — LowBand enforces one rule: **every privilege escalation is explicit,
logged, and user-visible.  There are no silent grant paths.**

This document is the authoritative reference for operators, auditors, and
contributors.  It covers the mechanism used on each platform, what is granted at
install time vs. runtime, and what is forbidden.

---

## Core principle

A "silent" escalation is any of the following — all of them are bugs:

| Forbidden pattern | Why it is silent |
|---|---|
| Re-trying a failed OS call with higher privileges without logging the failure | Hides the escalation from audit logs |
| SUID binary or ambient Linux capability used without an explicit grant record | Grants root without user interaction |
| `setuid(0)` / `sudo` from the running daemon after the privilege drop | Restores root inside a supposedly least-privilege process |
| Windows `ImpersonateLoggedOnUser` / `CreateProcessWithLogonW` without a UAC prompt | Acquires an elevated token without user consent |
| Falling back to a root-owned helper when the normal path fails | Hides privilege use behind an error recovery path |
| Swallowing an `ElevationOutcome::Denied` and proceeding as if granted | Ignores explicit user refusal |

The type system enforces the last rule: `ElevationOutcome` is `#[must_use]`; the
compiler warns (or errors with `#![deny(unused_must_use)]`) if the caller drops
it without matching.

---

## Windows

### Service account

The daemon runs as the **`NT SERVICE\LowBandDaemon`** virtual service account.
Virtual accounts have no password, no interactive logon right, and no membership
in any group beyond the built-in `Users` group.  They are isolated to the local
machine.

### Rights held at install time

The MSI (`packaging/windows/lowband.wxs`) grants the following ACEs during
installation; no further elevation is required at runtime:

| Right | Mechanism | ACE |
|---|---|---|
| DXGI Desktop Duplication (screen capture) | Service account is in the `Administrators` group only during install; the MSI sets the ACE on the DXGI device | `NT SERVICE\LowBandDaemon`: `GENERIC_READ` |
| `SendInput` (keyboard/mouse injection) | Available to any process in session 0 with `UIPI` bypass via manifest | Manifest `uiAccess="false"` — no extra right needed |
| `%ProgramData%\LowBand\` read/write | MSI ACL in `<PermissionEx>` | `NT SERVICE\LowBandDaemon`: `GENERIC_ALL` |

### Runtime escalation path

The **daemon never calls UAC APIs**.  If the UI shell needs a right the service
account does not hold (e.g. writing to `HKLM`), the shell sends an IPC request
to the user-facing GUI process, which calls:

```
ShellExecuteEx(hwnd, verb="runas", file=<helper.exe>, ...)
```

or the COM elevation moniker.  Both display the UAC prompt on the **Secure
Desktop** — the protected desktop isolated from any user-mode process.  The
outcome (granted / denied) is returned to the shell over a named event; the
shell relays it to the daemon over the IPC socket as a typed `ElevationOutcome`.

### What is audited

Every `ElevationRequest::execute()` call emits two lines to the Windows Event
Log (forwarded to `stderr` in the daemon; the UI shell writes them to the
Application event log):

```
lowband: elevation requested  reason=<reason> mechanism=Windows-UAC
lowband: elevation outcome    reason=<reason> mechanism=Windows-UAC outcome=<Granted|Denied|Unavailable>
```

---

## macOS

### Service account

The daemon runs as **`_lowband`** via the `UserName` key in the LaunchDaemon
plist (`packaging/macos/launchd/com.lowband.lowbandd.plist`).  `_lowband` is a
system account with no home directory, no shell, and no login rights, created by
the package installer.

### Rights held via entitlements

The daemon binary is code-signed with the following entitlements
(`packaging/macos/entitlements/lowbandd.entitlements`):

| Entitlement | Purpose |
|---|---|
| `com.apple.security.device.screen-capture` | Screen Recording right brokered by TCC |
| `com.apple.security.temporary-exception.mach-lookup.global-name` | Access to `com.apple.replaykit.recording-proxy` |
| `com.apple.security.cs.allow-unsigned-executable-memory` | Required by certain codec paths |

Entitlements are static — they do not grant the right; they make the process
*eligible* to receive it.  The right itself is granted once by TCC.

### First-use TCC prompt

When the daemon first calls `CGRequestScreenCaptureAccess()` or
`AXIsProcessTrustedWithOptions`, macOS displays the **TCC consent dialog** to
the user.  The user must click "Allow."  TCC writes the decision to
`/Library/Application Support/com.apple.TCC/TCC.db`.

Subsequent process launches check the TCC database without prompting.  This is
**not** a silent escalation: the right was explicitly granted in the past.  If
the right is later revoked via System Settings → Privacy & Security, the next
call returns `false` and the daemon degrades gracefully.

### What is audited

`ElevationRequest::execute()` emits:

```
lowband: elevation requested  reason=<reason> mechanism=macOS-TCC
lowband: elevation outcome    reason=<reason> mechanism=macOS-TCC outcome=<Granted|Denied|Unavailable>
```

macOS additionally writes TCC decisions to the Unified Log under the subsystem
`com.apple.tcc` — use `log stream --predicate 'subsystem == "com.apple.tcc"'` to
observe them.

### Forbidden on macOS

- `AuthorizationExecuteWithPrivileges` — deprecated, prompts for a root
  password, and does not go through TCC.  Never used.
- `sudo` from the running daemon — not in the `sudoers` file for `_lowband`.
- Calling `tccutil reset ScreenCapture` from the daemon to re-trigger the
  prompt — would require SIP override and is expressly prohibited.

---

## Linux

### Service account

The daemon is launched by systemd (`packaging/linux/lowbandd.service`) as
`root` (to bind the IPC socket at `/tmp/lowband.sock`), then immediately drops
to **`_lowband`** via the sequence:

```
setgid(_lowband_gid)
setgroups([])          ← clears all supplementary groups
setuid(_lowband_uid)
verify: getuid() == _lowband_uid   ← aborts if still root
```

This sequence is in `core/lowbandd/src/main.rs: drop_privileges`.  Any failure
in the sequence causes `std::process::exit(1)` — the daemon refuses to run as
root.

### Systemd hardening

The unit file applies the following kernel-level restrictions in addition to the
account drop:

| Directive | Effect |
|---|---|
| `NoNewPrivileges=yes` | Prevents `setuid` executables and capabilities from granting new privileges to child processes |
| `ProtectSystem=strict` | Mounts `/usr`, `/boot`, `/etc` read-only |
| `ProtectHome=yes` | Hides `/home`, `/root`, `/run/user` |
| `RestrictNamespaces=yes` | Blocks `unshare`/`clone` namespace creation |
| `ReadWritePaths=/tmp /var/lib/lowband /var/log/lowband` | Restricts writable paths to the minimum set |

### Runtime capture via portal

Screen-cast and remote-desktop input at runtime go through the
**XDG Desktop Portal** (`org.freedesktop.portal.ScreenCast`,
`org.freedesktop.portal.RemoteDesktop`).  The portal:

1. Receives a DBus request from the daemon (running as `_lowband`).
2. Displays a compositor-provided consent dialog to the user on the active session.
3. Returns a PipeWire node handle — no root involved.

This is the standard mechanism; no `SUID`, no capabilities, no Polkit at runtime.

### Polkit for install-time actions

One-shot privileged actions during package installation use the Polkit action
defined in `packaging/linux/org.lowband.daemon.policy`.  The PolicyKit agent
(e.g. `polkit-gnome-authentication-agent`) displays a password prompt to the
administrator.  Defined actions:

| Action ID | Description | Implicit authorization |
|---|---|---|
| `org.lowband.daemon.install` | Create `_lowband` system account and install the systemd unit | `auth_admin` (always prompts) |
| `org.lowband.daemon.configure` | Write to `/etc/lowband/` configuration | `auth_admin_keep` (prompts once per session) |

`auth_admin` means the agent always prompts; there is no "automatically
granted to local users" path.

### What is audited

`ElevationRequest::execute()` emits the standard log lines to `stderr`.
The systemd journal captures `stderr` under `SYSLOG_IDENTIFIER=lowbandd`.

Linux also writes Polkit decisions to the system journal under
`GLIB_DOMAIN=polkit`; inspect with:

```
journalctl -t polkit-agent
```

### Forbidden on Linux

- SUID binaries owned by `root` — not present in the package; verified by the
  post-install script.
- `CAP_SYS_ADMIN`, `CAP_NET_ADMIN`, or any ambient capability on the daemon
  binary — the service unit does not set `AmbientCapabilities`.
- `sudo` rules for `_lowband` — not added to `/etc/sudoers`.
- Calling `setuid(0)` from the running daemon — impossible after the drop
  because `NoNewPrivileges=yes` is set and the `_lowband` account has no
  `CAP_SETUID`.

---

## Cross-platform: the `ElevationRequest` type

All platform paths converge on the same Rust type in
`core/platform/src/elevation.rs`.  The lifecycle is:

```
ElevationRequest::new(EscalationReason::ScreenCapture)
    → logs "elevation requested  reason=screen-capture mechanism=<platform>"
    → calls platform_execute()   (OS prompt or stub)
    → logs "elevation outcome    reason=screen-capture mechanism=<platform> outcome=<…>"
    → returns ElevationOutcome   (#[must_use])

caller matches outcome {
    Granted    → proceed with the privileged operation
    Denied     → surface the denial to the user; do not retry
    Unavailable → degrade gracefully; do not retry with a different mechanism
}
```

The `#[must_use]` annotation on `ElevationOutcome` and the `#[non_exhaustive]`
on `EscalationReason` together ensure:

- New privilege surfaces require a code change (adding a new `EscalationReason`
  variant forces a re-review).
- Callers cannot silently discard an `Unavailable` or `Denied` result.
