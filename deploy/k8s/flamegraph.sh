#!/usr/bin/env bash
# Capture an on-demand CPU flamegraph SVG from a running b2bua-worker (b2b) or
# front-proxy (lb) pod via its metrics-port /debug/flamegraph route (pprof-rs,
# in-process SIGPROF sampling -> inferno SVG). No perf/privileges/PMU needed.
#
#   ./flamegraph.sh b2b   [seconds] [pod-index]   # b2bua-worker-<idx>:9091
#   ./flamegraph.sh lb    [seconds] [pod-index]   # sip-front-proxy[<idx>]:9090
#
# Defaults: seconds=20, pod-index=0. Output: flamegraph-<tier>-<idx>-<ts>.svg
# (override path with OUT=...). To profile across a chaos reboot, run with a
# window (e.g. 60) just before triggering the kill so the sample spans it.
set -euo pipefail
cd "$(dirname "$0")"
NS="${NS:-sip-test}"
tier="${1:-b2b}"; secs="${2:-20}"; idx="${3:-0}"

case "$tier" in
  b2b|worker|b2bua)
    pod="b2bua-worker-$idx"; port=9091 ;;
  lb|proxy)
    pod="$(kubectl -n "$NS" get pod -l app=sip-front-proxy -o jsonpath="{.items[$idx].metadata.name}")"
    port=9090 ;;
  *) echo "usage: $0 {b2b|lb} [seconds] [pod-index]" >&2; exit 1 ;;
esac
[ -n "$pod" ] || { echo "no pod found for tier=$tier idx=$idx" >&2; exit 1; }

ts="$(date +%Y%m%d-%H%M%S)"
out="${OUT:-flamegraph-$tier-$idx-$ts.svg}"
pflog="$(mktemp)"
echo ">> profiling $pod:$port for ${secs}s -> $out"
kubectl -n "$NS" port-forward "pod/$pod" "0:$port" >"$pflog" 2>&1 &
pf=$!; trap 'kill "$pf" 2>/dev/null || true; rm -f "$pflog"' EXIT
# Discover the ephemeral local port kubectl chose.
lp=""
for _ in $(seq 1 50); do
  lp="$(grep -oE '127.0.0.1:[0-9]+' "$pflog" | head -1)" && [ -n "$lp" ] && break
  sleep 0.2
done
[ -n "$lp" ] || { echo "port-forward failed:" >&2; cat "$pflog" >&2; exit 1; }
# curl timeout must exceed the sample window + render time.
curl -fsS --max-time "$(( secs + 45 ))" "http://$lp/debug/flamegraph?seconds=$secs" -o "$out" \
  || { echo "capture failed (is the new image deployed?)" >&2; exit 1; }
echo ">> wrote $out ($(wc -c < "$out") bytes) — open in a browser"
