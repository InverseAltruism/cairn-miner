#!/usr/bin/env bash
# Integration test for the cairn-miner HiveOS hooks (h-run/h-stats/h-stop/
# h-config). Uses stub `cairn-miner` + `nvidia-smi` so it needs no GPU. Covers:
#   * per-GPU spawn: N workers on the right --device / --stats-port,
#   * NO stats "bind" flag is ever passed (the historical crash-loop bug),
#   * Extra config args reach EVERY worker (not just device 0),
#   * a flag also present in Extra args is not injected twice (clap exit-2),
#   * operator-pinned --device / --backend cpu => single process, no supervisors,
#   * h-config lowercases the address and warns on an empty wallet,
#   * h-stats is SOURCED (BASH_SOURCE resolution) and delegates to hiveos-stats,
#   * h-stop reaps every worker AND the supervisors respawn nothing.
# Run: bash hiveos/test-hiveos-hooks.sh   (exit 0 = pass)
set -u

FAIL=0
ok()  { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAIL=1; }
note(){ printf '  %s\n' "$*"; }

HOOKS_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
MINER_DIR="$WORK/miner"
BINDIR="$WORK/bin"
LOGBASE="$WORK/log/cairn-miner"
INVOCATIONS="$WORK/invocations.log"
STATSMARK="$WORK/hiveos-stats.called"
mkdir -p "$MINER_DIR" "$BINDIR" "$(dirname "$LOGBASE")"

stop_all() { bash "$MINER_DIR/h-stop.sh" >/dev/null 2>&1 || true; pkill -f "$MINER_DIR/cairn-miner" 2>/dev/null || true; sleep 1; }
cleanup() { stop_all; rm -rf "$WORK"; }
trap cleanup EXIT

# stub nvidia-smi: report 3 GPUs (toggle by renaming to disable)
cat > "$BINDIR/nvidia-smi" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = "-L" ] && { echo "GPU 0: STUB"; echo "GPU 1: STUB"; echo "GPU 2: STUB"; }
exit 0
EOF
chmod +x "$BINDIR/nvidia-smi"
export PATH="$BINDIR:$PATH"

# stub cairn-miner
cat > "$MINER_DIR/cairn-miner" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
  --help) echo "cairn-miner ... --stats-port <PORT> ..."; exit 0 ;;
  hiveos-stats) : > "$STATSMARK"; echo '{"khs":42.5,"stats":{"hs":[42.5],"hs_units":"khs","ar":[3,0],"uptime":10,"algo":"sha256d","ver":"0.2.3","bus_numbers":[]}}'; exit 0 ;;
  devices) echo '{"gpus":[{"backend":"opencl","index":0,"name":"stub"}],"cpu":{"logical_cores":8},"notes":[]}'; exit 0 ;;
esac
echo "\$*" >> "$INVOCATIONS"
while :; do sleep 1; done
EOF
chmod +x "$MINER_DIR/cairn-miner"

cp "$HOOKS_DIR"/h-run.sh "$HOOKS_DIR"/h-stats.sh "$HOOKS_DIR"/h-stop.sh \
   "$HOOKS_DIR"/h-config.sh "$HOOKS_DIR"/h-manifest.conf "$MINER_DIR/"
chmod +x "$MINER_DIR"/h-*.sh

run_hrun() { # $1 = EXTRA (CUSTOM_USER_CONFIG)
  : > "$INVOCATIONS"
  CUSTOM_CONFIG_FILENAME="$MINER_DIR/config.toml" \
  CUSTOM_LOG_BASENAME="$LOGBASE" \
  CUSTOM_API_PORT=3380 \
  CUSTOM_USER_CONFIG="${1:-}" \
    bash "$MINER_DIR/h-run.sh" >/dev/null 2>&1 &
  sleep 3
}

echo "== h-config: lowercase address + empty-wallet warning =="
CUSTOM_CONFIG_FILENAME="$MINER_DIR/config.toml" \
CUSTOM_TEMPLATE="0xABCDEF0123456789ABCDEF0123456789ABCDEF01.rig7" bash "$MINER_DIR/h-config.sh"
grep -q 'address = "abcdef0123456789abcdef0123456789abcdef01"' "$MINER_DIR/config.toml" \
  && ok "uppercase 0x-address lowercased + 0x stripped" || bad "address not lowercased"
grep -q 'worker = "rig7"' "$MINER_DIR/config.toml" && ok "worker split from template" || bad "worker not split"
CUSTOM_CONFIG_FILENAME="$WORK/empty.toml" CUSTOM_TEMPLATE="" bash "$MINER_DIR/h-config.sh"
grep -qi 'WARNING: no wallet' "$WORK/empty.toml" && ok "empty wallet warns in config" || bad "no empty-wallet warning"

echo "== h-run: per-GPU spawn, Extra args reach every worker =="
run_hrun "--blocks 1024"
running=$(pgrep -f "$MINER_DIR/cairn-miner --" | wc -l | tr -d ' ')
[ "$running" = "3" ] && ok "3 miner processes (1 per GPU)" || bad "expected 3 running, got $running"
for d in 0 1 2; do
  grep -q -- "--device $d " "$INVOCATIONS" && grep -q -- "--stats-port $((3380 + d))" "$INVOCATIONS" \
    && ok "device $d on --stats-port $((3380 + d))" || bad "device $d wrong stats-port"
done
extra_workers=$(grep -c -- '--blocks 1024' "$INVOCATIONS")
[ "$extra_workers" = "3" ] && ok "Extra args (--blocks 1024) reached all 3 workers" || bad "Extra args reached only $extra_workers/3 workers"
grep -q -- '--stats-bind' "$INVOCATIONS" && bad "stats-bind flag was passed" || ok "no stats-bind flag (crash-loop bug gone)"
sup=$(grep -c . "$MINER_DIR/.cairn-sup.pids" 2>/dev/null); sup=${sup:-0}
[ "$sup" = "2" ] && ok "2 supervisor PIDs recorded" || bad "expected 2 supervisor PIDs, got $sup"

echo "== h-stats: SOURCED (BASH_SOURCE) + pidfile-based gpu count =="
rm -f "$STATSMARK"
( cd / && CUSTOM_API_PORT=3380 . "$MINER_DIR/h-stats.sh" >/dev/null 2>&1 )   # SOURCE from foreign CWD
[ -f "$STATSMARK" ] && ok "sourced h-stats located the binary + ran hiveos-stats" || bad "sourced h-stats did NOT find the binary (BASH_SOURCE bug)"
stop_all

echo "== glob char in Extra args is NOT expanded against the miner dir =="
# The miner dir contains h-*.sh; without `set -f` these would expand into argv.
run_hrun "--worker h-*.sh"
if grep -q -- '--worker h-\*.sh' "$INVOCATIONS"; then
  ok "glob '*' in Extra args passed literally (no pathname expansion)"
else
  bad "glob in Extra args was expanded to filenames: $(grep -m1 -- '--worker' "$INVOCATIONS")"
fi
stop_all

echo "== no duplicate flag when Extra args repeat --stats-port =="
run_hrun "--stats-port 9999"
dupes=$(awk '{n=gsub(/--stats-port/,"&"); if(n>1) c++} END{print c+0}' "$INVOCATIONS")
[ "$dupes" = "0" ] && ok "no invocation has a duplicated --stats-port" || bad "$dupes invocation(s) duplicated --stats-port (clap exit-2 risk)"
stop_all

echo "== operator-pinned --device => single process, no supervisors =="
run_hrun "--device 2"
running=$(pgrep -f "$MINER_DIR/cairn-miner --" | wc -l | tr -d ' ')
[ "$running" = "1" ] && ok "single process when --device pinned" || bad "expected 1 process, got $running"
sup=$(grep -c . "$MINER_DIR/.cairn-sup.pids" 2>/dev/null); sup=${sup:-0}
[ "$sup" = "0" ] && ok "no supervisors in single mode" || bad "expected 0 supervisors, got $sup"

echo "== h-stop reaps everything and nothing respawns =="
bash "$MINER_DIR/h-stop.sh" >/dev/null 2>&1
sleep 5
after=$(pgrep -f "$MINER_DIR/cairn-miner --" | wc -l | tr -d ' ')
[ "$after" = "0" ] && ok "all workers stopped, no respawn after 5s" || bad "expected 0 after stop, got $after"
[ ! -f "$MINER_DIR/.cairn-sup.pids" ] && ok "supervisor pidfile removed" || bad "pidfile not removed"

echo
if [ "$FAIL" = 0 ]; then echo "hiveos hooks: ALL CHECKS PASSED"; exit 0
else echo "hiveos hooks: FAILURES ABOVE"; exit 1; fi
