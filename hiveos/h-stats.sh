#!/usr/bin/env bash
# cairn-miner - HiveOS stats hook. Sourced by the HiveOS agent every ~10s; must
# set two shell vars: `khs` (total kH/s) and `stats` (JSON). We prefer the
# miner's HTTP stats endpoint if one is running (--stats-port), and fall back to
# parsing the run log for the latest `stratum hashrate ... combined=X GH/s` line
# plus accepted/rejected counts. Robust to a missing log (reports zeros = idle).

khs=0
stats='{}'

LOG="${CUSTOM_LOG_BASENAME:-/var/log/miner/cairn-miner/cairn-miner}.log"
STATS_PORT="${CAIRN_STATS_PORT:-}"

# 1. Preferred: HTTP stats endpoint (if the miner was started with --stats-port).
if [[ -n "$STATS_PORT" ]] && command -v curl >/dev/null 2>&1; then
  j=$(curl -fsS --max-time 2 "http://127.0.0.1:${STATS_PORT}/summary" 2>/dev/null || true)
  if [[ -n "$j" ]]; then
    hs=$(echo "$j" | jq -r '.hashrate_total // 0' 2>/dev/null)
    [[ -n "$hs" && "$hs" != "null" ]] && khs=$(awk "BEGIN{printf \"%.3f\", $hs/1000}")
    stats="$j"
    echo "$khs"; return 2>/dev/null || exit 0
  fi
fi

# 2. Fallback: parse the run log.
if [[ -f "$LOG" ]]; then
  # latest combined hashrate in GH/s -> kH/s
  ghs=$(grep -oE 'combined=[0-9.]+ GH/s' "$LOG" 2>/dev/null | tail -1 | grep -oE '[0-9.]+' | head -1)
  if [[ -z "$ghs" ]]; then
    # GPU-off CPU-only runs log "cpu=X MH/s"; take that instead.
    mhs=$(grep -oE 'cpu=[0-9.]+ MH/s' "$LOG" 2>/dev/null | tail -1 | grep -oE '[0-9.]+' | head -1)
    [[ -n "$mhs" ]] && khs=$(awk "BEGIN{printf \"%.1f\", $mhs*1000}")
  else
    khs=$(awk "BEGIN{printf \"%.1f\", $ghs*1000000}")
  fi
  acc=$(grep -c 'stratum submit OK' "$LOG" 2>/dev/null || echo 0)
  rej=$(grep -cE 'submit (rejected|reject|error)' "$LOG" 2>/dev/null || echo 0)
  # GPU temps from HiveOS-provided arrays if present.
  temps="[]"; fans="[]"; buses="[]"
  [[ -n "$gpu_temp" ]] && temps=$(printf '%s' "$gpu_temp" | jq -cR 'split(" ")|map(tonumber?)' 2>/dev/null || echo "[]")
  hs_json="[$(awk "BEGIN{printf \"%.3f\", ${khs:-0}}")]"
  stats=$(jq -nc \
    --argjson hs "$hs_json" --arg acc "${acc:-0}" --arg rej "${rej:-0}" \
    --argjson temp "$temps" \
    '{hs:$hs, hs_units:"khs", ar:[($acc|tonumber),($rej|tonumber)], temp:$temp, uptime:0, ver:"0.1.0", algo:"sha256d"}' 2>/dev/null || echo '{}')
fi

echo "$khs"
