#!/usr/bin/env bash
set -euo pipefail
# cairn-miner - self-updating launcher (Linux).
#
# Runs the miner and, in the background, keeps it current: every CHECK_MIN
# minutes it asks GitHub for the latest release, and IF a newer version is
# published it downloads the matching asset to a TEMP file, verifies its SHA-256
# against the release's SHA256SUMS, and only then atomically swaps it in and
# restarts the miner. FAIL-SAFE IS THE RULE: any failure (no network, missing
# checksum, mismatch, partial download) is discarded and the running binary is
# kept — the rig never bricks and never runs an unverified binary.
#
# Usage:  ./mine-auto.sh [nvidia|amd|cpu] [addr20]
#   or set CAIRN_ADDR / CAIRN_BACKEND / CAIRN_POOL in the environment.

REPO="InverseAltruism/cairn-miner"
CHECK_MIN="${CAIRN_CHECK_MIN:-30}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/cairn-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/cairn-miner"
BIN="$DATA_DIR/cairn-miner"
CFG="$CFG_DIR/config.toml"
mkdir -p "$DATA_DIR" "$CFG_DIR"

VARIANT="${1:-${CAIRN_BACKEND:-}}"
ADDR="${2:-${CAIRN_ADDR:-}}"
ARCH="$(uname -m)"

detect_variant() {
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then echo cuda
  elif { command -v lspci >/dev/null 2>&1 && lspci 2>/dev/null | grep -Eiq 'VGA.*(AMD|ATI)|Radeon'; }; then echo opencl
  else echo cpu; fi
}
[ -z "$VARIANT" ] && VARIANT="$(detect_variant)"
case "$VARIANT" in nvidia) VARIANT=cuda;; amd) VARIANT=opencl;; esac
ASSET="cairn-miner-linux-${VARIANT}-${ARCH}"

dl() { # url out
  local t="$2.tmp"
  if command -v curl >/dev/null 2>&1; then curl -fsSL --retry 3 -o "$t" "$1" && mv "$t" "$2"
  else wget -qO "$t" "$1" && mv "$t" "$2"; fi
}

# Bootstrap: if there's no binary yet, run the installer path once.
if [ ! -x "$BIN" ]; then
  echo "[mine-auto] no binary yet; installing $ASSET ..."
  CAIRN_ADDR="$ADDR" CAIRN_BACKEND="$VARIANT" bash "$(dirname "$0")/install.sh" --no-run || {
    echo "[mine-auto] install failed"; exit 1; }
fi

installed_version() { "$BIN" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1; }
latest_version() { dl "https://github.com/${REPO}/releases/latest/download/latest-version.txt" /dev/stdout 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1; }
# numeric semver >: is $1 strictly newer than $2 ?
newer() { [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]; }

try_update() {
  local latest cur tmp sums
  latest="$(latest_version)" || return 0
  [ -z "$latest" ] && return 0
  cur="$(installed_version)"; [ -z "$cur" ] && cur="0.0.0"
  newer "$latest" "$cur" || return 0
  echo "[mine-auto] update available: $cur -> $latest, fetching + verifying ..."
  tmp="$BIN.new"; sums="$DATA_DIR/SHA256SUMS.new"
  dl "https://github.com/${REPO}/releases/latest/download/${ASSET}" "$tmp" || { rm -f "$tmp"; return 0; }
  dl "https://github.com/${REPO}/releases/latest/download/SHA256SUMS" "$sums" || { rm -f "$tmp" "$sums"; return 0; }
  # FAIL CLOSED: asset must be listed and match, verified by the OS sha256sum.
  local want got
  want="$(grep -E "  ${ASSET}\$|  \\*?${ASSET}\$" "$sums" | awk '{print $1}' | head -1)"
  got="$(sha256sum "$tmp" | awk '{print $1}')"
  if [ -n "$want" ] && [ "$want" = "$got" ]; then
    chmod +x "$tmp"; mv "$tmp" "$BIN"; rm -f "$sums"
    echo "[mine-auto] verified + installed $latest; restarting miner"
    return 1   # signal restart
  fi
  echo "[mine-auto] checksum verify FAILED (want=$want got=$got); keeping $cur"
  rm -f "$tmp" "$sums"; return 0
}

# Background updater: polls, and on a successful swap kills the miner so the
# main loop below relaunches the new binary. Exits if the miner is gone.
updater() {
  while sleep "$((CHECK_MIN*60))"; do
    if ! try_update; then pkill -TERM -f "$BIN" 2>/dev/null || true; fi
  done
}
updater & UPDATER_PID=$!
trap 'kill "$UPDATER_PID" 2>/dev/null || true' EXIT

# Startup update, then the run loop (restart-on-exit).
try_update || true
echo "[mine-auto] backend=$VARIANT  running $(installed_version)"
while true; do
  "$BIN" --config "$CFG" --log-dir "$DATA_DIR/logs" || true
  echo "[mine-auto] miner exited; relaunching in 5s (Ctrl-C to stop)"
  sleep 5
done
