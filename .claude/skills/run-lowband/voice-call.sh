#!/usr/bin/env bash
# LowBand voice-call launcher.
#
# Turns "establish a session between two peers" into a one-liner. Three modes:
#
#   voice-call.sh                 local self-test: spins up signaling + a host
#                                 daemon + a join daemon on this box (loopback),
#                                 verifies BOTH reach "secure channel
#                                 established", then tears down. Proves the
#                                 join-code → E2EE voice channel path end to end
#                                 through the real daemon binaries. Runs on a
#                                 headless box (no audio hardware needed).
#
#   voice-call.sh host            host role for a REAL call: creates a join code
#                                 and prints it, then runs the daemon in the
#                                 foreground for the call's lifetime.
#
#   voice-call.sh join <code>     join role: enters <code> and connects.
#
# For a real audible call, run `host` on one machine and `join <code>` on the
# other, both pointing at the same signaling server, and build with AUDIO=1
# (needs the system audio lib on each machine — libasound2-dev / CoreAudio /
# WASAPI — plus a real mic + speaker). Without AUDIO the daemon establishes the
# encrypted channel but runs the receive-only worker instead of the mic↔speaker
# voice loop.
#
# Env:
#   SIGNALING_URL   host/join modes: rendezvous server as HOST:PORT (required
#                   for a real two-machine call, e.g. 192.0.2.10:3478). Not an
#                   http:// URL — the daemon dials it as a socket address.
#   SIGNALING_PORT  local mode: port for the throwaway signaling server (3478).
#   AUDIO=1         build + run with `--features audio` (real mic/speaker loop).
#   OPUS=1          also enable `--features opus` (production codec; needs
#                   libopus). Implies the C toolchain is present.
#   STUN=host:port  publish a STUN-reflexive candidate (needed across NATs /
#                   between two real machines not on the same host).
#   KEEP=1          local mode: keep both daemons running after the channel is
#                   up (Ctrl-C to stop) instead of tearing down.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO"   # repo .cargo/config.toml (musl target + rust-lld) must apply
export PATH="$HOME/.cargo/bin:$REPO/bin:$PATH"

BIN="$REPO/target/x86_64-unknown-linux-musl/debug"

# ── feature selection ───────────────────────────────────────────────────────
FEATURES=()
[ "${AUDIO:-0}" = 1 ] && FEATURES+=(audio)
[ "${OPUS:-0}" = 1 ] && FEATURES+=(opus)
FEATURE_ARGS=()
if [ ${#FEATURES[@]} -gt 0 ]; then
  FEATURE_ARGS=(--features "${FEATURES[*]}")
fi

STUN_ARGS=()
[ -n "${STUN:-}" ] && STUN_ARGS=(--stun "$STUN")

fail() { echo "FAIL: $*" >&2; exit 1; }

build_daemon() {
  echo "── build lowbandd ${FEATURES[*]:+(features: ${FEATURES[*]})}"
  cargo build -p lowbandd "${FEATURE_ARGS[@]}"
}

MODE="${1:-local}"

case "$MODE" in
# ── real-call roles: one command per machine ────────────────────────────────
host)
  [ -n "${SIGNALING_URL:-}" ] || fail "set SIGNALING_URL to your signaling server as HOST:PORT (e.g. 192.0.2.10:3478)"
  build_daemon
  echo "── hosting a call via $SIGNALING_URL — share the printed join code"
  exec "$BIN/lowbandd" \
    --ipc-socket "${IPC_SOCKET:-/tmp/lowbandd-host.sock}" \
    --data-dir "${DATA_DIR:-/tmp/lowbandd-host-data}" \
    --signaling "$SIGNALING_URL" --host "${STUN_ARGS[@]}"
  ;;
join)
  CODE="${2:-}"
  [ -n "$CODE" ] || fail "usage: voice-call.sh join <code>"
  [ -n "${SIGNALING_URL:-}" ] || fail "set SIGNALING_URL to your signaling server as HOST:PORT (e.g. 192.0.2.10:3478)"
  build_daemon
  echo "── joining call $CODE via $SIGNALING_URL"
  exec "$BIN/lowbandd" \
    --ipc-socket "${IPC_SOCKET:-/tmp/lowbandd-join.sock}" \
    --data-dir "${DATA_DIR:-/tmp/lowbandd-join-data}" \
    --signaling "$SIGNALING_URL" --join "$CODE" "${STUN_ARGS[@]}"
  ;;
# ── local self-test: both peers on this box over loopback ───────────────────
local)
  PORT="${SIGNALING_PORT:-3478}"
  SIG_ADDR="127.0.0.1:$PORT"       # daemon dials this as a socket address
  BASE="http://$SIG_ADDR"          # curl health check needs the http:// form
  WORK="$(mktemp -d)"
  PIDS=()
  cleanup() { [ ${#PIDS[@]} -gt 0 ] && kill "${PIDS[@]}" 2>/dev/null; rm -rf "$WORK"; }
  trap cleanup EXIT

  echo "── build lowbandd + signaling ${FEATURES[*]:+(features: ${FEATURES[*]})}"
  cargo build -p lowbandd -p lowband-signaling "${FEATURE_ARGS[@]}"

  echo "── launch signaling on :$PORT"
  SIGNALING_BIND="127.0.0.1:$PORT" TURN_SHARED_SECRET=voice-demo \
    SIGNALING_DB="$WORK/signaling-db" \
    "$BIN/lowband-signaling" >"$WORK/signaling.log" 2>&1 &
  PIDS+=($!)
  for _ in $(seq 1 50); do
    curl -sf -X POST "$BASE/signal/session" -o /dev/null && break
    sleep 0.1
  done

  echo "── launch host daemon (creates the join code)"
  "$BIN/lowbandd" --ipc-socket "$WORK/host.sock" --data-dir "$WORK/host-data" \
    --signaling "$SIG_ADDR" --host "${STUN_ARGS[@]}" >"$WORK/host.log" 2>&1 &
  PIDS+=($!)

  # Read the join code the host published to its log.
  CODE=""
  for _ in $(seq 1 100); do
    CODE=$(sed -n 's/.*join code: \([0-9]*\).*/\1/p' "$WORK/host.log" | head -1)
    [ -n "$CODE" ] && break
    sleep 0.1
  done
  [ -n "$CODE" ] || fail "host never printed a join code ($(cat "$WORK/host.log"))"
  echo "   join code = $CODE"

  echo "── launch join daemon with that code"
  "$BIN/lowbandd" --ipc-socket "$WORK/join.sock" --data-dir "$WORK/join-data" \
    --signaling "$SIG_ADDR" --join "$CODE" "${STUN_ARGS[@]}" >"$WORK/join.log" 2>&1 &
  PIDS+=($!)

  # Wait for BOTH peers to report the encrypted channel is up.
  established() { grep -q 'secure channel established' "$1"; }
  failed() { grep -q 'session establishment failed' "$1"; }
  for role in host join; do
    log="$WORK/$role.log"
    for _ in $(seq 1 300); do
      established "$log" && break
      failed "$log" && fail "$role establishment failed: $(grep 'session establishment failed' "$log")"
      sleep 0.1
    done
    established "$log" || fail "$role never established ($(tail -3 "$log"))"
  done

  echo "   host:  $(grep 'secure channel established' "$WORK/host.log")"
  echo "   join:  $(grep 'secure channel established' "$WORK/join.log")"
  if [ ${#FEATURES[@]} -gt 0 ] && printf '%s\n' "${FEATURES[@]}" | grep -qx audio; then
    echo "   (audio feature on — each daemon is running the mic↔speaker voice loop"
    echo "    if this box has audio hardware; check {host,join}.log for device errors)"
  else
    echo "   (no audio feature — channel is up; build with AUDIO=1 for the voice loop)"
  fi

  echo "PASS: join-code → E2EE channel established between two daemons on loopback"

  if [ "${KEEP:-0}" = 1 ]; then
    echo "── KEEP=1: daemons still running (Ctrl-C to stop)"
    wait
  fi
  ;;
*)
  fail "unknown mode '$MODE' (use: local | host | join <code>)"
  ;;
esac
