#!/usr/bin/env bash
# Integration test for the cairn-miner HiveOS hooks (h-run/h-stats/h-stop/
# h-config). Uses stub `cairn-miner` + `nvidia-smi` so it needs no GPU. Asserts:
#   * per-GPU spawn: N workers on the right --device / --stats-port,
#   * NO --stats-bind is ever passed (the historical crash-loop bug),
#   * h-config lowercases an uppercase address,
#   * h-stats delegates to `cairn-miner hiveos-stats`,
#   * h-stop reaps every worker AND the supervisors respawn nothing.
# Run: bash hiveos/test-hiveos-hooks.sh   (exit 0 = pass)
set -u

FAIL=0
note() { printf '  %s\n' "$*"; }
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAIL=1; }

HOOKS_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
MINER_DIR="$WORK/miner"          # where HiveOS extracts the package
BINDIR="$WORK/bin"               # stub nvidia-smi lives here (front of PATH)
LOGBASE="$WORK/log/cairn-miner"
INVOCATIONS="$WORK/invocations.log"
mkdir -p "$MINER_DIR" "$BINDIR" "$(dirname "$LOGBASE")"
: > "$INVOCATIONS"

cleanup() {
  bash "$MINER_DIR/h-stop.sh" >/dev/null 2>&1 || true
  pkill -f "$MINER_DIR/cairn-miner" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# --- stub nvidia-smi: report 3 GPUs ---
cat > "$BINDIR/nvidia-smi" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = "-L" ]; then
  echo "GPU 0: STUB (UUID: GPU-0)"
  echo "GPU 1: STUB (UUID: GPU-1)"
  echo "GPU 2: STUB (UUID: GPU-2)"
fi
exit 0
EOF
chmod +x "$BINDIR/nvidia-smi"
export PATH="$BINDIR:$PATH"

# --- stub cairn-miner ---
cat > "$MINER_DIR/cairn-miner" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
  --help) echo "cairn-miner ... --stats-port <PORT> ..."; exit 0 ;;
  hiveos-stats) echo '{"khs":42.5,"stats":{"hs":[42.5],"hs_units":"khs","ar":[3,0],"uptime":10,"algo":"sha256d","ver":"0.2.2","bus_numbers":[]}}'; exit 0 ;;
esac
# mining mode: record argv, then run "forever" while keeping the cairn-miner name
echo "\$*" >> "$INVOCATIONS"
while :; do sleep 1; done
EOF
chmod +x "$MINER_DIR/cairn-miner"

# --- install the hooks under test ---
cp "$HOOKS_DIR"/h-run.sh "$HOOKS_DIR"/h-stats.sh "$HOOKS_DIR"/h-stop.sh \
   "$HOOKS_DIR"/h-config.sh "$HOOKS_DIR"/h-manifest.conf "$MINER_DIR/"
chmod +x "$MINER_DIR"/h-*.sh

echo "== h-config lowercases the address =="
CUSTOM_CONFIG_FILENAME="$MINER_DIR/config.toml" \
CUSTOM_TEMPLATE="0xABCDEF0123456789ABCDEF0123456789ABCDEF01.rig7" \
  bash "$MINER_DIR/h-config.sh"
if grep -q 'address = "abcdef0123456789abcdef0123456789abcdef01"' "$MINER_DIR/config.toml"; then
  ok "uppercase 0x-address normalized to lowercase, 0x stripped"
else
  bad "address not lowercased: $(grep address "$MINER_DIR/config.toml" 2>/dev/null)"
fi
grep -q 'worker = "rig7"' "$MINER_DIR/config.toml" && ok "worker split from template" || bad "worker not split"

echo "== h-run spawns one worker per GPU =="
CUSTOM_CONFIG_FILENAME="$MINER_DIR/config.toml" \
CUSTOM_LOG_BASENAME="$LOGBASE" \
CUSTOM_API_PORT=3380 \
  bash "$MINER_DIR/h-run.sh" >/dev/null 2>&1 &
sleep 3

running=$(pgrep -f "$MINER_DIR/cairn-miner" | wc -l | tr -d ' ')
[ "$running" = "3" ] && ok "3 miner processes running (1 per GPU)" || bad "expected 3 running, got $running"

for d in 0 1 2; do
  if grep -q -- "--device $d " "$INVOCATIONS" && grep -q -- "--stats-port $((3380 + d))" "$INVOCATIONS"; then
    ok "device $d launched with --stats-port $((3380 + d))"
  else
    bad "device $d not launched with the right stats-port"
  fi
done

if grep -q -- '--stats-bind' "$INVOCATIONS"; then
  bad "--stats-bind was passed (this crash-loops a real rig)"
else
  ok "no --stats-bind anywhere (the crash-loop bug is gone)"
fi

sup=$(grep -c . "$MINER_DIR/.cairn-sup.pids" 2>/dev/null || echo 0)
[ "$sup" = "2" ] && ok "2 supervisor PIDs recorded (cards 1,2)" || bad "expected 2 supervisor PIDs, got $sup"

echo "== h-stats delegates to hiveos-stats =="
khs_out=$(CUSTOM_API_PORT=3380 bash "$MINER_DIR/h-stats.sh" 2>/dev/null | tail -1)
if command -v jq >/dev/null 2>&1; then
  [ "$khs_out" = "42.5" ] && ok "h-stats returned khs=42.5 from hiveos-stats" || bad "h-stats khs was '$khs_out' (expected 42.5)"
else
  note "jq not installed — skipping h-stats value check (khs_out='$khs_out')"
fi

echo "== h-stop reaps everything and nothing respawns =="
bash "$MINER_DIR/h-stop.sh" >/dev/null 2>&1
sleep 5   # longer than the supervisor's 3s respawn delay
after=$(pgrep -f "$MINER_DIR/cairn-miner" | wc -l | tr -d ' ')
[ "$after" = "0" ] && ok "all workers stopped, no respawn after 5s" || bad "expected 0 after stop, got $after (supervisor respawned?)"
[ ! -f "$MINER_DIR/.cairn-sup.pids" ] && ok "supervisor pidfile removed" || bad "pidfile not removed"

echo
if [ "$FAIL" = 0 ]; then
  echo "hiveos hooks: ALL CHECKS PASSED"
  exit 0
else
  echo "hiveos hooks: FAILURES ABOVE"
  exit 1
fi
