#!/usr/bin/env bash
# Localhost 3-tier SIP perf baseline: the whole stack on loopback, no k8s, no
# docker, no VIP.
#
# Topology (all on 127.0.0.1):
#
#     loadgen uac :6000 --INVITE--> sip-proxy-runner :5060
#                                      --INVITE--> b2bua-runner :5080
#                                                     --INVITE--> loadgen uas :6001
#
# One process per tier, each pinned into its own systemd scope (bounded CPU +
# memory) so the tiers cannot fight over the box and the measurement repeats.
#
# ## The metric
#
# **CPU-seconds per call, per tier.** `utime+stime` is read out of
# `/proc/<pid>/stat` at both ends of a steady-state window and divided by the
# calls the loadgen completed in that same window. Unlike achieved cps it does
# not move when the box is busy with something else, so it is the number to
# compare a before-run against an after-run with. Reported alongside: offered vs
# achieved cps, the loadgen result-class breakdown, and peak RSS (`VmHWM`).
#
# ## The windows
#
# Each run has TWO disjoint windows, deliberately not overlapped:
#
#   ramp (RAMP s) → cpu window (WINDOW s) → flamegraph window (FLAME s) → drain
#
# The pprof sampler costs CPU, so profiling inside the CPU-seconds window would
# inflate exactly the number being measured. The flamegraph window runs after
# it, and all three tiers are captured across the same wall-clock span so their
# hotspot rankings are directly comparable.
#
# ## Usage
#
#     ./run-3tier.sh [runs] [cps] [duration]      # default: 3 100 90
#
# Env knobs: TAG (results subdir, default baseline-pre-zerocopy), RAMP, WINDOW,
# FLAME, SCENARIOS (a `--scenario` flag string; empty = the registry default
# mix), BIN_DIR.
#
# Results (raw logs, SVGs, per-run CSV, summary.md) land in
# `results/$TAG/`.

set -u
cd "$(dirname "$0")"

RUNS="${1:-3}"
CPS="${2:-100}"
DURATION="${3:-90}"

TAG="${TAG:-baseline-pre-zerocopy}"
BIN_DIR="${BIN_DIR:-/home/vince/siprustserver/target/release}"
RAMP="${RAMP:-20}"      # let the long-tail shapes (options_hold) reach steady state
WINDOW="${WINDOW:-30}"  # CPU-seconds window
FLAME="${FLAME:-30}"    # flamegraph window, after the CPU window
# Empty = the shape registry's default mix. `--scenario basic_call=1` pins the
# single-shape variant.
SCENARIOS="${SCENARIOS:-}"

RESULTS="results/$TAG"
TICK=$(getconf CLK_TCK)

# SIP + observability ports. The observability ports are deliberately off the
# 9090/9091 defaults — those collide with a developer's local Prometheus.
PROXY_SIP=127.0.0.1:5060
B2BUA_SIP=127.0.0.1:5080
LG_BASE=6000            # uac=6000, uas=6001, refer=6002
PROXY_HTTP=127.0.0.1:19090
B2BUA_HTTP=127.0.0.1:19091
LG_HTTP=127.0.0.1:19300

mkdir -p "$RESULTS"
CSV="$RESULTS/runs.csv"

# ---------------------------------------------------------------- helpers ----

# Launch `$@` inside a transient systemd scope with the given memory/CPU caps,
# writing the real process pid (not systemd-run's) to $pidfile.
scope_run() {
  local name="$1" mem="$2" cpu="$3" pidfile="$4" log="$5"; shift 5
  systemd-run --user --scope -q --unit="$name" -p MemoryMax="$mem" -p CPUQuota="$cpu" \
    nice -n 10 bash -c 'echo $$ > "$0"; exec "$@"' "$pidfile" "$@" \
    > "$log" 2>&1 &
  for _ in $(seq 1 50); do [ -s "$pidfile" ] && return 0; sleep 0.1; done
  echo "!! $name never reported a pid" >&2
  return 1
}

# Stop the given pids and wait for them to go. SIGTERM first — the runners drain
# on it — then SIGKILL whatever is still up. A run must not start until the
# previous one's sockets are gone: the b2bua's drain grace outlives the kill, and
# a proxy that finds :5060 still bound dies at boot and the whole run times out.
stop_and_wait() {
  for p in "$@"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  for _ in $(seq 1 150); do
    local alive=0
    for p in "$@"; do kill -0 "$p" 2>/dev/null && alive=1; done
    [ "$alive" = 0 ] && return 0
    sleep 0.1
  done
  for p in "$@"; do [ -n "$p" ] && kill -9 "$p" 2>/dev/null; done
  sleep 0.5
}

# Block until no UDP socket is bound on any of the run's SIP ports.
wait_ports_free() {
  local want="${PROXY_SIP##*:}|${B2BUA_SIP##*:}|$LG_BASE|$((LG_BASE + 1))|$((LG_BASE + 2))"
  for _ in $(seq 1 150); do
    ss -lun 2>/dev/null | awk -v re=":($want)\$" '$5 ~ re' | grep -q . || return 0
    sleep 0.2
  done
  echo "!! SIP ports still bound after 30s" >&2
  return 1
}

# Fail the run loudly rather than measuring a tier that died at boot.
assert_alive() {
  local name="$1" pid="$2" log="$3"
  kill -0 "$pid" 2>/dev/null && return 0
  echo "!! $name (pid $pid) died at startup — tail of $log:" >&2
  tail -5 "$log" >&2
  return 1
}

# utime+stime of a pid, in clock ticks (fields 14+15 of /proc/<pid>/stat).
cpu_ticks() { awk '{print $14 + $15}' "/proc/$1/stat" 2>/dev/null || echo 0; }

# Peak RSS (VmHWM) in kB.
peak_rss_kb() { awk '/^VmHWM:/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }

# Total completed calls across every (scenario, class, chaos) series.
calls_total() {
  curl -s -m 5 "http://$LG_HTTP/metrics" \
    | awk '/^loadgen_calls_total\{/ {s += $NF} END {printf "%d", s+0}'
}

cleanup() {
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
}
trap cleanup EXIT

# -------------------------------------------------------------- the runs ----

echo "run,tier,cpu_s,calls,cpu_s_per_call,peak_rss_mb" > "$CSV"
echo "3-tier localhost baseline: runs=$RUNS cps=$CPS duration=${DURATION}s" \
     "(ramp ${RAMP}s, cpu window ${WINDOW}s, flame window ${FLAME}s) -> $RESULTS"

for run in $(seq 1 "$RUNS"); do
  echo
  echo "== run $run/$RUNS =="
  PIDS=()
  rm -f "$RESULTS"/r${run}_*.pid
  wait_ports_free || exit 1

  # Both proxy admission gates are opened well past the offered rate. They are
  # calibrated for a 2-worker cluster (self-gate 100 cps; per-worker AIMD cap
  # starts at 30 cps and climbs 2 cps per OPTIONS tick), so at their defaults a
  # single-worker 100 cps run measures the *admission controller* — a third of
  # the calls 503 before reaching the b2bua. This baseline measures the SIP
  # datapath, so the gates are lifted, not exercised.
  PROXY_LISTEN=$PROXY_SIP PROXY_ADVERTISE=$PROXY_SIP \
  PROXY_WORKERS=w0@$B2BUA_SIP PROXY_METRICS=$PROXY_HTTP \
  PROXY_SELF_GATE_CPS_RATE=1000 PROXY_SELF_GATE_CPS_SIZE=500 \
  LB_CAP_CEILING_CPS=2000 LB_AIMD_INCREASE_STEP_CPS=500 \
    scope_run "b3t-proxy-$run" 2G 400% "$RESULTS/r${run}_proxy.pid" \
      "$RESULTS/r${run}_proxy.log" "$BIN_DIR/sip-proxy-runner" || exit 1
  PX=$(cat "$RESULTS/r${run}_proxy.pid"); PIDS+=("$PX")

  B2BUA_LISTEN=$B2BUA_SIP B2BUA_ADVERTISE=$B2BUA_SIP \
  B2BUA_DEST=127.0.0.1:$((LG_BASE + 1)) B2BUA_METRICS=$B2BUA_HTTP \
  B2BUA_RELAY_HEADERS=X-Loadgen-Id \
    scope_run "b3t-b2bua-$run" 4G 600% "$RESULTS/r${run}_b2bua.pid" \
      "$RESULTS/r${run}_b2bua.log" "$BIN_DIR/b2bua-runner" || exit 1
  B2=$(cat "$RESULTS/r${run}_b2bua.pid"); PIDS+=("$B2")

  sleep 3
  assert_alive proxy "$PX" "$RESULTS/r${run}_proxy.log" || exit 1
  assert_alive b2bua "$B2" "$RESULTS/r${run}_b2bua.log" || exit 1

  # shellcheck disable=SC2086  # SCENARIOS is a deliberate multi-flag string
  scope_run "b3t-loadgen-$run" 6G 900% "$RESULTS/r${run}_loadgen.pid" \
    "$RESULTS/r${run}_loadgen.log" \
    "$BIN_DIR/loadgen" --target "$PROXY_SIP" --bind-ip 127.0.0.1 \
      --base-port "$LG_BASE" --cps "$CPS" --duration "$DURATION" \
      --options-hold 10 --metrics-addr "$LG_HTTP" \
      --out-dir "$RESULTS/r${run}_report" $SCENARIOS || exit 1
  LG=$(cat "$RESULTS/r${run}_loadgen.pid"); PIDS+=("$LG")

  echo "-- pids: proxy=$PX b2bua=$B2 loadgen=$LG; ramp ${RAMP}s"
  sleep "$RAMP"
  assert_alive proxy "$PX" "$RESULTS/r${run}_proxy.log" || exit 1
  assert_alive b2bua "$B2" "$RESULTS/r${run}_b2bua.log" || exit 1
  assert_alive loadgen "$LG" "$RESULTS/r${run}_loadgen.log" || exit 1

  # ---- CPU-seconds window (no profiler running) -----------------------------
  c0=$(calls_total)
  px0=$(cpu_ticks "$PX"); b20=$(cpu_ticks "$B2"); lg0=$(cpu_ticks "$LG")
  sleep "$WINDOW"
  c1=$(calls_total)
  px1=$(cpu_ticks "$PX"); b21=$(cpu_ticks "$B2"); lg1=$(cpu_ticks "$LG")
  calls=$((c1 - c0))
  echo "-- cpu window: $calls calls in ${WINDOW}s"

  # ---- flamegraph window (all three tiers, one coherent span) ---------------
  echo "-- flamegraph window ${FLAME}s"
  curl -s -m $((FLAME + 30)) "http://$PROXY_HTTP/debug/flamegraph?seconds=$FLAME" \
       -o "$RESULTS/r${run}_proxy.svg" & fp=$!
  curl -s -m $((FLAME + 30)) "http://$B2BUA_HTTP/debug/flamegraph?seconds=$FLAME" \
       -o "$RESULTS/r${run}_b2bua.svg" & fb=$!
  curl -s -m $((FLAME + 30)) "http://$LG_HTTP/debug/flamegraph?seconds=$FLAME" \
       -o "$RESULTS/r${run}_loadgen.svg" & fl=$!
  wait "$fp" "$fb" "$fl"

  # Final /metrics snapshots before the tiers go away.
  curl -s -m 5 "http://$LG_HTTP/metrics"    > "$RESULTS/r${run}_loadgen_metrics.txt"
  curl -s -m 5 "http://$PROXY_HTTP/metrics" > "$RESULTS/r${run}_proxy_metrics.txt"
  curl -s -m 5 "http://$B2BUA_HTTP/metrics" > "$RESULTS/r${run}_b2bua_metrics.txt"

  px_rss=$(peak_rss_kb "$PX"); b2_rss=$(peak_rss_kb "$B2"); lg_rss=$(peak_rss_kb "$LG")

  # ---- let the offered load finish + drain ---------------------------------
  for _ in $(seq 1 120); do kill -0 "$LG" 2>/dev/null || break; sleep 1; done
  stop_and_wait "$LG" "$B2" "$PX"
  PIDS=()

  for tier in proxy:$px0:$px1:$px_rss b2bua:$b20:$b21:$b2_rss loadgen:$lg0:$lg1:$lg_rss; do
    IFS=: read -r name t0 t1 rss <<< "$tier"
    awk -v run="$run" -v name="$name" -v d="$((t1 - t0))" -v tick="$TICK" \
        -v calls="$calls" -v rss="$rss" \
        'BEGIN { cpu = d / tick;
                 printf "%d,%s,%.2f,%d,%.5f,%.1f\n", run, name, cpu, calls,
                        (calls > 0 ? cpu / calls : 0), rss / 1024 }' >> "$CSV"
  done
done

# ------------------------------------------------------------- reporting ----

# Self-time attribution per tier, summed over the runs' SVGs (three short
# captures make one statistically usable profile).
for t in loadgen proxy b2bua; do
  echo "################ $t ################"
  python3 ./flame-attrib.py "$RESULTS"/r*_${t}.svg
  echo
done > "$RESULTS/flame-attribution.txt" 2>&1

{
  echo "# 3-tier localhost perf baseline — \`$TAG\`"
  echo
  echo "- offered: **$CPS cps** for ${DURATION}s, $RUNS runs"
  echo "- windows: ramp ${RAMP}s, CPU-seconds ${WINDOW}s, flamegraph ${FLAME}s (disjoint)"
  echo "- scenario mix: ${SCENARIOS:-registry default mix}"
  echo "- topology: loadgen:$LG_BASE -> proxy:${PROXY_SIP##*:} -> b2bua:${B2BUA_SIP##*:} -> loadgen:$((LG_BASE + 1))"
  echo "- scopes: proxy 400% / b2bua 600% / loadgen 900% CPUQuota"
  echo
  echo '## CPU-seconds per call (the comparison metric)'
  echo
  echo '| tier | mean cpu-s/call | min | max | spread | mean cpu-s / window | mean peak RSS (MB) |'
  echo '|---|---|---|---|---|---|---|'
  for t in loadgen proxy b2bua; do
    awk -F, -v t="$t" '$2 == t {
        n++; s += $5; c += $3; r += $6;
        if (mn == "" || $5 < mn) mn = $5; if ($5 > mx) mx = $5 }
      END { if (n) printf "| %s | %.5f | %.5f | %.5f | %.1f%% | %.2f | %.1f |\n",
                          t, s/n, mn, mx, (s/n > 0 ? 100*(mx-mn)/(s/n) : 0), c/n, r/n }' "$CSV"
  done
  awk -F, 'NR > 1 { by[$1] += $5; n[$1] = 1 }
    END { tot = 0; k = 0; for (r in by) { tot += by[r]; k++;
            if (mn == "" || by[r] < mn) mn = by[r]; if (by[r] > mx) mx = by[r] }
          if (k) printf "| **total** | %.5f | %.5f | %.5f | %.1f%% | | |\n",
                        tot/k, mn, mx, (tot/k > 0 ? 100*(mx-mn)/(tot/k) : 0) }' "$CSV"
  echo
  echo '## Throughput'
  echo
  echo '| run | calls in window | achieved cps | offered cps |'
  echo '|---|---|---|---|'
  awk -F, -v w="$WINDOW" -v o="$CPS" '$2 == "loadgen" {
        printf "| %s | %d | %.1f | %s |\n", $1, $4, $4/w, o }' "$CSV"
  echo
  echo '## Result classes (whole run — the loadgen exit dump, after drain)'
  echo
  echo '| run | scenario | class | count |'
  echo '|---|---|---|---|'
  for run in $(seq 1 "$RUNS"); do
    awk -v run="$run" -F'[{}]' '/^loadgen_calls_total\{/ {
        split($2, kv, ","); sc = ""; cl = "";
        for (i in kv) { split(kv[i], p, "="); gsub(/"/, "", p[2]);
                        if (p[1] == "scenario") sc = p[2]; if (p[1] == "class") cl = p[2] }
        n = $3; gsub(/ /, "", n);
        if (n + 0 > 0) printf "| %s | %s | %s | %d |\n", run, sc, cl, n }' \
      "$RESULTS/r${run}_loadgen.log" 2>/dev/null
  done
  echo
  echo '## Canaries (loadgen exit dump — `registry_size` must be 0)'
  echo
  echo '| run | calls | ok | ok% | shed | orphans | registry_size | inbox_drops | ringing recv/expected |'
  echo '|---|---|---|---|---|---|---|---|---|'
  for run in $(seq 1 "$RUNS"); do
    awk -v run="$run" '
      /^loadgen_calls_total\{/         { tot += $NF; if ($0 ~ /class="ok"/) ok += $NF }
      /^loadgen_shed_total\{/          { shed += $NF }
      /^loadgen_mux_orphan_total\{/    { orph += $NF }
      /^loadgen_mux_registry_size /    { reg = $NF }
      /^loadgen_mux_inbox_drop_total / { drop = $NF }
      /^loadgen_ringing_expected_total/{ want = $NF }
      /^loadgen_ringing_received_total/{ got = $NF }
      END { printf "| %s | %d | %d | %.1f%% | %d | %d | %d | %d | %d/%d |\n",
                   run, tot+0, ok+0, (tot > 0 ? 100*ok/tot : 0),
                   shed+0, orph+0, reg+0, drop+0, got+0, want+0 }' \
      "$RESULTS/r${run}_loadgen.log" 2>/dev/null
  done
  echo
  echo '## Flamegraph self-time buckets (all runs summed — full table in `flame-attribution.txt`)'
  echo
  echo '| tier | on-CPU samples | parse bucket | alloc bucket (043 frames) | alloc bucket (incl. allocator) | sip-message subtree (inclusive) |'
  echo '|---|---|---|---|---|---|'
  awk '/^################/ { tier = $2 }
       /^files:/          { n = $NF }
       /^PARSE bucket/    { p = $NF }
       /^ALLOC bucket \(043/ { a = $NF }
       /^ALLOC bucket \(incl/ { af = $NF }
       /^sip-message subtree/ { printf "| %s | %s | %s | %s | %s | %s |\n", tier, n, p, a, af, $NF }' \
    "$RESULTS/flame-attribution.txt"
  echo
  echo '## Raw'
  echo
  echo 'Per-run CSV: `runs.csv`. Flamegraph SVGs: `r<N>_{proxy,b2bua,loadgen}.svg`.'
  echo 'Per-run loadgen report: `r<N>_report/index.html`.'
} > "$RESULTS/summary.md"

echo
echo "Done. Summary -> $RESULTS/summary.md"
