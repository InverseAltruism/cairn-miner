#!/usr/bin/env bash
# cairn-miner - HiveOS stop hook. HiveOS calls this to stop the slot. We stop the
# miner gently (SIGTERM -> SIGKILL) by binary name; the process argv is
# `cairn-miner` because h-run.sh exec's it.
cd "$(dirname "$0")" || exit 0
pkill -TERM -f '/cairn-miner( |$)' 2>/dev/null
for _ in 1 2 3 4 5; do pgrep -f '/cairn-miner( |$)' >/dev/null 2>&1 || exit 0; sleep 1; done
pkill -KILL -f '/cairn-miner( |$)' 2>/dev/null
exit 0
