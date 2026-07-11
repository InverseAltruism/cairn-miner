#!/usr/bin/env bash
# cairn-miner - HiveOS stats hook. SOURCED by the HiveOS agent every ~10s; it
# must set two shell vars: `khs` (total kH/s) and `stats` (JSON in HiveOS's
# shape). We delegate ALL aggregation + unit math to the miner itself
# (`cairn-miner hiveos-stats`), which scrapes each per-GPU worker's loopback
# /stats port and emits the exact JSON. That keeps this hook trivial and the
# arithmetic (the H/s->kH/s divisor, per-card sums) in one unit-tested place —
# doing it here in shell was the source of past "online but 0 H/s" reports.
#
# NOTE this file is SOURCED, so $0 is the agent, not this script — resolve our
# own path via BASH_SOURCE to find the binary + pidfile.
SELF="${BASH_SOURCE[0]:-$0}"
cd "$(dirname "$(readlink -f "$SELF" 2>/dev/null || echo "$SELF")")" 2>/dev/null || true
BIN="$(pwd)/cairn-miner"
BASE_PORT="${CUSTOM_API_PORT:-3380}"
PIDFILE="$(pwd)/.cairn-sup.pids"

khs=0
stats='{}'

# Worker count = (supervised background workers in the pidfile) + 1 foreground,
# so we scrape exactly the ports h-run.sh spawned (no phantom zero-cards in
# single/CPU mode). No pidfile → let hiveos-stats auto-probe.
GPUS_ARG=""
if [ -f "$PIDFILE" ]; then
  n=$(grep -c . "$PIDFILE" 2>/dev/null)
  case "$n" in ''|*[!0-9]*) n=0 ;; esac
  GPUS_ARG="--gpus $((n + 1))"
fi

if [ -x "$BIN" ]; then
  out=$("$BIN" hiveos-stats --stats-port "$BASE_PORT" $GPUS_ARG 2>/dev/null || true)
  if [ -n "$out" ] && command -v jq >/dev/null 2>&1; then
    k=$(printf '%s' "$out" | jq -r '.khs // 0' 2>/dev/null)
    s=$(printf '%s' "$out" | jq -c '.stats // {}' 2>/dev/null)
    [ -n "$k" ] && [ "$k" != "null" ] && khs="$k"
    [ -n "$s" ] && [ "$s" != "null" ] && stats="$s"
  fi
fi

echo "$khs"
