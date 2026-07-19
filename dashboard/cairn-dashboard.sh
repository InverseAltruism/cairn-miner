#!/usr/bin/env sh
# cairn-dashboard.sh - watch a running cairn-miner in the terminal.
#
# Reads the miner's local /stats endpoint (loopback only; the miner must run
# with --stats-port, which HiveOS sets automatically) and renders a live view:
# hashrate, accepted/rejected/stale shares + reject %, difficulty, uptime,
# reconnects, connection state, version - and GPU temp/power when nvidia-smi is
# present. Read-only: it never touches mining.
#
# Multi-GPU rigs run one worker process per card on consecutive stats ports
# (BASE, BASE+1, ...); this probes upward from --port and aggregates them.
#
# Usage:
#   ./cairn-dashboard.sh                 # poll 127.0.0.1:3380, refresh every 2s
#   ./cairn-dashboard.sh --port 3380 --refresh 2
#   ./cairn-dashboard.sh --once          # print one frame and exit
#
# Pure POSIX sh + curl + awk - no jq, no extra deps (stock HiveOS shell works).
# Press q or Ctrl-C to quit.

set -u

PORT=3380
REFRESH=2
ONCE=0
MAX_GPUS=16   # cap the upward port probe

while [ $# -gt 0 ]; do
  case "$1" in
    --port)    PORT="${2:-3380}"; shift 2 ;;
    --refresh) REFRESH="${2:-2}"; shift 2 ;;
    --once)    ONCE=1; shift ;;
    -h|--help)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "unknown arg: $1 (see --help)" >&2; exit 2 ;;
  esac
done

command -v curl >/dev/null 2>&1 || { echo "curl not found" >&2; exit 1; }
HAVE_NVSMI=0
command -v nvidia-smi >/dev/null 2>&1 && HAVE_NVSMI=1

# --- tiny helpers -----------------------------------------------------------

# fetch <port> -> raw JSON on stdout, empty + nonzero if the worker isn't up.
fetch() { curl -fs -m 2 "http://127.0.0.1:$1/stats" 2>/dev/null; }

# jnum <json> <key> -> numeric value (or empty). Flat JSON only.
jnum() { printf '%s' "$1" | grep -oE "\"$2\":[-0-9.eE+]+" | head -1 | sed "s/\"$2\"://"; }
# jstr <json> <key> -> string value (or empty).
jstr() { printf '%s' "$1" | grep -oE "\"$2\":\"[^\"]*\"" | head -1 | sed "s/\"$2\":\"//;s/\"$//"; }
# jbool <json> <key> -> true/false.
jbool() { printf '%s' "$1" | grep -oE "\"$2\":(true|false)" | head -1 | sed "s/\"$2\"://"; }

# hr <hashes/sec> -> human string like "12.34 GH/s".
hr() {
  awk -v v="${1:-0}" 'BEGIN{
    if (v>=1e9) printf "%.2f GH/s", v/1e9;
    else if (v>=1e6) printf "%.2f MH/s", v/1e6;
    else if (v>=1e3) printf "%.2f KH/s", v/1e3;
    else printf "%.0f H/s", v;
  }'
}

# dur <secs> -> "1d02h", "3h04m", "5m10s".
dur() {
  awk -v s="${1:-0}" 'BEGIN{
    s=int(s); d=int(s/86400); h=int((s%86400)/3600); m=int((s%3600)/60); sec=s%60;
    if (d>0) printf "%dd%02dh", d, h;
    else if (h>0) printf "%dh%02dm", h, m;
    else printf "%dm%02ds", m, sec;
  }'
}

# gpu_tp <index> -> "62C 210W" via nvidia-smi, or empty.
gpu_tp() {
  [ "$HAVE_NVSMI" = 1 ] || return 0
  nvidia-smi --query-gpu=temperature.gpu,power.draw --format=csv,noheader,nounits -i "$1" 2>/dev/null \
    | awk -F',' 'NR==1{gsub(/ /,""); printf "%dC %dW", $1, $2}'
}

render() {
  # collect all live workers by probing ports upward from PORT
  n_up=0
  tot_hps=0; tot_acc=0; tot_rej=0; tot_stale=0
  rows=""
  i=0
  while [ "$i" -lt "$MAX_GPUS" ]; do
    p=$((PORT + i))
    j="$(fetch "$p")"
    if [ -z "$j" ]; then
      [ "$i" -eq 0 ] && break   # nothing on the base port at all
      # allow one gap? no - workers are contiguous; stop at first miss past base
      break
    fi
    n_up=$((n_up + 1))
    conn="$(jbool "$j" connected)"; [ "$conn" = "true" ] && dot="UP " || dot="DOWN"
    wk="$(jstr "$j" worker)"; [ -z "$wk" ] && wk="gpu$i"
    diff="$(jnum "$j" difficulty)"
    hps="$(jnum "$j" hashrate_total_hps)"
    acc="$(jnum "$j" shares_accepted)"; rej="$(jnum "$j" shares_rejected)"; stale="$(jnum "$j" shares_stale)"
    rc="$(jnum "$j" reconnects)"
    tp="$(gpu_tp "$i")"
    tot_hps="$(awk -v a="$tot_hps" -v b="${hps:-0}" 'BEGIN{print a+b}')"
    tot_acc=$((tot_acc + ${acc:-0})); tot_rej=$((tot_rej + ${rej:-0})); tot_stale=$((tot_stale + ${stale:-0}))
    rows="$rows$(printf '  %-10s %-4s %12s  d=%-8s a=%-5s r=%-4s s=%-4s rc=%-3s %s\n' \
      "$wk" "$dot" "$(hr "${hps:-0}")" "${diff:-?}" "${acc:-0}" "${rej:-0}" "${stale:-0}" "${rc:-0}" "$tp")
"
    i=$((i + 1))
  done

  # header (read version/uptime/backend/last-share from the base worker)
  base="$(fetch "$PORT")"
  clear 2>/dev/null || printf '\033[2J\033[H'
  if [ -z "$base" ]; then
    printf '  cairn-miner dashboard\n\n  no miner answering on 127.0.0.1:%s/stats\n' "$PORT"
    printf '  start the miner with  --stats-port %s  (HiveOS sets this automatically).\n' "$PORT"
    return
  fi
  ver="$(jstr "$base" version)"; be="$(jstr "$base" backend)"
  up="$(dur "$(jnum "$base" uptime_secs)")"; pool="$(jstr "$base" pool)"
  lsa="$(jnum "$base" last_share_age_secs)"
  if [ -z "$lsa" ]; then last_share="no shares yet"; else last_share="${lsa}s ago"; fi
  rejpct="$(awk -v a="$tot_acc" -v r="$tot_rej" 'BEGIN{t=a+r; if(t>0) printf "%.1f", 100*r/t; else print "0.0"}')"

  printf '  cairn-miner v%s  ·  %s  ·  pool %s  ·  up %s\n' "${ver:-?}" "${be:-?}" "${pool:-?}" "$up"
  printf '  ----------------------------------------------------------------------\n'
  printf '  TOTAL  %s   accepted %s   rejected %s (%s%%)   stale %s   last share %s\n' \
    "$(hr "$tot_hps")" "$tot_acc" "$tot_rej" "$rejpct" "$tot_stale" "$last_share"
  printf '  workers (%s):\n' "$n_up"
  printf '%s' "$rows"
  printf '  ----------------------------------------------------------------------\n'
  printf '  refresh %ss · q or Ctrl-C to quit\n' "$REFRESH"
}

if [ "$ONCE" = 1 ]; then
  render
  exit 0
fi

# Interactive loop: render, then wait REFRESH seconds OR until the user hits 'q'.
trap 'printf "\n"; exit 0' INT TERM
while :; do
  render
  # read -t is not POSIX-guaranteed but works in bash/busybox ash; fall back to sleep.
  if read -t "$REFRESH" -r key 2>/dev/null; then
    case "$key" in q|Q) break ;; esac
  else
    sleep "$REFRESH" 2>/dev/null || sleep 2
  fi
done
