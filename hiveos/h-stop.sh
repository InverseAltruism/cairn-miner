#!/usr/bin/env bash
# cairn-miner - HiveOS stop hook. HiveOS calls this to stop the slot.
#
# Order matters: kill the per-GPU restart SUPERVISORS first (from the pidfile
# h-run.sh wrote), otherwise a supervisor would just respawn the miner we're
# trying to stop. Then stop the miner processes. The pattern matches a mining
# argv ("/cairn-miner --…") so it hits the foreground process and every per-GPU
# worker, but NOT the short-lived "cairn-miner hiveos-stats" scrape helper and
# NOT the supervisor subshell (whose cmdline is ".../cairn-miner/h-run.sh").
SELF="${BASH_SOURCE[0]:-$0}"
cd "$(dirname "$(readlink -f "$SELF" 2>/dev/null || echo "$SELF")")" 2>/dev/null || true
PIDFILE="$(pwd)/.cairn-sup.pids"
MATCH='/cairn-miner --'

# 1) Stop the background supervisor subshells so they can't relaunch a card.
if [ -f "$PIDFILE" ]; then
  while read -r pid; do
    [ -n "$pid" ] && kill -TERM "$pid" 2>/dev/null
  done < "$PIDFILE"
  sleep 1
  while read -r pid; do
    [ -n "$pid" ] && kill -KILL "$pid" 2>/dev/null
  done < "$PIDFILE"
  rm -f "$PIDFILE"
fi

# 2) Stop the miner processes (foreground device 0 + any per-GPU children).
pkill -TERM -f "$MATCH" 2>/dev/null
for _ in 1 2 3 4 5; do pgrep -f "$MATCH" >/dev/null 2>&1 || exit 0; sleep 1; done
pkill -KILL -f "$MATCH" 2>/dev/null
exit 0
