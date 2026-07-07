#!/usr/bin/env bash
set -euo pipefail
# Builds the HiveOS custom-miner package: cairn-miner-hiveos.tar.gz containing
# the h-* hooks, the manifest, and a Linux binary at the tar root (HiveOS
# extracts it to /hive/miners/custom/cairn-miner/).
#
# Usage: packaging/build-hiveos-package.sh [path-to-binary] [out.tar.gz]
# Defaults: target/release/cairn-miner  ->  dist/cairn-miner-hiveos.tar.gz
# The bundled binary is the CPU build (universal brick-safe seed); the flight
# sheet's --backend + the rig's own GPU pick the real backend at runtime.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/target/release/cairn-miner}"
OUT="${2:-$ROOT/dist/cairn-miner-hiveos.tar.gz}"

[ -x "$BIN" ] || { echo "no binary at $BIN (build it: cargo build --release)"; exit 1; }
mkdir -p "$(dirname "$OUT")"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
pkg="$stage/cairn-miner"
mkdir -p "$pkg"

cp "$ROOT"/hiveos/h-manifest.conf "$pkg/"
cp "$ROOT"/hiveos/h-config.sh "$ROOT"/hiveos/h-run.sh "$ROOT"/hiveos/h-stats.sh "$ROOT"/hiveos/h-stop.sh "$pkg/"
cp "$BIN" "$pkg/cairn-miner"
chmod +x "$pkg"/h-*.sh "$pkg/cairn-miner"

# HiveOS expects the miner dir at the tar root.
tar -czf "$OUT" -C "$stage" cairn-miner
echo "built $OUT"
( cd "$(dirname "$OUT")" && sha256sum "$(basename "$OUT")" )
