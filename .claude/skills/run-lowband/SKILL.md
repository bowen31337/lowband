---
name: run-lowband
description: Build, run, and drive LowBand. Use when asked to start lowbandd or the signaling service, run the app end-to-end, smoke-test it, run its tests, or interact with the daemon over IPC.
---

LowBand is two Rust binaries: `lowband-signaling` (axum HTTP rendezvous on
`/signal/*`) and `lowbandd` (governor daemon broadcasting events over a Unix
IPC socket at 10 Hz). Drive both with one command:
`.claude/skills/run-lowband/smoke.sh`. The daemon's client handle is
`shell-probe` (in this skill dir) — a stand-in UI shell that connects to the
IPC socket and prints events.

All paths are relative to the repo root. **Ignore `run.sh` at the repo root**
— it is a stale leftover from an unrelated Python project and runs nothing in
this repo.

## Prerequisites

Nothing to apt-get. The box already has rustup (stable-gnu toolchain) and the
repo vendors its own musl linker. Two PATH entries are mandatory — `cargo` is
NOT on the default shell PATH here, and `.cargo/config.toml` names `rust-lld`
as the musl linker, which resolves via the repo's `bin/`:

```bash
export PATH="$HOME/.cargo/bin:$PWD/bin:$PATH"   # from repo root
```

## Build

Default target is `x86_64-unknown-linux-musl` (set in `.cargo/config.toml`);
binaries land in `target/x86_64-unknown-linux-musl/debug/`.

```bash
cargo build -p lowbandd -p lowband-signaling
```

## Run (agent path)

One script builds everything, launches both binaries, drives the full
two-peer rendezvous (session → join → offer → answer → ICE → TURN →
connected → reuse-must-404), then connects `shell-probe` to the daemon's IPC
socket and asserts TierUpdate/StreamBudget/GearUpdate events arrive. Cleans
up after itself; prints `PASS` on success.

```bash
.claude/skills/run-lowband/smoke.sh                        # ~15 s warm, ~1 min cold
SIGNALING_PORT=3500 .claude/skills/run-lowband/smoke.sh    # if :3478 is taken
```

To drive the pieces by hand instead:

```bash
# signaling service (env: SIGNALING_BIND, TURN_SHARED_SECRET, SIGNALING_DB)
SIGNALING_BIND=127.0.0.1:3478 TURN_SHARED_SECRET=dev SIGNALING_DB=/tmp/lb-signaling-db \
  target/x86_64-unknown-linux-musl/debug/lowband-signaling &
curl -s -X POST http://127.0.0.1:3478/signal/session   # → {"session_code":"..."}

# daemon + IPC probe (probe args: <socket-path> [event-count, default 9])
target/x86_64-unknown-linux-musl/debug/lowbandd \
  --ipc-socket /tmp/lb.sock --data-dir /tmp/lb-data --link-bps 150000 &
cargo build --manifest-path .claude/skills/run-lowband/shell-probe/Cargo.toml
.claude/skills/run-lowband/shell-probe/target/x86_64-unknown-linux-musl/debug/shell-probe /tmp/lb.sock 9
```

Both binaries stop cleanly on SIGTERM (`kill %1 %2`).

## Test

```bash
cargo test --workspace          # 2700 pass, 0 fail (verified 2026-07-10)
cargo test -p lowband-e2e       # just the e2e suite: signaling + uc1–uc3, 22 tests
```

## Gotchas

- **Do NOT use `./cargo-test.sh` / `./cargo-test-lbtp.sh` wrappers.** They
  export `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=bin/rust-lld`, which
  overrides the committed `.cargo/config.toml` (gnu → `cc`) and produces
  build scripts that SIGSEGV (`failed to run custom build command for libc
  … SIGSEGV`). Plain `cargo test` works for every crate; the wrappers
  predate the config fix.
- **`shell-probe` is deliberately outside the workspace** (own `[workspace]`
  table). Build it with `--manifest-path`; run the build from repo root so
  the repo's `.cargo/config.toml` still applies (config is discovered from
  cwd, not manifest path).
- **`.cargo/config.toml` hard-codes `-L/tmp/rust-libs`** for host-target
  links. That directory exists on this box (stub `.so`s for libc/libm/…).
  If a fresh machine lacks it, host-target link steps will fail — recreate
  or drop the `[host]` rustflags line.
- **The default `SIGNALING_DB=:memory:` is NOT in-memory.** sled has no
  `:memory:` magic string — `sled::open(":memory:")` creates a literal
  `:memory:/` directory (with a persistent db) in the cwd. Always set
  `SIGNALING_DB` to a real path (smoke.sh uses its temp workdir); if you
  see a stray `:memory:/` directory in the repo, it's this — safe to `rm
  -rf ':memory:'`.
- **Session codes are sequential** (`100000000`, `100000001`, …) — fine
  for smoke runs, don't treat codes as unguessable in tests.

## Troubleshooting

- **`Command 'cargo' not found`**: this shell doesn't source the rustup
  profile. `export PATH="$HOME/.cargo/bin:$PATH"`.
- **`lowband-signaling: bind 127.0.0.1:3478: Address already in use`**
  (exits immediately, curl then fails): another run is still alive. `pkill
  lowband-signaling` or rerun with `SIGNALING_PORT=<other>`.
- **`lowbandd: bind /tmp/lb.sock: Address already in use`**: stale socket
  file from a killed daemon. `rm` the socket path and relaunch.
- **`TURN_SHARED_SECRET not set; credentials will be insecure`** on
  signaling stderr: expected in dev; set the env var to silence.
