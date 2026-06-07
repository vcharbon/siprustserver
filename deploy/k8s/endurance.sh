#!/usr/bin/env bash
# 2-hour endurance + chaos orchestrator for the Rust SIP SUT on kind.
#
# Drives the realistic steady-state profile the chaos suite is meant to run
# against, and injects one chaos event every CHAOS_INTERVAL, cycling through
# ALL chaos elements:
#
#   baseline (always on):
#     long    calls uac-long-options.xml          @ LONG_CPS  (OPTIONS-driven
#                                                              hold -> very high conc.)
#     reinvite calls uac-reinvite.xml             @ REINVITE_CPS (=LONG_CPS; INVITE
#                                                  + two in-dialog re-INVITEs at
#                                                  +60s/+120s -> proves the in-dialog
#                                                  re-INVITE transaction survives a
#                                                  reboot, incl. the split where one
#                                                  re-INVITE lands on the backup and
#                                                  the next on the reclaimed nominal)
#     short   calls uac-endurance-short.xml       @ SHORT_CPS (30s hold)
#     abuse         uac-abuse-options-flood       @ ABUSE_CAPS (default 1cps)
#     limiter calls uac-endurance-limiter-cap20.xml @ LIMITER_CPS (30s hold, sends
#                   X-Api-Call cap=20 -> the limiter pins admitted conc. at ~20;
#                   the over-cap calls get 486, scored apart like abuse). Every
#                   worker also carries an always-on global-stress:999999 entry, so
#                   ALL streams traverse the limiter's admit/release/refresh chain.
#   chaos cycle (every CHAOS_INTERVAL):
#     orphan_kill -> kill_worker -> kill_proxy -> peak -> limiter_kill ->
#     limiter_netcut -> (repeat). The two limiter events assert the cap
#     RECONVERGES to ~20 within LIMITER_GRACE (10 min) after the fault.
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
#   DURATION=7200 CHAOS_INTERVAL=900 LONG_CPS=5 REINVITE_CPS=5 SHORT_CPS=100 ABUSE_CAPS=1
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
REINVITE_CPS="${REINVITE_CPS:-$LONG_CPS}"  # in-dialog re-INVITE stream, same volume as long
SHORT_CPS="${SHORT_CPS:-100}"
ABUSE_CAPS="${ABUSE_CAPS:-1}"
PEAK_CAPS="${PEAK_CAPS:-200}"
PEAK_SECS="${PEAK_SECS:-30}"
ORPHAN_CAPS="${ORPHAN_CAPS:-50}"
ORPHAN_BUILD_SECS="${ORPHAN_BUILD_SECS:-20}"
ORPHAN_REAP_WAIT="${ORPHAN_REAP_WAIT:-420}"  # must clear the FULL orphan-reap chain, not just the
                                             # 300s keepalive interval. The keepalive is armed AT
                                             # ANSWER (so a killed dialog has up to 300s until its
                                             # next fire) -> in-dialog OPTIONS to the dead A-leg ->
                                             # KeepaliveTimeout +5s -> BYE to dead peer ->
                                             # TerminatingTimeout safety reaper +32s -> RemoveCall.
                                             # Zero-load floor = 300+5+32 = 337s; +queue latency at
                                             # ~9.5k concurrent pushes the active_calls drop to ~420s
                                             # (330s tripped a FALSE leak at ~100cps in cycle 0).
PASS_THRESHOLD="${PASS_THRESHOLD:-90}"
# --- call-limiter exercise (continuous stream + limiter chaos events) ---
LIMITER_CPS="${LIMITER_CPS:-2}"        # continuous limiter stream rate; 2cps x 30s
                                       # hold ≈ 60 offered vs cap 20 → ~40 rejected
LIMITER_CAP="${LIMITER_CAP:-20}"       # cap stamped into the -key xapi JSON (40-job)
LIMITER_TARGET="${LIMITER_TARGET:-$LIMITER_CAP}" # the cap the stream pins conc. at
# Front-proxy HA VIP (ADR-0012 D7): UAC streams target the VIP, not the Service.
PROXY_VIP="${PROXY_VIP:-172.20.255.250}"
PROXY_TARGET="${PROXY_TARGET:-$PROXY_VIP}"
export PROXY_VIP PROXY_TARGET LIMITER_CAP
LIMITER_TOL="${LIMITER_TOL:-10}"       # allowed band around the cap (±)
LIMITER_GRACE="${LIMITER_GRACE:-600}"  # post-event divergence window (10 min): after
                                       # a limiter fault the cap may drift this long
                                       # (fail-open admits + ~window refill) before it
                                       # must have reconverged to ≈ LIMITER_TARGET
NETCUT_SECS="${NETCUT_SECS:-60}"       # limiter_netcut packet-loss duration
CLUSTER_SETTLE="${CLUSTER_SETTLE:-30}"  # seconds to hold UAC streams back AFTER the
                         # cluster reports Ready, so the front proxy's EndpointSlice
                         # informer has populated its worker set (workers=[] at boot,
                         # filled async) before traffic starts. Without it the first
                         # INVITEs at full rate are rejected mid-discovery -> the
                         # startup failed-call spike (ADR-0012 D4 informer warm-up).
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
  # `-l` headroom (MAX_CONCURRENT): sized so the offered rate never throttles on
  # transient stuck-call backlog. With the SIPp dead-call reaper (-recv_timeout
  # 600s, manifests/40) the backlog is bounded to ~10 min of leaked calls; 1200s
  # of full-rate headroom keeps `-l` comfortably above steady concurrency + that
  # backlog so lost calls do NOT decrease the open rate.
  export UAC_JOB_NAME="$job" SCENARIO="$scenario" CAPS="$cps" ROLE="$role" \
         MAX_CALLS=$(( cps * (DURATION + 600) )) \
         MAX_CONCURRENT="${MAX_CONCURRENT:-$(( cps * 1200 ))}"
  kubectl -n "$NS" delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
  envsubst < manifests/40-sipp-uac-job.yaml | kubectl apply -f - >/dev/null
}

# Re-create any baseline stream whose job has vanished or failed (keeps the
# steady state alive across the whole window even if a job hits MAX_CALLS).
ensure_baseline() {
  local s
  for s in "sipp-uac-long uac-long-options.xml $LONG_CPS long" \
           "sipp-uac-reinvite uac-reinvite.xml $REINVITE_CPS reinvite" \
           "sipp-uac-short uac-endurance-short.xml $SHORT_CPS short" \
           "sipp-uac-limiter uac-endurance-limiter-cap20.xml $LIMITER_CPS limiter"; do
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
# Role-scoped variants. A reboot must be judged on LONG-hold survival SEPARATELY
# from short-call success: long dialogs (OPTIONS-keepalive holds) span the reboot,
# so their loss is the true HA signal, whereas short calls (30s hold) churn so
# fast at 100cps that their success% drowns long-call loss out of any blended
# figure (a reboot can lose ~100% of a worker's long dialogs yet still score
# ~95% blended). See reboot_event.
snap_failed_role()  { vmq "sum(sipp_failed_calls_total{role=\"$1\"})"; }
snap_success_role() { vmq "sum(sipp_successful_calls_total{role=\"$1\"})"; }
snap_created_role() { vmq "sum(sipp_calls_created_total{role=\"$1\"})"; }
# Role-scoped live concurrency (held dialogs right now). Used to size the AT-RISK
# long population at kill time: when the long stream sits at its `-l` ceiling,
# created_delta over the reboot window collapses to ~0, so created can't be the
# loss denominator — the dialogs HELD across the reboot are what's at risk.
snap_conc_role() { vmq "sum(sipp_current_calls{role=\"$1\"})"; }
# B2BUA-side live dialog count + the GHOST GAP (calls the B2BUA still holds that
# SIPp has already abandoned). The gap cancels baseline level/ramp, so it is the
# robust leak signal: a healthy system keeps b2bua_active ≈ sipp_current (gap ~0
# + scrape skew); a leak makes the gap rise and stay risen.
snap_active() { vmq 'sum(b2bua_active_calls)'; }
snap_ghost()  { vmq 'sum(b2bua_active_calls) - sum(sipp_current_calls)'; }
# Call-limiter exercise: the dedicated stream's ADMITTED-and-held concurrency,
# read SIPp-side (rejected 486s never enter hold). A healthy limiter pins this at
# LIMITER_TARGET. The limiter's own `limiter_current_total` gauge is NOT usable
# here: every call now carries the global-stress entry, so that gauge aggregates
# all streams (thousands).
snap_limiter_conc() { vmq 'sum(sipp_current_calls{role="limiter"})'; }

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
  # A single instant at the reap edge aliases scrape skew at ~100cps churn (and
  # can catch the last few orphans still draining). Take the FLOOR of a short
  # window — the true post-reap residual, robust to a one-scrape blip.
  g1="$(snap_ghost)"
  for _ in 1 2 3 4 5; do
    sleep 12; s="$(snap_ghost)"
    g1=$(python3 -c "print(min(float('$g1'), float('$s')))")
  done
  a1="$(snap_active)"
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

# limiter_event measurement: the continuous limiter stream pins ADMITTED
# concurrency at LIMITER_TARGET (the cap). Inject a limiter fault (pod kill, or a
# netem black-hole that leaves the pod up), allow the 10-min divergence window
# (the b2bua fails open so the cap is briefly unenforced + the limiter refills
# over ~window), then assert the stream's admitted concurrency has RECONVERGED to
# within ±LIMITER_TOL of the cap. Pass iff reconverged — this is the requested
# "stays ≈ 20, allowed to differ for up to 10 min after critical changes" bar.
limiter_event() {
  local type="$1" idx="$2" t0 c0 cmid c1 result
  t0="$(date +%s)"
  c0="$(snap_limiter_conc)"
  log "CHAOS #$idx: $type (limiter-stream concurrent before=$c0, target=$LIMITER_TARGET)"
  push_metric "sip_chaos_run{type=\"$type\",phase=\"inject\"} 1"
  open_window "$type"
  case "$type" in
    limiter_kill)   ./chaos.sh limiterkill >>"$RUNLOG" 2>&1 || true ;;
    limiter_netcut) NETCUT_SECS="$NETCUT_SECS" ./chaos.sh limiternetcut >>"$RUNLOG" 2>&1 || true ;;
  esac
  cmid="$(snap_limiter_conc)"
  log "$type: limiter-stream concurrent at/after fault=$cmid; waiting ${LIMITER_GRACE}s for reconvergence"
  sleep "$LIMITER_GRACE"
  ensure_baseline
  c1="$(snap_limiter_conc)"
  close_window "$type"
  result=$(python3 -c "print('pass' if abs(float('$c1')-$LIMITER_TARGET) <= $LIMITER_TOL else 'fail')")
  push_metric "sip_chaos_event{type=\"$type\",result=\"$result\"} 1
sip_chaos_limiter_conc{type=\"$type\"} $c1"
  printf '{"ts":%s,"event":%d,"type":"%s","limiter_before":%s,"limiter_mid":%s,"limiter_after":%s,"target":%s,"tol":%s,"result":"%s"}\n' \
    "$t0" "$idx" "$type" "${c0%.*}" "${cmid%.*}" "${c1%.*}" "$LIMITER_TARGET" "$LIMITER_TOL" "$result" >> "$EVENTS"
  if [ "$result" = "pass" ]; then
    ok "CHAOS #$idx $type: limiter reconverged — concurrent $c0 ->(fault $cmid)-> $c1 (target ${LIMITER_TARGET}±${LIMITER_TOL})"
  else
    warn "CHAOS #$idx $type: limiter did NOT reconverge — concurrent $c1 outside ${LIMITER_TARGET}±${LIMITER_TOL} after ${LIMITER_GRACE}s — FAILURE"
  fi
}

# reboot_event = the ADR-0014 verification. A b2bua worker REBOOT (kill → the
# StatefulSet recreates the same pod → it reclaims its primary partition + the
# backup self-releases its takeover copy on the served txn's terminal state) must
# satisfy THREE invariants, judged SEPARATELY (long ≠ short), or it FAILS:
#
#   (1) SHORT SURVIVAL — short calls (30s hold, 100cps) keep resolving across the
#       window: short success% must clear the bar. Short calls rarely span the
#       reboot, so this mostly proves new-call admission + fast in-dialog flow.
#   (2) LONG SURVIVAL  — the one that matters for HA: long OPTIONS-keepalive holds
#       SPAN the reboot, so their end-of-hold in-dialog BYE lands AFTER the
#       takeover/reclaim. long loss% = long failed / long created in-window must
#       stay under LONG_LOSS_TOL. This is gated APART from short because at 100cps
#       short success drowns out long loss in any blend (a reboot can lose ~100%
#       of a worker's long dialogs and still score ~95% blended — the blind spot
#       that masked the repl-takeover-longcall-loss regression). The observed
#       failure: the BYE gets `481 Call/Transaction Does Not Exist` (dialog gone on
#       the B2BUA after the reboot) → SIPp unexpected_msg.
#   (3) NO LEAK — the GHOST GAP (b2bua_active − sipp_current) must return near
#       baseline after reclaim settles. A gap that stays RISEN is the double-serve
#       leak ADR-0014 killed (backup kept a live takeover copy the reclaimed primary
#       also serves, or eager-takeover re-stormed). Same signal orphan_kill uses.
#
# Modes alternate graceful (rolling-restart drain: flush changelog to backup) and
# crash (hard kill: cold reactive takeover + reclaim) so both reclaim paths run.
REBOOT_GHOST_TOL="${REBOOT_GHOST_TOL:-300}"   # allowed ghost-gap rise (scrape skew
                                              # + in-flight churn during reclaim at
                                              # ~100cps short + ~1750 long-hold conc.)
REBOOT_SETTLE="${REBOOT_SETTLE:-180}"         # post-Ready reclaim/self-release window
LONG_LOSS_TOL="${LONG_LOSS_TOL:-5}"           # max % of long calls created in-window
                                              # allowed to fail (transparent failover
                                              # ⇒ ~0; the regression loses ~100% of a
                                              # rebooted worker's long share)
reboot_event() {
  local idx="$1" t0 s0 f0 g0 a0 mode tgt s s1 f1 g1 a1 ds df pct rise res_pct res_leak result conc
  local sf0 ss0 lf0 lc0 sf1 ss1 lf1 lc1 sds sdf spct ldf ldc ldenom lconc0 lloss res_short res_long
  t0="$(date +%s)"
  s0="$(snap_success)"; f0="$(snap_failed)"; g0="$(snap_ghost)"; a0="$(snap_active)"
  ss0="$(snap_success_role short)"; sf0="$(snap_failed_role short)"
  lf0="$(snap_failed_role long)";   lc0="$(snap_created_role long)"
  lconc0="$(snap_conc_role long)"   # AT-RISK held long dialogs at the kill instant
  if [ "${REBOOT_MODE:-alternate}" = "alternate" ]; then
    [ $(( idx % 2 )) -eq 0 ] && mode=graceful || mode=crash
  else
    mode="$REBOOT_MODE"
  fi
  [ "$mode" = "crash" ] && local grace=0 || local grace=10
  tgt="b2bua-worker-$(( idx % WORKER_REPLICAS ))"
  log "CHAOS #$idx: kill_worker REBOOT $tgt mode=$mode (success=$s0 failed=$f0 ghost=$g0 active=$a0)"
  push_metric "sip_chaos_run{type=\"kill_worker\",phase=\"inject\"} 1"
  open_window kill_worker
  KILL_GRACE="$grace" KILL_TARGET="$tgt" WORKER_REPLICAS="$WORKER_REPLICAS" \
    ./chaos.sh kill >>"$RUNLOG" 2>&1 || true
  # The StatefulSet recreates the pod (same name, NEW IP); the proxy rediscovers it
  # via the EndpointSlice informer (ADR-0012 D4) — no proxy redeploy. The /ready
  # probe only flips once the fresh worker has reclaimed its primary partition, so
  # "Ready again" is the reclaim gate.
  kubectl -n "$NS" wait --for=condition=ready "pod/$tgt" --timeout=180s >>"$RUNLOG" 2>&1 || true
  log "kill_worker: $tgt rebooted (Ready); settling ${REBOOT_SETTLE}s for reclaim + backup self-release"
  sleep "$REBOOT_SETTLE"
  ensure_baseline
  # Ghost-gap FLOOR over a short window — robust to a one-scrape blip at ~100cps
  # churn (a single instant aliases scrape skew, like orphan_event).
  g1="$(snap_ghost)"
  for _ in 1 2 3 4 5; do sleep 12; s="$(snap_ghost)"; g1=$(python3 -c "print(min(float('$g1'),float('$s')))"); done
  s1="$(snap_success)"; f1="$(snap_failed)"; a1="$(snap_active)"; conc="$(snap_conc)"
  ss1="$(snap_success_role short)"; sf1="$(snap_failed_role short)"
  lf1="$(snap_failed_role long)";   lc1="$(snap_created_role long)"
  close_window kill_worker
  ds=$(python3 -c "print(max(0,int(float('$s1'))-int(float('$s0'))))")
  df=$(python3 -c "print(max(0,int(float('$f1'))-int(float('$f0'))))")
  pct=$(python3 -c "t=$ds+$df; print(round(100.0*$ds/t,1) if t else 100.0)")
  rise=$(python3 -c "print(round(float('$g1')-float('$g0'),0))")
  # SHORT survival% (counters can RESET if ensure_baseline relaunched the stream
  # mid-window → a negative delta; clamp to 0 so a relaunch reads as n/a, not a
  # bogus pass). LONG loss% = long-failed-delta / long-created-delta in-window.
  sds=$(python3 -c "print(max(0,int(float('$ss1'))-int(float('$ss0'))))")
  sdf=$(python3 -c "print(max(0,int(float('$sf1'))-int(float('$sf0'))))")
  spct=$(python3 -c "t=$sds+$sdf; print(round(100.0*$sds/t,1) if t else 100.0)")
  ldf=$(python3 -c "print(max(0,int(float('$lf1'))-int(float('$lf0'))))")
  ldc=$(python3 -c "print(max(0,int(float('$lc1'))-int(float('$lc0'))))")
  # AT-RISK denominator: long dialogs HELD at the kill instant ($lconc0). When the
  # long stream is at its `-l` ceiling (the steady state for 19-min holds at 5cps,
  # MAX_CONCURRENT=6000), in-window created ($ldc) collapses to ~0 and would make
  # loss% degenerate to ~100% even with near-perfect failover. Use max(held-at-kill,
  # created-delta) so a non-saturated high-churn window still counts newly-created
  # calls; floor at LONG_LOSS_DENOM_FLOOR to avoid div-by-tiny. A genuine mass
  # teardown still fails (long_failed climbs into the hundreds vs the ~6000 base).
  ldenom=$(python3 -c "print(max(int(float('$lconc0')), $ldc, ${LONG_LOSS_DENOM_FLOOR:-100}))")
  lloss=$(python3 -c "print(round(100.0*$ldf/$ldenom,1) if $ldenom else 0.0)")
  local thr="${KILL_WORKER_THRESHOLD:-$PASS_THRESHOLD}"
  res_short=$(python3 -c "print('pass' if float('$spct')>=$thr else 'fail')")
  res_long=$(python3 -c "print('pass' if float('$lloss')<=$LONG_LOSS_TOL else 'fail')")
  res_leak=$(python3 -c "print('pass' if float('$rise')<=$REBOOT_GHOST_TOL else 'fail')")
  res_pct="$res_short"   # kept for the legacy sip_chaos_success_pct series
  # Overall pass requires ALL THREE: short survival, long survival, no leak.
  if [ "$(( ds + df ))" -eq 0 ]; then
    result="n/a"                       # baseline streams were down — not a real pass
  elif [ "$res_short" = pass ] && [ "$res_long" = pass ] && [ "$res_leak" = pass ]; then
    result="pass"
  else
    result="fail"
  fi
  push_metric "sip_chaos_event{type=\"kill_worker\",result=\"$result\"} 1
sip_chaos_success_pct{type=\"kill_worker\"} $pct
sip_chaos_short_survival_pct{type=\"kill_worker\"} $spct
sip_chaos_long_loss_pct{type=\"kill_worker\"} $lloss
sip_chaos_long_failed{type=\"kill_worker\"} $ldf
sip_chaos_long_created{type=\"kill_worker\"} $ldc
sip_chaos_long_at_risk{type=\"kill_worker\"} $ldenom
sip_chaos_resolved{type=\"kill_worker\",outcome=\"success\"} $ds
sip_chaos_resolved{type=\"kill_worker\",outcome=\"failed\"} $df
sip_chaos_ghost_rise{type=\"kill_worker\"} $rise
sip_chaos_ghost_gap{type=\"kill_worker\"} $g1"
  printf '{"ts":%s,"event":%d,"type":"kill_worker","mode":"%s","short_survival_pct":%s,"short_ok":%d,"short_fail":%d,"long_loss_pct":%s,"long_failed":%d,"long_created":%d,"long_at_risk":%d,"blended_success_pct":%s,"ghost_before":%s,"ghost_after":%s,"ghost_rise":%s,"active_after":%s,"concurrent":%s,"short_result":"%s","long_result":"%s","leak":"%s","result":"%s"}\n' \
    "$t0" "$idx" "$mode" "$spct" "$sds" "$sdf" "$lloss" "$ldf" "$ldc" "$ldenom" "$pct" "${g0%.*}" "${g1%.*}" "${rise%.*}" "${a1%.*}" "${conc%.*}" "$res_short" "$res_long" "$res_leak" "$result" >> "$EVENTS"
  case "$result" in
    pass) ok   "CHAOS #$idx kill_worker($mode): short ${spct}% + long-loss ${lloss}%(tol ${LONG_LOSS_TOL}) + NO leak (ghost rise ${rise}) — PASS" ;;
    n/a)  warn "CHAOS #$idx kill_worker($mode): no baseline calls resolved (streams down?) — n/a" ;;
    *)    warn "CHAOS #$idx kill_worker($mode): FAILURE — short ${spct}%/${thr}% ($res_short); LONG-LOSS ${lloss}% > ${LONG_LOSS_TOL}% ($res_long, ${ldf}/${ldc} long calls); leak ghost rise ${rise} ($res_leak)" ;;
  esac
}

# Record one chaos event: snapshot baseline outcomes, run it, settle, snapshot
# again, compute the success% of calls that resolved across the window, push a
# metric + append a JSONL row. Flags result=fail if below PASS_THRESHOLD.
chaos_event() {
  local type="$1" idx="$2"
  # orphan_kill has a bespoke measurement (B2BUA active_calls reaping), not the
  # baseline success-rate path the other events share.
  if [ "$type" = "orphan_kill" ]; then orphan_event "$idx"; return; fi
  # limiter_kill / limiter_netcut assert limiter-cap reconvergence, not baseline
  # success% (the limiter stream's expected 486s are excluded from that anyway).
  if [ "$type" = "limiter_kill" ] || [ "$type" = "limiter_netcut" ]; then
    limiter_event "$type" "$idx"; return
  fi
  # kill_worker = a b2bua REBOOT: asserts the ADR-0014 invariants (survival +
  # no double-serve leak), not just baseline success% — see reboot_event.
  if [ "$type" = "kill_worker" ]; then reboot_event "$idx"; return; fi
  local t0 s0 f0 s1 f1 ds df pct result conc
  t0="$(date +%s)"
  s0="$(snap_success)"; f0="$(snap_failed)"
  log "CHAOS #$idx: $type (baseline success=$s0 failed=$f0)"
  push_metric "sip_chaos_run{type=\"$type\",phase=\"inject\"} 1"
  open_window "$type"

  local tgt
  case "$type" in
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
  # Per-type pass bar. kill_proxy now meets the normal bar: the proxy is HA behind
  # a keepalived VRRP VIP (ADR-0012 D7), so killing the master fails over to the
  # warm backup in <2s with the VIP (and thus Record-Route) stable — new + in-
  # dialog calls keep flowing. No lenient threshold anymore.
  local thr="$PASS_THRESHOLD"
  case "$type" in
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
  local SUT_IMAGE="${SUT_IMAGE:-siprustserver:dev}"
  local KEEPALIVED_IMAGE="${KEEPALIVED_IMAGE:-siprustserver-keepalived:dev}"
  local RABBITMQ_IMAGE="${RABBITMQ_IMAGE:-rabbitmq:3.13-management}"
  # Rebuild the SUT (b2bua worker + front-proxy) image from CURRENT source. The
  # previous wireup rebuilt ONLY sipp:dev, so on an existing cluster the b2bua
  # binary was never refreshed — a 14h-old worker pod kept running and the
  # uncommitted ADR-0014 repl changes were silently NOT under test (the whole
  # point of the run). Build the SAME three images `run.sh up` builds so wireup
  # deploys exactly what a fresh cluster start would. (SKIP_BUILD=1 to reuse.)
  if [ "${SKIP_BUILD:-0}" != "1" ]; then
    log "wireup: building SUT image $SUT_IMAGE (current source) + keepalived + sipp:dev"
    docker build -f "$REPO_ROOT/deploy/docker/Dockerfile" -t "$SUT_IMAGE" "$REPO_ROOT" >>"$RUNLOG" 2>&1
    docker build -f "$REPO_ROOT/deploy/docker/Dockerfile.keepalived" -t "$KEEPALIVED_IMAGE" "$REPO_ROOT" >>"$RUNLOG" 2>&1
    docker build -t sipp:dev "$SIPP_DIR" >>"$RUNLOG" 2>&1
    log "wireup: pulling RabbitMQ image $RABBITMQ_IMAGE (CDR transport)"
    docker image inspect "$RABBITMQ_IMAGE" >/dev/null 2>&1 || docker pull "$RABBITMQ_IMAGE" >>"$RUNLOG" 2>&1
    log "wireup: loading images into kind"
    kind load docker-image "$SUT_IMAGE" --name "$CLUSTER" >>"$RUNLOG" 2>&1
    kind load docker-image "$KEEPALIVED_IMAGE" --name "$CLUSTER" >>"$RUNLOG" 2>&1
    kind load docker-image sipp:dev --name "$CLUSTER" >>"$RUNLOG" 2>&1
    kind load docker-image "$RABBITMQ_IMAGE" --name "$CLUSTER" >>"$RUNLOG" 2>&1
  fi
  log "wireup: deploy stack (repl on, ${WORKER_REPLICAS} workers) — same path as cluster start"
  REPL_ENABLE=1 WORKER_REPLICAS="$WORKER_REPLICAS" OBS_ENABLE="${OBS_ENABLE:-1}" ./run.sh deploy >>"$RUNLOG" 2>&1
  # imagePullPolicy=IfNotPresent + an unchanged image tag means `kubectl apply`
  # will NOT restart pods onto the freshly-loaded image. Force a rollout so the
  # new binary actually runs, then wait Ready before driving any traffic — else
  # we'd repeat the stale-binary trap above.
  if [ "${SKIP_BUILD:-0}" != "1" ]; then
    log "wireup: rolling workers/proxy/uas onto the freshly-built image"
    kubectl -n "$NS" rollout restart statefulset/b2bua-worker deploy/sip-front-proxy deploy/sipp-uas deploy/cdr-consumer >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status statefulset/b2bua-worker --timeout=300s >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status deploy/sip-front-proxy --timeout=180s >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status deploy/sipp-uas --timeout=180s >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status deploy/cdr-consumer --timeout=180s >>"$RUNLOG" 2>&1 || true
  fi
  log "wireup: (re)load observability dashboards/scrape"
  [ -x "$OBS_DIR/install.sh" ] && "$OBS_DIR/install.sh" --apply >>"$RUNLOG" 2>&1 || true
  ok "wireup complete"
}

start_baseline() {
  log "starting baseline streams: long@${LONG_CPS} reinvite@${REINVITE_CPS} short@${SHORT_CPS} abuse@${ABUSE_CAPS} limiter@${LIMITER_CPS}(cap ${LIMITER_TARGET})"
  launch_stream sipp-uac-long     uac-long-options.xml          "$LONG_CPS"     long
  launch_stream sipp-uac-reinvite uac-reinvite.xml              "$REINVITE_CPS" reinvite
  launch_stream sipp-uac-short    uac-endurance-short.xml       "$SHORT_CPS"    short
  launch_stream sipp-uac-limiter uac-endurance-limiter-cap20.xml "$LIMITER_CPS" limiter
  ABUSE_CAPS="$ABUSE_CAPS" ./chaos.sh abuse up >>"$RUNLOG" 2>&1 || true
  push_metric "sip_endurance_run{phase=\"start\"} 1"
}

stop_streams() {
  log "stopping baseline + abuse streams"
  for j in sipp-uac-long sipp-uac-reinvite sipp-uac-short sipp-uac-limiter sipp-uac-peak sipp-uac-orphan; do
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
  # Hold the UAC streams back so the proxy's worker-discovery informer is warm
  # before traffic starts (avoids the startup failed-call spike).
  log "cluster-settle ${CLUSTER_SETTLE}s (let the proxy discover workers) before starting UAC streams"
  sleep "$CLUSTER_SETTLE"
  start_baseline

  local cycle
  if [ "${REBOOT_FOCUS:-0}" = "1" ]; then
    # Reboot-focused run: every chaos event is a b2bua worker reboot (modes
    # alternate graceful/crash inside reboot_event), to hammer the ADR-0014
    # takeover/reclaim/self-release path.
    cycle=(kill_worker)
    log "REBOOT_FOCUS=1 — chaos cycle is b2bua worker reboot only (graceful/crash alternating)"
  else
    cycle=(orphan_kill kill_worker kill_proxy peak limiter_kill limiter_netcut)
  fi
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
