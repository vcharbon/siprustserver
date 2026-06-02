#!/usr/bin/env bash
# 2-hour endurance + chaos orchestrator for the Rust SIP SUT on kind.
#
# Drives the realistic steady-state profile the chaos suite is meant to run
# against, and injects one chaos event every CHAOS_INTERVAL, cycling through
# ALL chaos elements:
#
#   baseline (always on) — existing vendored scenarios, used AS-IS:
#     long  calls  uac-long-options.xml     @ LONG_CPS  (in-dialog OPTIONS-driven
#                                                        hold -> very high conc.)
#     short calls  uac-endurance-short.xml  @ SHORT_CPS (30s  hold)
#     abuse        uac-abuse-options-flood  @ ABUSE_CAPS (default 1cps)
#   chaos cycle (every CHAOS_INTERVAL):
#     kill_worker  ->  kill_proxy  ->  peak(200cps burst)  ->  (repeat)
#
# Every SIPp stream reports live via its native-sidecar exporter (scraped by
# vmagent exactly like cluster start); every chaos event is measured from those
# metrics, pushed to VictoriaMetrics, and appended to events.jsonl. The
# dedicated Grafana dashboard (sipp-endurance-chaos) renders all of it.
#
#   ./endurance.sh run        # full 2h run (wire-up + baseline + chaos loop)
#   SMOKE=1 ./endurance.sh run # ~10min validation: one of each chaos event
#   ./endurance.sh wireup     # just (re)build sipp image + deploy(repl) + dashboard
#   ./endurance.sh stop       # stop baseline + abuse streams (leave cluster up)
#
# Env knobs (defaults shown):
#   DURATION=7200 CHAOS_INTERVAL=900 LONG_CPS=5 SHORT_CPS=100 ABUSE_CAPS=1
#   PEAK_CAPS=200 PEAK_SECS=30 WORKER_REPLICAS=2 PASS_THRESHOLD=90
#   SMOKE=1 -> DURATION=600 CHAOS_INTERVAL=180
set -euo pipefail
cd "$(dirname "$0")"
HERE="$(pwd)"
REPO_ROOT="$(cd ../.. && pwd)"

NS="${NS:-sip-test}"
CLUSTER="${CLUSTER:-sip-e2e}"
SIPP_DIR="$HERE/sipp"
OBS_DIR="$REPO_ROOT/deploy/observability"
VM="${VM:-http://127.0.0.1:8428}"
VM_IMPORT="$VM/api/v1/import/prometheus"

WORKER_REPLICAS="${WORKER_REPLICAS:-2}"
LONG_CPS="${LONG_CPS:-5}"
SHORT_CPS="${SHORT_CPS:-100}"
ABUSE_CAPS="${ABUSE_CAPS:-1}"
PEAK_CAPS="${PEAK_CAPS:-200}"
PEAK_SECS="${PEAK_SECS:-30}"
ORPHAN_CAPS="${ORPHAN_CAPS:-50}"
ORPHAN_BUILD_SECS="${ORPHAN_BUILD_SECS:-20}"
ORPHAN_REAP_WAIT="${ORPHAN_REAP_WAIT:-330}"  # must clear the 300s keepalive: orphan A-leg
                                             # OPTIONS goes unanswered at 300s -> reap ~305s
PASS_THRESHOLD="${PASS_THRESHOLD:-90}"
SETTLE="${SETTLE:-60}"   # seconds to let an event's effect resolve before measuring
                         # (sized so proxy-kill new-call failures, which only
                         #  resolve ~32s after the ~30s outage, are attributed
                         #  to the event that caused them)

if [ "${SMOKE:-0}" = "1" ]; then
  DURATION="${DURATION:-600}"
  CHAOS_INTERVAL="${CHAOS_INTERVAL:-180}"
else
  DURATION="${DURATION:-7200}"
  CHAOS_INTERVAL="${CHAOS_INTERVAL:-900}"
fi

TS="$(date +%Y%m%d-%H%M%S)"
RUN_DIR="$HERE/results/endurance-$TS"
EVENTS="$RUN_DIR/events.jsonl"
RUNLOG="$RUN_DIR/run.log"

log()  { printf '\033[1;36m>> %s\033[0m\n' "$*" | tee -a "$RUNLOG" >&2; }
ok()   { printf '\033[1;32mOK: %s\033[0m\n' "$*" | tee -a "$RUNLOG" >&2; }
warn() { printf '\033[1;33mWARN: %s\033[0m\n' "$*" | tee -a "$RUNLOG" >&2; }

push_metric() { curl -s --max-time 4 -X POST "$VM_IMPORT" --data-binary "$1" >/dev/null 2>&1 || true; }

# Chaos WINDOW marker: a per-event gauge that is 1 for the whole real duration of
# one chaos event and 0 otherwise. Grafana renders the contiguous run of 1s as a
# single region annotation, so each event shows its TRUE start→end span (no more
# 5-min staleness smear of overlapping instant markers). A background heartbeat
# re-pushes 1 every 60s so VM's lookback never leaves a hole mid-event (kill_worker
# can run >5min: pod-ready wait + redeploy + settle).
WINDOW_HB_PID=""
open_window() {  # $1 = chaos type
  push_metric "sip_chaos_window{type=\"$1\"} 1"
  ( while sleep 60; do push_metric "sip_chaos_window{type=\"$1\"} 1"; done ) &
  WINDOW_HB_PID=$!
}
close_window() { # $1 = chaos type
  [ -n "$WINDOW_HB_PID" ] && kill "$WINDOW_HB_PID" >/dev/null 2>&1 || true
  WINDOW_HB_PID=""
  push_metric "sip_chaos_window{type=\"$1\"} 0"
}

# vmq <promql> -> scalar value of the first result (0 if none / unreachable).
vmq() {
  curl -s --max-time 5 --data-urlencode "query=$1" "$VM/api/v1/query" 2>/dev/null \
    | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin); r = d['data']['result']
    print(r[0]['value'][1] if r else '0')
except Exception:
    print('0')
" 2>/dev/null || echo 0
}

# Launch (or replace) a long-lived UAC stream from the shared job manifest.
launch_stream() {
  local job="$1" scenario="$2" cps="$3" role="$4"
  export UAC_JOB_NAME="$job" SCENARIO="$scenario" CAPS="$cps" ROLE="$role" \
         MAX_CALLS=$(( cps * (DURATION + 600) )) \
         MAX_CONCURRENT="${MAX_CONCURRENT:-$(( cps * 600 ))}"
  kubectl -n "$NS" delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
  envsubst < manifests/40-sipp-uac-job.yaml | kubectl apply -f - >/dev/null
}

# Re-create any baseline stream whose job has vanished or failed (keeps the
# steady state alive across the whole window even if a job hits MAX_CALLS).
ensure_baseline() {
  local s
  for s in "sipp-uac-long uac-long-options.xml $LONG_CPS long" \
           "sipp-uac-short uac-endurance-short.xml $SHORT_CPS short"; do
    set -- $s
    local active
    active="$(kubectl -n "$NS" get job "$1" -o jsonpath='{.status.active}' 2>/dev/null || echo)"
    if [ "${active:-0}" != "1" ]; then
      # Capture WHY it died before relaunching, so the monitoring loop / a
      # subagent investigation has the dead pod's status + tail of its logs.
      local dump="$RUN_DIR/dead-$1-$(date +%s)"
      { kubectl -n "$NS" describe job "$1" 2>&1
        echo "--- last pod logs (sipp) ---"
        kubectl -n "$NS" logs "job/$1" -c sipp-uac --tail=60 2>&1
        echo "--- last pod logs (stat-exporter) ---"
        kubectl -n "$NS" logs "job/$1" -c stat-exporter --tail=20 2>&1
      } > "$dump.txt" 2>&1 || true
      warn "baseline stream $1 not active (active=${active:-none}) — diagnostics in ${dump##*/}.txt — relaunching"
      push_metric "sip_endurance_stream_restart{stream=\"$4\"} 1"
      launch_stream "$1" "$2" "$3" "$4"
    fi
  done
  # Abuse stream via chaos.sh (handles its own metric markers).
  if ! kubectl -n "$NS" get job sipp-uac-abuse >/dev/null 2>&1; then
    ABUSE_CAPS="$ABUSE_CAPS" ./chaos.sh abuse up >>"$RUNLOG" 2>&1 || true
  fi
}

# Sum baseline (long+short) cumulative outcome counters from the exporters.
snap_success() { vmq 'sum(sipp_successful_calls_total{role=~"long|short"})'; }
snap_failed()  { vmq 'sum(sipp_failed_calls_total{role=~"long|short"})'; }
snap_conc()    { vmq 'sum(sipp_current_calls)'; }
# B2BUA-side live dialog count + the GHOST GAP (calls the B2BUA still holds that
# SIPp has already abandoned). The gap cancels baseline level/ramp, so it is the
# robust leak signal: a healthy system keeps b2bua_active ≈ sipp_current (gap ~0
# + scrape skew); a leak makes the gap rise and stay risen.
snap_active() { vmq 'sum(b2bua_active_calls)'; }
snap_ghost()  { vmq 'sum(b2bua_active_calls) - sum(sipp_current_calls)'; }

# orphan_kill measurement: launch a dedicated 50cps stream, abruptly kill the UAC
# mid-call (no BYE), then assert the B2BUA REAPS the orphaned dialogs. Measured by
# the ghost gap, not a raw active delta, so the ~3-4k churning baseline doesn't
# swamp the signal. With keepalive at 300s the orphans reap ~305s after the kill,
# so ORPHAN_REAP_WAIT must clear 300s. A ghost gap that stays risen = leak.
ORPHAN_GHOST_TOL="${ORPHAN_GHOST_TOL:-250}"
orphan_event() {
  local idx="$1" t0 g0 gpk a0 g1 a1 rise result
  t0="$(date +%s)"
  g0="$(snap_ghost)"; a0="$(snap_active)"
  log "CHAOS #$idx: orphan_kill (ghost_gap before=$g0, b2bua active=$a0)"
  push_metric 'sip_chaos_run{type="orphan_kill",phase="inject"} 1'
  open_window orphan_kill
  ORPHAN_CAPS="$ORPHAN_CAPS" ORPHAN_BUILD_SECS="$ORPHAN_BUILD_SECS" \
    ./chaos.sh orphankill >>"$RUNLOG" 2>&1 || true
  gpk="$(snap_ghost)"
  log "orphan_kill: ghost gap spiked to $gpk; waiting ${ORPHAN_REAP_WAIT}s (> 300s keepalive) for reap"
  sleep "$ORPHAN_REAP_WAIT"
  ensure_baseline
  g1="$(snap_ghost)"; a1="$(snap_active)"
  close_window orphan_kill
  rise=$(python3 -c "print(round(float('$g1')-float('$g0'),0))")
  # Pass if the ghost gap returned near baseline (orphans reaped within tolerance).
  result=$(python3 -c "print('pass' if float('$rise') <= $ORPHAN_GHOST_TOL else 'fail')")
  push_metric "sip_chaos_event{type=\"orphan_kill\",result=\"$result\"} 1
sip_chaos_ghost_rise $rise
sip_chaos_ghost_gap $g1"
  printf '{"ts":%s,"event":%d,"type":"orphan_kill","ghost_before":%s,"ghost_peak":%s,"ghost_after":%s,"ghost_rise":%s,"active_before":%s,"active_after":%s,"result":"%s"}\n' \
    "$t0" "$idx" "${g0%.*}" "${gpk%.*}" "${g1%.*}" "${rise%.*}" "${a0%.*}" "${a1%.*}" "$result" >> "$EVENTS"
  if [ "$result" = "pass" ]; then
    ok "CHAOS #$idx orphan_kill: REAPED — ghost gap $g0 ->(spike $gpk)-> $g1 (rise ${rise}, tol ${ORPHAN_GHOST_TOL})"
  else
    warn "CHAOS #$idx orphan_kill: LEAK — ghost gap stayed risen ${g0}->${g1} (rise ${rise} > ${ORPHAN_GHOST_TOL}) — FAILURE"
  fi
}

# Record one chaos event: snapshot baseline outcomes, run it, settle, snapshot
# again, compute the success% of calls that resolved across the window, push a
# metric + append a JSONL row. Flags result=fail if below PASS_THRESHOLD.
chaos_event() {
  local type="$1" idx="$2"
  # orphan_kill has a bespoke measurement (B2BUA active_calls reaping), not the
  # baseline success-rate path the other events share.
  if [ "$type" = "orphan_kill" ]; then orphan_event "$idx"; return; fi
  local t0 s0 f0 s1 f1 ds df pct result conc
  t0="$(date +%s)"
  s0="$(snap_success)"; f0="$(snap_failed)"
  log "CHAOS #$idx: $type (baseline success=$s0 failed=$f0)"
  push_metric "sip_chaos_run{type=\"$type\",phase=\"inject\"} 1"
  open_window "$type"

  local tgt
  case "$type" in
    kill_worker)
      tgt="b2bua-worker-$(( idx % WORKER_REPLICAS ))"
      # Graceful kill so the worker flushes its changelog to the backup before
      # exit (in-dialog BYEs survive). KILL_MODE=crash forces a hard kill.
      [ "${KILL_MODE:-graceful}" = "crash" ] && KILL_GRACE=0 || KILL_GRACE=10
      KILL_GRACE="$KILL_GRACE" KILL_TARGET="$tgt" WORKER_REPLICAS="$WORKER_REPLICAS" \
        ./chaos.sh kill >>"$RUNLOG" 2>&1 || true
      # The StatefulSet recreates the pod with a NEW IP; the proxy's PROXY_WORKERS
      # is IP-literal, so without a registry refresh the proxy can never route to
      # the recovered worker -> the run goes single-worker. run.sh deploy
      # re-resolves live pod IPs and redeploys the proxy.
      kubectl -n "$NS" wait --for=condition=ready "pod/$tgt" --timeout=150s >>"$RUNLOG" 2>&1 || true
      REPL_ENABLE=1 WORKER_REPLICAS="$WORKER_REPLICAS" OBS_ENABLE=0 ./run.sh deploy >>"$RUNLOG" 2>&1 || true ;;
    kill_proxy)
      ./chaos.sh proxykill >>"$RUNLOG" 2>&1 || true ;;
    peak)
      PEAK_CAPS="$PEAK_CAPS" PEAK_SECS="$PEAK_SECS" ./chaos.sh peak >>"$RUNLOG" 2>&1 || true ;;
  esac

  sleep "$SETTLE"
  ensure_baseline
  s1="$(snap_success)"; f1="$(snap_failed)"; conc="$(snap_conc)"
  ds=$(python3 -c "print(max(0, int(float('$s1'))-int(float('$s0'))))")
  df=$(python3 -c "print(max(0, int(float('$f1'))-int(float('$f0'))))")
  pct=$(python3 -c "t=$ds+$df; print(round(100.0*$ds/t,1) if t else 100.0)")
  # Per-type pass bar. kill_proxy is a single-replica SPOF by design (proxy HA is
  # out of scope for this runner) -> a ~30s new-call gap is expected, so its bar
  # is lenient; established dialogs still survive via Record-Route pinning.
  local thr="$PASS_THRESHOLD"
  case "$type" in
    kill_proxy) thr="${PROXY_THRESHOLD:-60}" ;;
    peak)       thr="${PEAK_THRESHOLD:-90}" ;;
  esac
  # A window where NO baseline calls resolved (ds+df==0) is not a real pass — it
  # means the baseline streams were down/relaunching. Report it honestly as n/a
  # rather than a vacuous 100%.
  if [ "$(( ds + df ))" -eq 0 ]; then
    result="n/a"
  else
    result=$(python3 -c "print('pass' if float('$pct')>=$thr else 'fail')")
  fi

  close_window "$type"
  push_metric "sip_chaos_event{type=\"$type\",result=\"$result\"} 1
sip_chaos_success_pct{type=\"$type\"} $pct
sip_chaos_resolved{type=\"$type\",outcome=\"success\"} $ds
sip_chaos_resolved{type=\"$type\",outcome=\"failed\"} $df"

  printf '{"ts":%s,"event":%d,"type":"%s","success_delta":%d,"failed_delta":%d,"success_pct":%s,"concurrent":%s,"result":"%s"}\n' \
    "$t0" "$idx" "$type" "$ds" "$df" "$pct" "${conc%.*}" "$result" >> "$EVENTS"

  case "$result" in
    pass) ok   "CHAOS #$idx $type: ${pct}% resolved OK (thr ${thr}%, Δok=$ds Δfail=$df)" ;;
    n/a)  warn "CHAOS #$idx $type: no baseline calls resolved in window (streams down?) — n/a" ;;
    *)    warn "CHAOS #$idx $type: ${pct}% < ${thr}% (Δok=$ds Δfail=$df) — FAILURE" ;;
  esac
}

wireup() {
  mkdir -p "$RUN_DIR"
  log "wireup: rebuilding sipp:dev (with python3 exporter) + loading into kind"
  docker build -t sipp:dev "$SIPP_DIR" >>"$RUNLOG" 2>&1
  kind load docker-image sipp:dev --name "$CLUSTER" >>"$RUNLOG" 2>&1
  log "wireup: deploy stack (repl on, ${WORKER_REPLICAS} workers) — same path as cluster start"
  REPL_ENABLE=1 WORKER_REPLICAS="$WORKER_REPLICAS" OBS_ENABLE="${OBS_ENABLE:-1}" ./run.sh deploy >>"$RUNLOG" 2>&1
  log "wireup: (re)load observability dashboards/scrape"
  [ -x "$OBS_DIR/install.sh" ] && "$OBS_DIR/install.sh" --apply >>"$RUNLOG" 2>&1 || true
  ok "wireup complete"
}

start_baseline() {
  log "starting baseline streams: long@${LONG_CPS} short@${SHORT_CPS} abuse@${ABUSE_CAPS}"
  launch_stream sipp-uac-long  uac-long-options.xml    "$LONG_CPS"  long
  launch_stream sipp-uac-short uac-endurance-short.xml "$SHORT_CPS" short
  ABUSE_CAPS="$ABUSE_CAPS" ./chaos.sh abuse up >>"$RUNLOG" 2>&1 || true
  push_metric "sip_endurance_run{phase=\"start\"} 1"
}

stop_streams() {
  log "stopping baseline + abuse streams"
  for j in sipp-uac-long sipp-uac-short sipp-uac-peak sipp-uac-orphan; do
    kubectl -n "$NS" delete job "$j" --ignore-not-found >/dev/null 2>&1 || true
  done
  ./chaos.sh abuse down >>"$RUNLOG" 2>&1 || true
  push_metric "sip_endurance_run{phase=\"stop\"} 1"
}

run() {
  mkdir -p "$RUN_DIR"; : > "$EVENTS"
  log "=== ENDURANCE RUN $TS (smoke=${SMOKE:-0}) dur=${DURATION}s interval=${CHAOS_INTERVAL}s ==="
  log "results: $RUN_DIR"
  wireup
  # Gate readiness before driving traffic.
  kubectl -n "$NS" wait --for=condition=ready pod -l app=b2bua-worker --timeout=120s >>"$RUNLOG" 2>&1 || true
  kubectl -n "$NS" rollout status deploy/sip-front-proxy --timeout=90s >>"$RUNLOG" 2>&1 || true
  start_baseline

  local cycle=(orphan_kill kill_worker kill_proxy peak)
  local start now idx=0
  start="$(date +%s)"
  # Let the steady state build before the first event.
  log "warmup ${CHAOS_INTERVAL}s before first chaos event"
  sleep "$CHAOS_INTERVAL"
  while :; do
    now="$(date +%s)"
    [ $(( now - start )) -ge "$DURATION" ] && break
    ensure_baseline
    chaos_event "${cycle[$(( idx % ${#cycle[@]} ))]}" "$idx"
    idx=$(( idx + 1 ))
    # Remaining sleep until the next interval boundary.
    local elapsed_since=$(( $(date +%s) - now ))
    local rest=$(( CHAOS_INTERVAL - elapsed_since ))
    [ "$rest" -gt 0 ] && sleep "$rest" || true
  done

  log "=== run window elapsed — $idx chaos events injected ==="
  # Final outcome snapshot.
  local fs ff
  fs="$(snap_success)"; ff="$(snap_failed)"
  log "final baseline totals: success=$fs failed=$ff"
  if [ "${KEEP:-0}" = "1" ]; then
    log "KEEP=1 — leaving streams running"
  else
    stop_streams
  fi
  ok "endurance run complete — events in $EVENTS"
}

trap 'warn "interrupted — stopping streams"; [ -n "$WINDOW_HB_PID" ] && kill "$WINDOW_HB_PID" 2>/dev/null; stop_streams || true' INT TERM

cmd="${1:-run}"; shift || true
case "$cmd" in
  run)    run ;;
  wireup) wireup ;;
  stop)   stop_streams ;;
  *) printf 'usage: %s {run|wireup|stop}\n' "$0" >&2; exit 1 ;;
esac
