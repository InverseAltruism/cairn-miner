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
# worst case one card mines, never a bricked rig. No set -e anywhere.
#
# Each worker exposes a loopback stats port (BASE + device index); h-stats.sh
# scrapes them via `cairn-miner hiveos-stats`. We pass --stats-port ONLY: the
# stats server always binds 127.0.0.1, and there is no --stats-bind flag (passing
# one makes clap exit 2 and crash-loops the rig — the historical HiveOS bug).
#
# Deliberately NO relay node and NO phone-home: cairn-miner ships nothing that
# follows a remote blacklist or reports your rig anywhere.
cd "$(dirname "$0")" || exit 1
[ -e h-manifest.conf ] && . ./h-manifest.conf

BIN="$(pwd)/cairn-miner"
CONF="${CUSTOM_CONFIG_FILENAME:-$(pwd)/config.toml}"
LOG="${CUSTOM_LOG_BASENAME:-/var/log/miner/cairn-miner/cairn-miner}.log"
LOGDIR="$(dirname "$LOG")"
mkdir -p "$LOGDIR"
EXTRA="${CUSTOM_USER_CONFIG:-}"
BASE_PORT="${CUSTOM_API_PORT:-3380}"
PIDFILE="$(pwd)/.cairn-sup.pids"
: > "$PIDFILE"

# --stats-port <BASE+dev> if this binary advertises the flag; empty otherwise
# (an older binary without it just mines, no stats endpoint).
HAS_STATS=0
if "$BIN" --help 2>/dev/null | grep -q -- '--stats-port'; then
  HAS_STATS=1
fi
stats_arg() { # $1 = device index
  [ "$HAS_STATS" = 1 ] && printf -- '--stats-port %s' "$((BASE_PORT + $1))"
}

# Count NVIDIA GPUs (the overwhelming majority of HiveOS rigs). timeout-guarded
# so a hung driver can't wedge startup; 0 / nvidia-smi absent → single process.
gpu_count() {
  command -v nvidia-smi >/dev/null 2>&1 || { echo 0; return; }
  local n
  n=$(timeout 15 nvidia-smi -L 2>/dev/null | grep -c '^GPU ') || n=0
  echo "${n:-0}"
}

# If the operator pinned a card or forced the CPU backend, honour it with ONE
# process — don't second-guess them, don't multi-spawn.
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
# supervisor PIDs so h-stop.sh can stop them first (killing only the miner would
# let the supervisor loop respawn it).
if [ "$SINGLE" = 0 ] && [ "$NGPU" -gt 1 ]; then
  i=1
  while [ "$i" -lt "$NGPU" ]; do
    dev="$i"
    (
      while :; do
        "$BIN" --config "$CONF" --device "$dev" --log-dir "$LOGDIR" $(stats_arg "$dev") \
          >> "${LOG%.log}.gpu${dev}.log" 2>&1
        # Worker exited (crash, or a device-fault self-exit for restart) — pause
        # briefly, then relaunch just this card.
        sleep 3
      done
    ) &
    echo "$!" >> "$PIDFILE"
    i=$((i + 1))
  done
fi

# Foreground process. Truncate the main log so it can't grow unbounded across
# restarts (HiveOS's "miner log" button reads this file). `exec` makes
# cairn-miner THIS process, satisfying HiveOS's exec-rename contract.
: > "$LOG"
if [ "$SINGLE" = 1 ]; then
  exec "$BIN" --config "$CONF" --log-dir "$LOGDIR" $(stats_arg 0) $EXTRA >> "$LOG" 2>&1
else
  exec "$BIN" --config "$CONF" --device 0 --log-dir "$LOGDIR" $(stats_arg 0) $EXTRA >> "$LOG" 2>&1
fi
