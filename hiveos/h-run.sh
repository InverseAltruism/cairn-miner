#!/usr/bin/env bash
# cairn-miner - HiveOS run hook.
#
# HiveOS requires the miner process argv to be the real binary (not h-run.sh),
# so the LAST thing this script does is `exec` cairn-miner, replacing this shell.
# One cairn-miner process drives exactly ONE GPU (CUDA/OpenCL are single-device),
# so on a multi-GPU rig we first launch one restart-supervised worker per
# additional card in the background, then `exec` device 0 as the foreground
# process. If GPU detection fails, or the operator pinned a --device / forced
# --backend cpu in "Extra config args", we fall through to a single process —
# worst case one card mines, never a bricked rig. No `set -e` anywhere.
#
# Each worker exposes a loopback stats port (BASE + device index); h-stats.sh
# scrapes them via `cairn-miner hiveos-stats`. We pass --stats-port ONLY (the
# stats server always binds 127.0.0.1). Do NOT add a stats "bind" flag — the CLI
# defines no such flag and passing an unknown one makes clap exit 2 and
# crash-loops the rig (the historical HiveOS bug this file fixes).
#
# Deliberately NO relay node and NO phone-home: cairn-miner ships nothing that
# follows a remote blacklist or reports your rig anywhere.

# Resolve our own directory whether this hook is executed OR sourced.
SELF="${BASH_SOURCE[0]:-$0}"
cd "$(dirname "$(readlink -f "$SELF" 2>/dev/null || echo "$SELF")")" || exit 1
[ -e h-manifest.conf ] && . ./h-manifest.conf

BIN="$(pwd)/cairn-miner"
CONF="${CUSTOM_CONFIG_FILENAME:-$(pwd)/config.toml}"
LOG="${CUSTOM_LOG_BASENAME:-/var/log/miner/cairn-miner/cairn-miner}.log"
LOGDIR="$(dirname "$LOG")"
mkdir -p "$LOGDIR" 2>/dev/null || true
EXTRA="${CUSTOM_USER_CONFIG:-}"
BASE_PORT="${CUSTOM_API_PORT:-3380}"
PIDFILE="$(pwd)/.cairn-sup.pids"

# A re-run without an intervening h-stop would orphan the previous supervisors
# (they'd keep respawning miners we no longer track). Stop any recorded ones,
# then reset the pidfile.
if [ -f "$PIDFILE" ]; then
  while read -r p; do [ -n "$p" ] && kill -TERM "$p" 2>/dev/null; done < "$PIDFILE"
fi
: > "$PIDFILE"

# Does this binary advertise --stats-port? timeout-guarded so a hung/wrong-arch
# binary can't wedge startup at the probe.
HAS_STATS=0
if timeout 10 "$BIN" --help 2>/dev/null | grep -q -- '--stats-port'; then
  HAS_STATS=1
fi

# Whether the operator's Extra args already carry a flag (so we don't inject it
# twice → clap "cannot be used multiple times" → exit 2 → crash-loop).
extra_has() { case " $EXTRA " in *" $1"*) return 0 ;; esac; return 1; }

# Build a worker's argv into the global array WARGS. $1 = device index, or empty
# to omit --device (a pinned/single process). EXTRA is appended verbatim (word-
# split on purpose); paths are quoted via the array so spaces are safe.
build_args() {
  WARGS=()
  extra_has --config  || WARGS+=(--config "$CONF")
  [ -n "$1" ] && WARGS+=(--device "$1")
  extra_has --log-dir || WARGS+=(--log-dir "$LOGDIR")
  if [ "$HAS_STATS" = 1 ] && ! extra_has --stats-port; then
    WARGS+=(--stats-port "$((BASE_PORT + ${1:-0}))")
  fi
  # shellcheck disable=SC2206
  [ -n "$EXTRA" ] && WARGS+=($EXTRA)
}

# Count GPUs: nvidia-smi first (NVIDIA majority), else ask the miner itself
# (covers AMD/OpenCL rigs + a flaky nvidia-smi). Always a non-negative integer.
gpu_count() {
  local n=""
  if command -v nvidia-smi >/dev/null 2>&1; then
    n=$(timeout 15 nvidia-smi -L 2>/dev/null | grep -c '^GPU ') || n=""
  fi
  if { [ -z "$n" ] || [ "$n" = 0 ]; } && command -v jq >/dev/null 2>&1; then
    n=$(timeout 15 "$BIN" devices --json 2>/dev/null | jq '.gpus | length' 2>/dev/null) || n=""
  fi
  case "$n" in ''|*[!0-9]*) n=0 ;; esac
  echo "$n"
}

# Operator pinned a card or forced CPU → honour it with ONE process.
SINGLE=0
case "$EXTRA" in
  *--device*|*"--backend cpu"*|*--backend=cpu*) SINGLE=1 ;;
esac

NGPU=1
if [ "$SINGLE" = 0 ]; then
  NGPU="$(gpu_count)"
  [ "$NGPU" -ge 1 ] 2>/dev/null || NGPU=1
fi

# Multi-GPU: background a restart-supervised worker for cards 1..N-1. Record the
# supervisor PIDs so h-stop.sh stops them first (killing only the miner would let
# the supervisor respawn it). Per-worker log is truncated each restart and the
# restart backs off (capped) so a permanently-faulting card can't fill the disk.
if [ "$SINGLE" = 0 ] && [ "$NGPU" -gt 1 ]; then
  i=1
  while [ "$i" -lt "$NGPU" ]; do
    dev="$i"
    WLOG="${LOG%.log}.gpu${dev}.log"
    build_args "$dev"
    wargs=("${WARGS[@]}")
    (
      fails=0
      while :; do
        : > "$WLOG" 2>/dev/null || WLOG=/dev/null
        "$BIN" "${wargs[@]}" >> "$WLOG" 2>&1
        # Worker exited (crash, or a device-fault self-exit for restart). Back
        # off, growing on repeated fast failures (cap 30s) so a dead card doesn't
        # hot-loop the CPU/disk.
        fails=$((fails + 1)); [ "$fails" -gt 10 ] && fails=10
        sleep $((3 * fails))
      done
    ) &
    echo "$!" >> "$PIDFILE"
    i=$((i + 1))
  done
fi

# Foreground process (device 0). Guarantee a writable redirect target so the exec
# can't fail-and-abort (would crash-loop a single-GPU rig); truncate the main log
# once so it stays bounded across restarts (HiveOS's "miner log" button reads it).
if ! : >> "$LOG" 2>/dev/null; then LOG=/dev/null; fi
[ "$LOG" != /dev/null ] && : > "$LOG" 2>/dev/null
if [ "$SINGLE" = 1 ]; then
  build_args ""      # no --device injected; the pin / --backend cpu decides
else
  build_args 0
fi
exec "$BIN" "${WARGS[@]}" >> "$LOG" 2>&1
