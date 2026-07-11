#!/usr/bin/env bash
set -euo pipefail
# Builds the HiveOS custom-miner package: cairn-miner-hiveos.tar.gz containing
# the h-* hooks, the manifest, and a Linux binary at the tar root (HiveOS
# extracts it to /hive/miners/custom/cairn-miner/).
#
# Usage: packaging/build-hiveos-package.sh [path-to-binary] [out.tar.gz] [version]
# Defaults: target/release/cairn-miner  ->  dist/cairn-miner-hiveos.tar.gz
#
# IMPORTANT: pass a GPU-capable binary (an all-backends cuda+opencl build, or at
# least a CUDA build). HiveOS is a GPU mining OS; a CPU-only binary makes
# `--backend cuda` bail ("not compiled in") or silently CPU-mine at a tiny
# fraction of the card's rate. The binary MUST be built against an old glibc
# floor (HiveOS ships glibc ~2.27; e.g. `cargo zigbuild --target
# x86_64-unknown-linux-gnu.2.27`) or it fails to start with a GLIBC_2.xx error.
# `--backend auto` (the default) then picks cuda -> opencl -> cpu at runtime.

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/target/release/cairn-miner}"
OUT="${2:-$ROOT/dist/cairn-miner-hiveos.tar.gz}"
VERSION="${3:-}"

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

# Stamp the manifest version from the release tag (e.g. "0.2.3") so HiveOS shows
# the right version. Only touch it for a plausible version string.
if printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+'; then
  sed -i -E "s/^CUSTOM_VERSION=.*/CUSTOM_VERSION=${VERSION}/" "$pkg/h-manifest.conf"
  echo "stamped CUSTOM_VERSION=${VERSION}"
fi

# HiveOS expects the miner dir at the tar root.
tar -czf "$OUT" -C "$stage" cairn-miner
echo "built $OUT"
( cd "$(dirname "$OUT")" && sha256sum "$(basename "$OUT")" )
