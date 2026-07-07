#!/usr/bin/env bash
# cairn-miner - HiveOS run hook.
#
# HiveOS requires the miner process argv to be the real binary (not h-run.sh),
# so we `exec` it, replacing this shell. stdout/stderr are redirected (not
# piped) to the HiveOS log so the "miner log" button works AND the exec-rename
# contract is honoured. One process drives all same-vendor GPUs; for a mixed or
# multi-GPU rig pass --device N in Extra config args (or run one slot per card).
#
# Deliberately NO relay node and NO phone-home: cairn-miner ships nothing that
# follows a remote blacklist or reports your rig anywhere. It mines and submits
# shares, full stop.
cd "$(dirname "$0")" || exit 1
[ -e h-manifest.conf ] && . ./h-manifest.conf

BIN="$(pwd)/cairn-miner"
CONF="${CUSTOM_CONFIG_FILENAME:-$(pwd)/config.toml}"
LOG="${CUSTOM_LOG_BASENAME:-/var/log/miner/cairn-miner/cairn-miner}.log"
mkdir -p "$(dirname "$LOG")"
EXTRA="${CUSTOM_USER_CONFIG:-}"

# Optional local stats endpoint (bound to loopback only) so h-stats.sh can scrape
# structured stats instead of parsing the log. Harmless if the running binary
# doesn't support it yet (unknown flags are rejected, so only pass it when the
# binary advertises it).
STATS_ARGS=""
if "$BIN" --help 2>/dev/null | grep -q -- '--stats-port'; then
  STATS_ARGS="--stats-port ${CUSTOM_API_PORT:-3380} --stats-bind 127.0.0.1"
fi

exec "$BIN" --config "$CONF" --log-dir "$(dirname "$LOG")" $STATS_ARGS $EXTRA >> "$LOG" 2>&1
