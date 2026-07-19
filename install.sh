#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
#  cairn-miner - one-line installer for Linux.
#
#  Quick start (auto-detects GPU, mines to the cairn pool):
#     curl -fsSL https://raw.githubusercontent.com/InverseAltruism/cairn-miner/main/install.sh | CAIRN_ADDR=<addr20> bash
#
#  What it does:
#     1. Detects your GPU (NVIDIA -> cuda, AMD -> opencl) or falls back to CPU.
#     2. Installs the matching cairn-miner binary (prebuilt release if one is
#        published for your arch; otherwise builds from source with cargo).
#     3. Saves your payout address + a config.toml once.
#     4. Starts mining. Optionally installs a systemd service (--service).
#
#  Options (env or flags):
#     CAIRN_ADDR=<addr20>      your 40-hex payout address (required, no TTY prompt under curl|bash)
#     CAIRN_POOL=host:3333     override the pool endpoint (default: cairn-pool.com:3333)
#     CAIRN_WORKER=<name>      rig name (default: hostname)
#     CAIRN_BACKEND=auto|cpu|cuda|opencl   force a backend (default: auto-detect)
#     --service                install + enable a systemd user/system service instead of running inline
#     --no-run                 install only, don't start
#
#  This installer NEVER installs GPU drivers. The cuda/opencl builds need your
#  vendor driver already present; without it, it uses the cpu build.
# ============================================================================

REPO="InverseAltruism/cairn-miner"
DEFAULT_POOL="cairn-pool.com:3333"

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/cairn-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/cairn-miner"
BIN="$DATA_DIR/cairn-miner"
CFG="$CFG_DIR/config.toml"
mkdir -p "$DATA_DIR" "$CFG_DIR"

WANT_SERVICE=0 ; WANT_RUN=1 ; VARIANT="${CAIRN_BACKEND:-}"
for a in "$@"; do case "$a" in
  --service) WANT_SERVICE=1 ;;
  --no-run)  WANT_RUN=0 ;;
  nvidia|cuda)  VARIANT="cuda" ;;
  amd|opencl)   VARIANT="opencl" ;;
  cpu)          VARIANT="cpu" ;;
esac; done

say(){ printf '%s\n' "$*"; }
grn(){ printf '\033[38;2;87;217;119m%s\033[0m\n' "$*"; }  # cairn phosphor green
die(){ printf '\033[38;2;255;107;107m[x] %s\033[0m\n' "$*" >&2; exit 1; }

grn "  === cairn-miner installer (Linux) ==="
say

# --- 1. backend detection --------------------------------------------------
if [ -z "$VARIANT" ]; then
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
    VARIANT="cuda"
  elif { command -v lspci >/dev/null 2>&1 && lspci 2>/dev/null | grep -Eiq 'VGA.*(AMD|ATI)|Radeon|Display.*(AMD|ATI)'; } \
       || { command -v clinfo >/dev/null 2>&1 && clinfo 2>/dev/null | grep -Eiq 'AMD|Radeon'; }; then
    VARIANT="opencl"
  else
    VARIANT="cpu"
  fi
fi
say "  backend:   $VARIANT"
say "  pool:      ${CAIRN_POOL:-$DEFAULT_POOL}"
say

# --- 2. obtain the binary: prebuilt release, else build from source --------
# Done BEFORE the address check so the "create one: cairn-miner newwallet"
# instruction below actually works — the binary exists by the time we print it.
ARCH="$(uname -m)"
ASSET="cairn-miner-linux-${VARIANT}-${ARCH}"
download(){ # url out
  local tmp="$2.tmp"
  if command -v curl >/dev/null 2>&1; then curl -fL --retry 3 -o "$tmp" "$1" && mv "$tmp" "$2"
  elif command -v wget >/dev/null 2>&1; then wget -qO "$tmp" "$1" && mv "$tmp" "$2"
  else return 1; fi
}
REL_URL="https://github.com/${REPO}/releases/latest/download/${ASSET}"
SUMS_URL="https://github.com/${REPO}/releases/latest/download/SHA256SUMS"
# Verify the downloaded binary against the release SHA256SUMS before trusting it.
# FAIL CLOSED: a present-but-unverifiable binary is deleted and we fall through to
# build-from-source, never run an unverified download. Mirrors mine-auto.sh.
verify_asset(){ # binpath -> 0 ok / 1 fail
  local bin="$1" sums="$DATA_DIR/SHA256SUMS.tmp" want got
  command -v sha256sum >/dev/null 2>&1 || { say "  [warn] sha256sum not found - cannot verify download"; return 1; }
  download "$SUMS_URL" "$sums" 2>/dev/null || { say "  [warn] could not fetch SHA256SUMS"; return 1; }
  want="$(grep -E "  \\*?${ASSET}\$" "$sums" | awk '{print $1}' | head -1)"
  got="$(sha256sum "$bin" | awk '{print $1}')"
  rm -f "$sums"
  [ -n "$want" ] && [ "$want" = "$got" ]
}
if download "$REL_URL" "$BIN" 2>/dev/null && [ -s "$BIN" ] && verify_asset "$BIN"; then
  chmod +x "$BIN"; grn "  [ok] installed prebuilt $ASSET (sha256 verified)"
else
  [ -s "$BIN" ] && { rm -f "$BIN"; say "  [warn] prebuilt $ASSET failed checksum verification - not using it"; }
  say "  no prebuilt $ASSET published yet - building from source with cargo..."
  command -v cargo >/dev/null 2>&1 || die "cargo (Rust) not found. Install Rust from https://rustup.rs then re-run, or wait for a published release."
  SRC="$DATA_DIR/src"
  if [ -d "$SRC/.git" ]; then git -C "$SRC" pull -q || true
  else git clone -q --depth 1 "https://github.com/${REPO}.git" "$SRC" || die "git clone failed"; fi
  FEAT=""; case "$VARIANT" in cuda) FEAT="--features cuda";; opencl) FEAT="--features opencl";; esac
  # IMPORTANT: plain release build - never target-cpu=native (kills the SHA-NI path).
  ( cd "$SRC" && cargo build --release $FEAT ) || die "cargo build failed (for GPU builds you need the CUDA/OpenCL dev libs; try CAIRN_BACKEND=cpu)"
  cp "$SRC/target/release/cairn-miner" "$BIN"; chmod +x "$BIN"
  grn "  [ok] built cairn-miner ($VARIANT) from source"
fi

# --- 3. address ------------------------------------------------------------
ADDR="${CAIRN_ADDR:-}"
[ -z "$ADDR" ] && ADDR="$(grep -oE '^address *= *"[0-9a-fx]+"' "$CFG" 2>/dev/null | grep -oE '[0-9a-fx]{40,42}' | head -1 || true)"
if [ -z "$ADDR" ] && [ -t 0 ]; then
  read -rp "  your CSD payout address (addr20, 40 hex): " ADDR
fi
ADDR="${ADDR#0x}"
# The miner hard-rejects uppercase hex (crash-loop); normalize like h-config.sh.
ADDR="${ADDR,,}"
[ -z "$ADDR" ] && die "no payout address. Create one now:  $BIN newwallet    then re-run with:  curl ... | CAIRN_ADDR=<addr20> bash"
printf '%s' "$ADDR" | grep -Eq '^[0-9a-f]{40}$' || die "address must be 40 hex chars (got: $ADDR)"
say "  address:   $ADDR"
say

# --- 4. write config -------------------------------------------------------
{
  echo "# cairn-miner config (written by install.sh)"
  echo "address = \"$ADDR\""
  [ -n "${CAIRN_POOL:-}" ] && echo "pool = \"$CAIRN_POOL\""
  [ -n "${CAIRN_WORKER:-}" ] && echo "worker = \"$CAIRN_WORKER\""
  echo "backend = \"$VARIANT\""
  echo "cpu_threads = 0   # GPU-only by default; raise on a desktop with thermal headroom"
} > "$CFG"
say "  config:    $CFG"

# --- 5. run or install a service ------------------------------------------
RUN_ARGS=(--config "$CFG" --log-dir "$DATA_DIR/logs")
if [ "$WANT_SERVICE" = 1 ]; then
  UNIT="$HOME/.config/systemd/user/cairn-miner.service"
  mkdir -p "$(dirname "$UNIT")"
  cat > "$UNIT" <<EOF
[Unit]
Description=cairn-miner (Compute Substrate pool miner)
After=network-online.target
Wants=network-online.target
[Service]
ExecStart=$BIN ${RUN_ARGS[*]}
Restart=always
RestartSec=5
Nice=5
[Install]
WantedBy=default.target
EOF
  systemctl --user daemon-reload 2>/dev/null || true
  systemctl --user enable --now cairn-miner.service 2>/dev/null \
    && grn "  [ok] systemd user service 'cairn-miner' enabled (logs: journalctl --user -u cairn-miner -f)" \
    || say "  [i] to run at boot as a system service, copy $UNIT to /etc/systemd/system/ and 'sudo systemctl enable --now cairn-miner'"
  exit 0
fi

if [ "$WANT_RUN" = 1 ]; then
  grn "  starting cairn-miner - Ctrl-C to stop. Re-run anytime:  $BIN --config $CFG"
  exec "$BIN" "${RUN_ARGS[@]}"
else
  say "  installed. start with:  $BIN --config $CFG"
fi
