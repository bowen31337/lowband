#!/usr/bin/env bash
# LowBand end-to-end smoke driver.
#
# Builds both binaries, launches the signaling service, drives the full
# two-peer rendezvous flow over HTTP, then launches the lowbandd daemon and
# connects the shell-probe IPC client to check governor events arrive.
#
# Usage:  .claude/skills/run-lowband/smoke.sh
# Env:    SIGNALING_PORT (default 3478) — override if the port is taken.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO"   # repo .cargo/config.toml (musl target + rust-lld) must apply
export PATH="$HOME/.cargo/bin:$REPO/bin:$PATH"

PORT="${SIGNALING_PORT:-3478}"
BASE="http://127.0.0.1:$PORT"
WORK="$(mktemp -d)"
SOCK="$WORK/lowband.sock"
PIDS=()
cleanup() { [ ${#PIDS[@]} -gt 0 ] && kill "${PIDS[@]}" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

echo "── build (first build takes ~1 min; incremental ~15 s)"
cargo build -p lowbandd -p lowband-signaling
cargo build --manifest-path .claude/skills/run-lowband/shell-probe/Cargo.toml
BIN="$REPO/target/x86_64-unknown-linux-musl/debug"
PROBE="$REPO/.claude/skills/run-lowband/shell-probe/target/x86_64-unknown-linux-musl/debug/shell-probe"

echo "── launch signaling on :$PORT"
SIGNALING_BIND="127.0.0.1:$PORT" TURN_SHARED_SECRET=smoke-secret \
  "$BIN/lowband-signaling" >"$WORK/signaling.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do
  curl -sf -X POST "$BASE/signal/session" -o /dev/null && break
  sleep 0.1
done

echo "── rendezvous flow: two peers"
CODE=$(curl -sS -X POST "$BASE/signal/session" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["session_code"])')
[ -n "$CODE" ] || fail "no session code"
echo "   session_code=$CODE"

curl -sf "$BASE/signal/join/$CODE" >/dev/null || fail "join"
curl -sf -X POST "$BASE/signal/offer" -H 'content-type: application/json' \
  -d "{\"session_code\":\"$CODE\",\"sdp\":\"v=lowband-offer\"}" >/dev/null || fail "post offer"
curl -sf "$BASE/signal/join/$CODE" | grep -q 'lowband-offer' || fail "joiner did not see offer"
curl -sf -X POST "$BASE/signal/answer" -H 'content-type: application/json' \
  -d "{\"session_code\":\"$CODE\",\"sdp\":\"v=lowband-answer\"}" >/dev/null || fail "post answer"
curl -sf -X POST "$BASE/signal/candidate" -H 'content-type: application/json' \
  -d "{\"session_code\":\"$CODE\",\"candidate\":\"candidate:1 1 UDP 2130706431 192.0.2.10 40001 typ host\"}" \
  >/dev/null || fail "post candidate"
curl -sf "$BASE/signal/join/$CODE" | grep -q 'candidate:1' || fail "candidate not visible"
curl -sf -X POST "$BASE/signal/turn" | grep -q 'turn_credential' || fail "turn credential"
curl -sf -X POST "$BASE/signal/connected" -H 'content-type: application/json' \
  -d "{\"session_code\":\"$CODE\"}" >/dev/null || fail "connected"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/signal/join/$CODE")
[ "$STATUS" = 404 ] || fail "consumed code should 404, got $STATUS"
echo "   offer/answer/ICE/TURN exchanged; code consumed (404 on reuse) ✓"

echo "── launch lowbandd + connect IPC probe shell"
"$BIN/lowbandd" --ipc-socket "$SOCK" --data-dir "$WORK/data" --link-bps 150000 \
  >"$WORK/lowbandd.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || fail "IPC socket never appeared ($(cat "$WORK/lowbandd.log"))"

# 9 events = three full TierUpdate/StreamBudget/GearUpdate governor cycles
OUT=$("$PROBE" "$SOCK" 9)
echo "$OUT" | grep -q 'TierUpdate'   || fail "no TierUpdate event"
echo "$OUT" | grep -q 'StreamBudget' || fail "no StreamBudget event"
echo "$OUT" | grep -q 'GearUpdate'   || fail "no GearUpdate event"
echo "$OUT" | sed 's/^/   /' | head -4
echo "   governor events flowing over IPC ✓"

echo "PASS: signaling rendezvous + daemon IPC both working end-to-end"
