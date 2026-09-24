#!/usr/bin/env bash
# Wait until VictoriaMetrics holds a fresh sample of every series selector given,
# and name the ones it does not.
#
#   check.sh [--wait S] [--fresh S] <selector>...
#     --wait S    keep polling up to S seconds (default 180): a scrape lands every 15 s
#     --fresh S   a sample counts when it is at most S seconds old (default 90)
#   exit 0 when every selector matches; 1 otherwise, each missing one printed as
#   "missing: <selector>" on stderr.
#
#   VM=http://127.0.0.1:8428 (default)
set -uo pipefail
VM="${VM:-http://127.0.0.1:8428}"
wait_s=180; fresh_s=90
while [ $# -gt 0 ]; do
  case "$1" in
    --wait)  wait_s="$2"; shift 2 ;;
    --fresh) fresh_s="$2"; shift 2 ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || { echo "usage: $0 [--wait S] [--fresh S] <selector>..." >&2; exit 2; }

present() { # $1 selector: at least one series with a sample in the last fresh_s
  curl -s --noproxy '*' --max-time 10 "$VM/api/v1/query" \
    --data-urlencode "query=count(last_over_time($1[${fresh_s}s]))" 2>/dev/null \
    | python3 -c 'import sys,json
try: r=json.load(sys.stdin)["data"]["result"]; sys.exit(0 if r and float(r[0]["value"][1])>0 else 1)
except Exception: sys.exit(1)'
}

deadline=$(( $(date +%s) + wait_s ))
pending=("$@")
while :; do
  left=()
  for sel in "${pending[@]}"; do present "$sel" || left+=("$sel"); done
  [ ${#left[@]} -eq 0 ] && { echo "observability: all ${#} series present in $VM" >&2; exit 0; }
  [ "$(date +%s)" -lt "$deadline" ] || break
  pending=("${left[@]}")
  sleep 10
done
for sel in "${left[@]}"; do echo "missing: $sel" >&2; done
echo "observability: ${#left[@]} of ${#} series missing from $VM after ${wait_s}s" >&2
exit 1
