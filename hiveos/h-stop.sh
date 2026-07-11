#!/usr/bin/env bash
# cairn-miner - HiveOS stop hook. HiveOS calls this to stop the slot.
#
# Order matters: kill the per-GPU restart SUPERVISORS first (from the pidfile
# h-run.sh wrote), otherwise a supervisor would just respawn the miner we're
# trying to stop. Then stop the miner processes by binary name (their argv is
# `.../cairn-miner` because h-run.sh exec's/launches the real binary).
cd "$(dirname "$0")" 2>/dev/null || true
PIDFILE="$(pwd)/.cairn-sup.pids"

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
pkill -TERM -f '/cairn-miner( |$)' 2>/dev/null
for _ in 1 2 3 4 5; do pgrep -f '/cairn-miner( |$)' >/dev/null 2>&1 || exit 0; sleep 1; done
pkill -KILL -f '/cairn-miner( |$)' 2>/dev/null
exit 0
