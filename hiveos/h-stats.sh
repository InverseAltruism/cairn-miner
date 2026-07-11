#!/usr/bin/env bash
# cairn-miner - HiveOS stats hook. Sourced by the HiveOS agent every ~10s; it
# must set two shell vars: `khs` (total kH/s) and `stats` (JSON in HiveOS's
# shape). We delegate ALL aggregation + unit math to the miner itself
# (`cairn-miner hiveos-stats`), which scrapes each per-GPU worker's loopback
# /stats port and emits the exact JSON. That keeps this hook trivial and the
# arithmetic (the H/s->kH/s divisor, per-card sums) in one unit-tested place —
# the source of past "online but 0 H/s" reports was doing it here in shell.
cd "$(dirname "$0")" 2>/dev/null || true
BIN="$(pwd)/cairn-miner"
BASE_PORT="${CUSTOM_API_PORT:-3380}"

khs=0
stats='{}'

# Number of GPU worker slots (matches h-run.sh's per-GPU spawn). Best-effort;
# hiveos-stats also auto-probes if we can't determine it.
GPUS_ARG=""
if command -v nvidia-smi >/dev/null 2>&1; then
  ngpu=$(timeout 10 nvidia-smi -L 2>/dev/null | grep -c '^GPU ')
  [ -n "$ngpu" ] && [ "$ngpu" -gt 0 ] 2>/dev/null && GPUS_ARG="--gpus $ngpu"
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
