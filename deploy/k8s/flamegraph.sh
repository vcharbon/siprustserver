#!/usr/bin/env bash
# Capture an on-demand CPU flamegraph SVG from a running b2bua-worker (b2b) or
# front-proxy (lb) pod via the /debug/flamegraph route of its loopback
# listener, through a port-forward (pprof-rs, in-process SIGPROF sampling ->
# inferno SVG). No perf/privileges/PMU needed. The pod runs a profiling build,
# the runner built with its `flamegraph` feature; a default build answers 501.
#
#   ./flamegraph.sh b2b   [seconds] [pod-index]   # b2bua-worker-<idx>:9094 (worker.admin.bind)
#   ./flamegraph.sh lb    [seconds] [pod-index]   # sip-front-proxy[<idx>]:9097 (proxy.debug.bind)
#
# A release that moves those listeners states their ports in
# WORKER_DEBUG_PORT and PROXY_DEBUG_PORT.
# Defaults: seconds=20, pod-index=0. Output: flamegraph-<tier>-<idx>-<ts>.svg
# (override path with OUT=...). To profile across a chaos reboot, run with a
# window (e.g. 60) just before triggering the kill so the sample spans it.
set -euo pipefail
cd "$(dirname "$0")"
NS="${NS:-sip-test}"
tier="${1:-b2b}"; secs="${2:-20}"; idx="${3:-0}"

case "$tier" in
  b2b|worker|b2bua)
    pod="b2bua-worker-$idx"; port="${WORKER_DEBUG_PORT:-9094}" ;;
  lb|proxy)
    pod="$(kubectl -n "$NS" get pod -l app=sip-front-proxy -o jsonpath="{.items[$idx].metadata.name}")"
    port="${PROXY_DEBUG_PORT:-9097}" ;;
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
code="$(curl -sS --max-time "$(( secs + 45 ))" -w '%{http_code}' \
  "http://$lp/debug/flamegraph?seconds=$secs" -o "$out")" \
  || { echo "capture failed: curl exit $?" >&2; exit 1; }
case "$code" in
  200) ;;
  501) rm -f "$out"
       echo "capture refused: the image is no --features flamegraph build" >&2; exit 1 ;;
  *) echo "capture failed: HTTP $code ($(head -c 200 "$out"))" >&2; rm -f "$out"; exit 1 ;;
esac
echo ">> wrote $out ($(wc -c < "$out") bytes) — open in a browser"
