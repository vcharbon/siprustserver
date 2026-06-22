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
#     short   calls SPLIT 50/50 into emergency + non-emergency halves (30s hold):
#               short_em uac-endurance-short.xml         @ SHORT_EM_CPS
#                        (Resource-Priority esnet.0 ⇒ never shed by overload)
#               short_ne uac-endurance-short-noemerg.xml @ SHORT_NE_CPS
#                        (no RP ⇒ sheddable; tolerates the overload 503)
#     abuse         uac-abuse-options-flood       @ ABUSE_CAPS (default 1cps)
#     limiter calls uac-endurance-limiter-cap20.xml @ LIMITER_CPS (30s hold, sends
#                   X-Api-Call cap=20 -> the limiter pins admitted conc. at ~20;
#                   the over-cap calls get 486, scored apart like abuse). Every
#                   worker also carries an always-on global-stress:999999 entry, so
#                   ALL streams traverse the limiter's admit/release/refresh chain.
#   chaos cycle (every CHAOS_INTERVAL):
#     orphan_kill -> kill_worker -> kill_proxy -> peak -> cpu_starve ->
#     limiter_kill -> limiter_netcut -> (repeat). The two limiter events assert
#     the cap RECONVERGES to ~20 within LIMITER_GRACE (10 min) after the fault.
#     cpu_starve is the OVERLOAD event: it shrinks ONE worker's CPU quota (via
#     the kind node's cgroup, no restart) so its ELU crosses the panic threshold
#     and the Tier-3 admission gate sheds NEW non-emergency INVITEs (503) while
#     leaving emergency (Resource-Priority esnet.0) and all in-dialog traffic
#     untouched. Unlike `peak` (which only overloaded the SIPp generators, never
#     the platform), this actually engages the SUT's overload protection.
#     Launch it standalone for tuning: `./chaos.sh cpustarve` (or `overload` to
#     run it with a concurrent mini peak); knobs STARVE_TARGET/STARVE_SECS/
#     STARVE_QUOTA_US/STARVE_PERIOD_US.
#   involuntary (unscheduled, NOT the SUT):
#     uas_crash -> a background watcher flags every SIPp-UAS exit-255 timer-wheel
#     abort (a load-generator fault that wipes B-leg state) as its own red event
#     and TAINTS any deliberate chaos window it overlaps, so a UAS crash is never
#     mis-investigated as a SUT regression. See snap_uas_restarts / uas_crash_watcher.
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
#   SHORT_EM_CPS=SHORT_CPS/2 SHORT_NE_CPS=SHORT_CPS-SHORT_EM_CPS (emergency/non-emergency split)
#   PEAK_CAPS=200 PEAK_SECS=30 WORKER_REPLICAS=2 PASS_THRESHOLD=90
#   cpu_starve: STARVE_TARGET=b2bua-worker-0 STARVE_SECS=90 STARVE_QUOTA_US=30000
#               STARVE_PERIOD_US=100000 STARVE_PEAK_CAPS=120 EMERG_LOSS_TOL=1
#               (calibrated 2026-06-20 — 0.30 core + 120cps non-emergency peak)
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
# The long stream's steady-state concurrency (≈ hold × LONG_CPS ≈ 5700 at 5cps)
# sits FAR above a single SIPp process's ~2900-dialog timer-wheel ceiling (the same
# ceiling that aborts the UAS with exit 255 — see manifests/10-sipp-uas.yaml), so a
# single long pod self-aborts mid-ramp and the long-HA signal sawtooths. Shard it
# across LONG_SHARDS pods at LONG_CPS/LONG_SHARDS each (all role=long, so the
# exporters aggregate by sum()), keeping every shard well under the ceiling — the
# UAC analogue of the UAS 1→5 scale-out. 5 shards × 1cps ≈ 1140 conc/pod.
LONG_SHARDS="${LONG_SHARDS:-5}"
LONG_SHARD_CPS=$(( LONG_CPS / LONG_SHARDS )); [ "$LONG_SHARD_CPS" -lt 1 ] && LONG_SHARD_CPS=1
# Per-shard `-l` (= the long steady-state, since OPTIONS-hold calls grow to `-l`).
# CAPPED at 800/shard → 4000 total long concurrency. The 5 UAS pods hold every
# B-leg (long + short + reinvite); at the old 1800/shard (9000 long) the UAS sat
# at ~2480 dialogs/pod — right against its ~2900 SIPp timer-wheel crash ceiling —
# so a peak burst or normal churn tipped a pod over → UAS crash → ~1800 long
# B-legs lose keepalive → b2bua tears those long calls down → long churns and its
# refills fail (the 9000-conc symptom). At 800/shard the UAS steady load is
# ~1520/pod with real margin: no UAS crash, no long churn, a clean long-HA signal,
# and still ~4000 at-risk long dialogs (a strong reboot signal). NOTE: the single
# pre-shard long pod *targeted* 9000 too but crashed at ~2900 before reaching it,
# so the UAS was never actually loaded to 9000 — sharding exposed the oversubscription.
LONG_SHARD_MAXCONC="${LONG_SHARD_MAXCONC:-800}"
REINVITE_CPS="${REINVITE_CPS:-$LONG_CPS}"  # in-dialog re-INVITE stream, same volume as long
SHORT_CPS="${SHORT_CPS:-100}"
# The short stream is split HALF emergency / HALF non-emergency so the cpu_starve
# overload experiment can prove the SUT sheds non-emergency NEW calls while never
# touching emergency ones (and never touching ESTABLISHED in-dialog calls). The
# two halves run as SEPARATE SIPp streams with DISTINCT roles so their outcomes
# are measured apart:
#   short_em  uac-endurance-short.xml          (Resource-Priority: esnet.0 ⇒
#             emergency ⇒ overload.should_admit ALWAYS admits ⇒ never shed)
#   short_ne  uac-endurance-short-noemerg.xml  (no Resource-Priority ⇒ sheddable;
#             tolerates the overload 503/480/486 as a clean terminate, so a shed
#             call is NOT a SIPp failure — the shed is read SUT-side on
#             b2bua_overload_rejected_total instead)
# Default each half to SHORT_CPS/2 so the aggregate offered short rate is unchanged.
SHORT_EM_CPS="${SHORT_EM_CPS:-$(( SHORT_CPS / 2 ))}"; [ "$SHORT_EM_CPS" -lt 1 ] && SHORT_EM_CPS=1
SHORT_NE_CPS="${SHORT_NE_CPS:-$(( SHORT_CPS - SHORT_EM_CPS ))}"; [ "$SHORT_NE_CPS" -lt 1 ] && SHORT_NE_CPS=1
ABUSE_CAPS="${ABUSE_CAPS:-1}"
PEAK_CAPS="${PEAK_CAPS:-200}"
PEAK_SECS="${PEAK_SECS:-30}"
# cpu_starve (OVERLOAD) event — CPU scarcity on ONE worker, NOT a traffic peak.
# Defaulted here too (set -u) since cpu_starve_event references them bare; the
# values are passed through to chaos.sh. CALIBRATED 2026-06-20 — see chaos.sh.
# The event runs the `overload` COMBO (starve + a small non-emergency peak): at
# the calibrated 0.30-core cap the worker's BASELINE demand (~0.22) sits UNDER the
# cap, so starve-alone would not engage; the +STARVE_PEAK_CAPS non-emergency peak
# lifts demand to ~0.33 (just over the cap) so the panic-ELU gate sheds the
# non-emergency excess while emergency + in-dialog stay clean.
STARVE_TARGET="${STARVE_TARGET:-b2bua-worker-0}"
STARVE_SECS="${STARVE_SECS:-90}"
STARVE_QUOTA_US="${STARVE_QUOTA_US:-30000}"
STARVE_PERIOD_US="${STARVE_PERIOD_US:-100000}"
STARVE_PEAK_CAPS="${STARVE_PEAK_CAPS:-120}"   # non-emergency peak the overload combo adds
ORPHAN_CAPS="${ORPHAN_CAPS:-50}"
ORPHAN_BUILD_SECS="${ORPHAN_BUILD_SECS:-20}"
ORPHAN_REAP_WAIT="${ORPHAN_REAP_WAIT:-540}"  # must clear the FULL orphan-reap chain, not just the
                                             # 300s keepalive interval. The keepalive is armed AT
                                             # ANSWER (so a killed dialog has up to 300s until its
                                             # next fire) -> in-dialog OPTIONS to the dead A-leg ->
                                             # KeepaliveTimeout (B2BUA_KEEPALIVE_TIMEOUT_SEC, now 120s)
                                             # -> BYE to dead peer -> TerminatingTimeout safety reaper
                                             # +32s -> RemoveCall. Zero-load floor = 300+120+32 = 452s
                                             # (was 337s at the old 45s timeout); +queue latency at
                                             # ~9.5k concurrent pushes the active_calls drop higher, so
                                             # 540s (was 420s) keeps margin. Bumped in lockstep with the
                                             # keepalive-timeout 45->120 fix for the peak keepalive-shed
                                             # cascade — a 420s wait would FALSE-fail orphan_kill now.
PASS_THRESHOLD="${PASS_THRESHOLD:-90}"
# --- call-limiter exercise (continuous stream + limiter chaos events) ---
LIMITER_CPS="${LIMITER_CPS:-2}"        # continuous limiter stream rate; 2cps x 30s
                                       # hold ≈ 60 offered vs cap 20 → ~40 rejected
LIMITER_CAP="${LIMITER_CAP:-20}"       # cap stamped into the -key xapi JSON (40-job)
LIMITER_TARGET="${LIMITER_TARGET:-$LIMITER_CAP}" # the cap the stream pins conc. at
# Front-proxy HA VIP + LB port (ADR-0012 D7): from the shared lib (subnet→VIP
# derivation, all three runners agree). UAC streams target PROXY_VIP, not the Service.
source "$HERE/lib/net-env.sh"
# Proxy split (local=kind+127 bypass, non-local=through proxy). Gives wireup's
# image rebuild the SAME proxy-forwarding `docker_build` run.sh uses, so the
# endurance run can build images on a PROXIFIED host. Sourced AFTER net-env.sh.
source "$HERE/lib/proxy-env.sh"
source "$HERE/lib/kube-env.sh"   # pin every kubectl to context kind-$CLUSTER
export LIMITER_CAP
LIMITER_TOL="${LIMITER_TOL:-3}"        # allowed band around the cap (±). Was ±10:
                                       # the 2026-06-12 zombie pinning (15/20 held
                                       # for ~50 min by stuck-in-setup calls) sat
                                       # INSIDE that band and was invisible. A
                                       # healthy stream pins the cap within ±1-2.
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
  # 600s, manifests/40) the backlog is bounded to ~10 min of leaked calls; the
  # headroom keeps `-l` comfortably above steady concurrency + that backlog so
  # lost calls do NOT decrease the open rate. Sized at 1800× (was 1200×): at 1200×
  # the long stream's steady ~5700 conc. sat right against its 6000 `-l` ceiling,
  # so a one-time teardown (e.g. a chaos peak) wedged the stream at the wall and
  # turned it into a self-sustaining failed/s climb (every over-`-l` attempt = a
  # failed call). 1800× decouples the failure accounting from the ceiling.
  # Compute `-l` FRESH from cps/role — never inherit a leaked MAX_CONCURRENT from a
  # prior launch_stream call (the export below persists it in the env; reading it
  # back as a default made every stream after the first reuse the first's `-l`).
  local maxc=$(( cps * 1800 ))
  # Long shards are CAPPED (not 1800×) so the UAS B-leg load stays under its
  # ~2900/pod crash ceiling — see LONG_SHARD_MAXCONC. Other streams keep the
  # generous 1800× headroom (short@100cps never sits near the UAS ceiling).
  [ "$role" = "long" ] && maxc="${LONG_SHARD_MAXCONC:-800}"
  # Per-role pod resources. The exit-255 crash is a CPU-starvation timer-wheel slip,
  # not OOM/-l, so the lever is guaranteed CPU + a high burst limit per process —
  # NOT more pods (more SIPp processes contend for the 2×24-core load pool and make
  # the slip MORE likely). `short` holds ~3000 conc in one process (at the ceiling)
  # so it gets fattened; the rest stay modest to keep total CPU requests inside the
  # node budget alongside the 5 fattened UAS pods (req 4 each). Override via env.
  # CPU REQUESTS trimmed for THIS cluster shape (2 load nodes, ~10 allocatable CPU
  # each, already running other pods + the 5 UAS pods) — issue1: the old requests
  # (short 4, short_em/ne 3, long 2, default 2) reserved more than the load nodes
  # had spare and piled scheduling pressure on top of the UAS StatefulSet. We trim
  # the RESERVATION only; the burst LIMITS are unchanged, so each CPU-bound SIPp
  # timer wheel can still burst to catch up under load (the slip is prevented by the
  # high limit, not the request floor). Override per-role via *_CPU_REQ env.
  local cpu_req cpu_lim mem_req mem_lim
  case "$role" in
    # Both short halves (short_em/short_ne) hold ~half the old single-stream
    # concurrency (SHORT_CPS/2 × 30s); req 3 -> 2 (limit 12 unchanged).
    short_em|short_ne) cpu_req="${SHORT_CPU_REQ:-2}"; cpu_lim="${SHORT_CPU_LIM:-12}"; mem_req="512Mi"; mem_lim="2Gi" ;;
    short)  cpu_req="${SHORT_CPU_REQ:-2}"; cpu_lim="${SHORT_CPU_LIM:-16}"; mem_req="512Mi"; mem_lim="2Gi" ;;
    long)   cpu_req="${LONG_CPU_REQ:-1}";  cpu_lim="${LONG_CPU_LIM:-12}"; mem_req="384Mi"; mem_lim="1536Mi" ;;
    *)      cpu_req="1"; cpu_lim="8"; mem_req="384Mi"; mem_lim="1536Mi" ;;
  esac
  export UAC_JOB_NAME="$job" SCENARIO="$scenario" CAPS="$cps" ROLE="$role" \
         MAX_CALLS=$(( cps * (DURATION + 600) )) \
         MAX_CONCURRENT="$maxc" \
         UAC_CPU_REQ="$cpu_req" UAC_CPU_LIM="$cpu_lim" \
         UAC_MEM_REQ="$mem_req" UAC_MEM_LIM="$mem_lim"
  kubectl -n "$NS" delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
  envsubst < manifests/40-sipp-uac-job.yaml | kubectl apply -f - >/dev/null
}

# The baseline stream specs ("job scenario cps role"). The long stream is expanded
# into LONG_SHARDS shards (sipp-uac-long-0..N-1), each a separate SIPp pod at
# LONG_SHARD_CPS — all role=long so the exporters aggregate by sum(). This keeps
# each SIPp process under the ~2900-dialog timer-wheel ceiling that otherwise
# aborts a single long pod with exit 255 (see LONG_SHARDS above).
baseline_specs() {
  local i
  for ((i=0; i<LONG_SHARDS; i++)); do
    echo "sipp-uac-long-$i uac-long-options.xml $LONG_SHARD_CPS long"
  done
  echo "sipp-uac-reinvite uac-reinvite.xml $REINVITE_CPS reinvite"
  # Short split into emergency (short_em) + non-emergency (short_ne) halves — see
  # SHORT_EM_CPS/SHORT_NE_CPS. Two roles so the overload gate measures them apart.
  echo "sipp-uac-short-em uac-endurance-short.xml $SHORT_EM_CPS short_em"
  echo "sipp-uac-short-ne uac-endurance-short-noemerg.xml $SHORT_NE_CPS short_ne"
  echo "sipp-uac-limiter uac-endurance-limiter-cap20.xml $LIMITER_CPS limiter"
}

# Re-create any baseline stream whose job has vanished or failed (keeps the
# steady state alive across the whole window even if a job hits MAX_CALLS). A
# vanished baseline job is itself an INVOLUNTARY SIPp crash (exit 255 timer-wheel
# abort on the UAC side, same load-generator fault as the UAS) — flag it as such
# for visibility, but do NOT taint chaos windows: a dead UAC stream loses its own
# calls (long-loss vacuously passes), it does not corrupt the SUT's view the way a
# UAS crash does, so it cannot produce a FALSE SUT failure.
ensure_baseline() {
  local job scenario cps role
  while read -r job scenario cps role; do
    [ -n "$job" ] || continue
    local active
    active="$(kubectl -n "$NS" get job "$job" -o jsonpath='{.status.active}' 2>/dev/null || echo)"
    if [ "${active:-0}" != "1" ]; then
      # Capture WHY it died before relaunching, so the monitoring loop / a
      # subagent investigation has the dead pod's status + tail of its logs.
      local dump="$RUN_DIR/dead-$job-$(date +%s)"
      local exitc
      exitc="$(kubectl -n "$NS" get pods -l "job-name=$job" -o jsonpath='{range .items[*]}{.status.containerStatuses[?(@.name=="sipp-uac")].lastState.terminated.exitCode}{" "}{end}' 2>/dev/null)"
      { kubectl -n "$NS" describe job "$job" 2>&1
        echo "--- last pod logs (sipp) ---"
        kubectl -n "$NS" logs "job/$job" -c sipp-uac --tail=60 2>&1
        echo "--- last pod logs (stat-exporter) ---"
        kubectl -n "$NS" logs "job/$job" -c stat-exporter --tail=20 2>&1
      } > "$dump.txt" 2>&1 || true
      warn "INVOLUNTARY UAC-stream crash (test-infra, NOT SUT): $job not active (exit=${exitc:-?}) — diagnostics in ${dump##*/}.txt — relaunching (no taint: a dead UAC stream cannot false-fail the SUT)"
      push_metric "sip_endurance_stream_restart{stream=\"$role\"} 1
sip_chaos_event{type=\"uac_crash\",result=\"involuntary\"} 1"
      printf '{"ts":%s,"type":"uac_crash_involuntary","job":"%s","role":"%s","exit_code":"%s","note":"SIPp UAC self-abort (exit 255 timer-wheel) — load generator, not the SUT; no taint"}\n' \
        "$(date +%s)" "$job" "$role" "${exitc:-?}" >> "$EVENTS"
      launch_stream "$job" "$scenario" "$cps" "$role"
    fi
  done < <(baseline_specs)
  # Abuse stream via chaos.sh (handles its own metric markers).
  if ! kubectl -n "$NS" get job sipp-uac-abuse >/dev/null 2>&1; then
    ABUSE_CAPS="$ABUSE_CAPS" ./chaos.sh abuse up >>"$RUNLOG" 2>&1 || true
  fi
}

# Sum baseline (long+short) cumulative outcome counters from the exporters.
snap_success() { vmq 'sum(sipp_successful_calls_total{role=~"long|short_em|short_ne"})'; }
snap_failed()  { vmq 'sum(sipp_failed_calls_total{role=~"long|short_em|short_ne"})'; }
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
# Combined SHORT outcomes across BOTH halves (short_em + short_ne) — the reboot
# short-survival gate judges the whole short population, not one priority class.
snap_failed_short()  { vmq 'sum(sipp_failed_calls_total{role=~"short_em|short_ne"})'; }
snap_success_short() { vmq 'sum(sipp_successful_calls_total{role=~"short_em|short_ne"})'; }
# Per-priority-class deltas the cpu_starve overload gate keys on. The shed of a
# non-emergency NEW call is read SUT-side (b2bua_overload_rejected_total), not as
# a short_ne SIPp failure (it tolerates the 503); a short_em SIPp failure or ANY
# emergency reject IS a regression (emergency must never be shed).
snap_overload_rejected() { vmq 'sum(b2bua_overload_rejected_total)'; }
snap_emergency_admitted() { vmq 'sum(b2bua_emergency_admitted_total)'; }
# Panic-ELU rejects ONLY (the CPU-overload signal; excludes bucket_empty) and the
# worst worker's published ELU EWMA — the two dials for tuning the cpu_starve
# throttle. ELU must cross B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD (0.75) for the
# panic-ELU sheds to start; if it does not, the throttle is too loose.
snap_panic_rejected() { vmq 'sum(b2bua_overload_reject_total{reason="panic_elu"})'; }
snap_elu_max()        { vmq 'max(b2bua_overload_elu_ewma)'; }
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

# --- INVOLUNTARY SIPp-UAS crash detection (test-infra fault, NOT the SUT) ---
# The downstream SIPp UAS (sipp-uas-N) periodically self-aborts with exit 255 —
# a SIPp v3.7.7 internal timer-wheel slip ("wheel_base > clock_tick") under high
# concurrency, NOT OOM / fd / -l (see manifests/10-sipp-uas.yaml). When it dies,
# k8s restarts it but the in-memory B-leg dialog state is WIPED, so the b2bua's
# in-dialog keepalive/BYE to that pod go unanswered and it tears down long calls.
# That looks exactly like a SUT HA regression (long-loss spike) but is purely a
# load-generator fault. We surface every UAS restart as an explicit INVOLUNTARY
# chaos event and TAINT any chaos window it overlaps, so the run's failures are
# not mis-attributed to the SUT. KSM relabels its emitted pod identity into
# exported_* (the scrape overwrites namespace/pod with KSM's own), so the UAS
# pods are matched on exported_pod, not pod.
snap_uas_restarts() { vmq 'sum(kube_pod_container_status_restarts_total{exported_namespace="sip-test",exported_pod=~"sipp-uas.*",exported_container="sipp-uas"})'; }

# A UAS crash (exit-255 timer-wheel abort) wipes that pod's B-leg dialog state, so
# ~1/N of established long calls are left A-leg-up / B-leg-dead. Those calls are not
# torn down at crash time — they limp until their next in-dialog OPTIONS keepalive
# (interval KEEPALIVE_SEC) gets no 200 and the KEEPALIVE_TIMEOUT_SEC grace elapses.
# A subsequent worker reboot ACCELERATES that detection: reclaim_all's catchup
# smoothing re-probes the whole partition at once, so the dead-B-leg backlog drains
# as a keepalive-timeout BYE burst inside the reboot window — a LOAD-GENERATOR
# aftermath, not a SUT failure (endurance-20260610 #7: 12.8% long-loss == all 1968
# keepalive-timeout BYEs, evenly across shards, B-legs on the crashed sipp-uas-2).
# The in-window restart-delta below misses it (the crash predates the event), so we
# also taint when a UAS crash occurred within the dead-B-leg drain window
# (keepalive interval + timeout, the longest a dead B-leg can linger undetected).
UAS_CRASH_DRAIN_SEC="${UAS_CRASH_DRAIN_SEC:-$(( ${KEEPALIVE_SEC:-300} + ${KEEPALIVE_TIMEOUT_SEC:-120} ))}"
# Epoch of the most recent involuntary UAS crash, written by the backgrounded
# uas_crash_watcher (a subshell — cannot mutate a parent var) and read here.
UAS_CRASH_TS_FILE="${UAS_CRASH_TS_FILE:-${RUN_DIR:-/tmp}/.last_uas_crash_ts}"
# Epoch of this worker's PREVIOUS reboot, written by reboot_event after it finishes.
# A kill_worker reclaim's catchup re-probes the WHOLE partition at once, so it
# surfaces the entire dead-B-leg backlog accrued since that partition was last
# reclaimed (= the previous reboot) — REGARDLESS of wall-clock age, which is why the
# fixed drain window below is not enough on its own (endurance-20260610 #7: the UAS
# crash predated the reboot by 2264s ≫ drain, yet the reboot still surfaced it).
LAST_REBOOT_TS_FILE="${LAST_REBOOT_TS_FILE:-${RUN_DIR:-/tmp}/.last_reboot_ts}"

# Downgrade a non-pass result to "tainted" when the UAS crashed during the window
# [$2 = restart count at event start .. now] OR when a UAS crash within the last
# UAS_CRASH_DRAIN_SEC left a dead-B-leg backlog this event's reclaim surfaced.
# Echoes the (possibly downgraded) result; warns loudly so the monitor/subagent
# SKIPS a SUT investigation for it.
taint_if_uas_crash() {  # $1=result  $2=uas_restarts_at_start  $3=event-type
  [ "$1" = "pass" ] && { echo "$1"; return; }
  [ "$1" = "n/a" ]  && { echo "$1"; return; }
  local d; d="$(python3 -c "print(int(float('$(snap_uas_restarts)')-float('$2')))" 2>/dev/null || echo 0)"
  if [ "${d:-0}" -gt 0 ]; then
    warn "  ↳ TAINTED: $d involuntary SIPp-UAS crash(es) (exit 255 timer-wheel abort) during the $3 window — this is a LOAD-GENERATOR fault, NOT a SUT failure. Do not investigate the SUT for it."
    push_metric "sip_chaos_event{type=\"$3\",result=\"tainted\"} 1"
    echo "tainted"; return
  fi
  local last_crash; last_crash="$(cat "$UAS_CRASH_TS_FILE" 2>/dev/null || echo 0)"
  local age=$(( $(date +%s) - last_crash ))
  if [ "${last_crash:-0}" -gt 0 ] && [ "$age" -lt "$UAS_CRASH_DRAIN_SEC" ]; then
    warn "  ↳ TAINTED (aftermath): an involuntary SIPp-UAS crash ${age}s ago (< ${UAS_CRASH_DRAIN_SEC}s drain) left dead B-legs; this $3 reclaim re-probed them as a keepalive-timeout BYE burst — LOAD-GENERATOR aftermath, NOT a SUT failure. Do not investigate the SUT for it."
    push_metric "sip_chaos_event{type=\"$3\",result=\"tainted\"} 1"
    echo "tainted"; return
  fi
  # kill_worker only: the reclaim catchup surfaces the dead-B-leg backlog accrued
  # since this partition's PREVIOUS reclaim (= the last reboot), at any age. Taint
  # if a UAS crash falls in that (prev-reboot, now] interval — the natural-drain
  # window above misses it because the backlog limped (no keepalive re-fired) until
  # this reboot's catchup compressed it into the event window.
  if [ "$3" = "kill_worker" ] && [ "${last_crash:-0}" -gt 0 ]; then
    local last_reboot; last_reboot="$(cat "$LAST_REBOOT_TS_FILE" 2>/dev/null || echo 0)"
    if [ "$last_crash" -gt "${last_reboot:-0}" ]; then
      warn "  ↳ TAINTED (aftermath): an involuntary SIPp-UAS crash occurred since this worker's previous reboot ($(( $(date +%s) - last_crash ))s ago); its reclaim catchup re-probed the dead-B-leg backlog as a keepalive-timeout BYE burst — LOAD-GENERATOR aftermath, NOT a SUT failure. Do not investigate the SUT for it."
      push_metric "sip_chaos_event{type=\"$3\",result=\"tainted\"} 1"
      echo "tainted"; return
    fi
  fi
  echo "$1"
}

# Whole-VM stall detector (WSL2/kind host deschedule). Pods OFF the SIP data path
# (cdr-consumer, call-limiter) cannot be moved by SIP traffic, so if their summed
# container CPU collapses to <25% of its own in-window peak, EVERY container was
# descheduled by the host — not a SIP event. Used to quarantine an aftermath
# long-loss that is really recv_timeout from frozen b2bua event loops (the
# 2026-06-16 endurance idx7 artifact: a ~2.5-min host freeze; cdr/limiter CPU
# 0.04→0.004 in lockstep, workers 0.5→0.08 then RECOVERED, long loss 100%
# timeout_recv), which is distinct from a genuine keepalive-BYE teardown
# (unexpected_msg/481). $1 = lookback window in seconds (the aftermath span).
# Returns 0 (stalled) / 1 (clean). Uses instant subqueries over the recent window
# (no @ modifier) — correct because the aftermath runs while streams are still up.
cluster_stalled_in_window() {  # $1 = window_seconds
  local w="${1:-540}" mn mx cpu='sum(rate(container_cpu_usage_seconds_total{pod=~"cdr-consumer.*|call-limiter.*",container!=""}[1m]))'
  mn="$(vmq "min_over_time(${cpu}[${w}s:30s])")"
  mx="$(vmq "max_over_time(${cpu}[${w}s:30s])")"
  python3 -c "import sys; mn=float('${mn:-0}'); mx=float('${mx:-0}'); sys.exit(0 if mx>0.02 and mn < 0.25*mx else 1)"
}

# Background watcher: poll the UAS restart total every 30s; on any increase, emit
# an INVOLUNTARY chaos event (red annotation window + JSONL row + log) naming the
# pod(s) and exit code, so the timeline shows exactly when a load-generator crash
# perturbed the SUT independently of the deliberate chaos schedule.
UAS_WATCH_PID=""
uas_crash_watcher() {
  local last cur delta pods exitc
  last="$(snap_uas_restarts)"
  while sleep 30; do
    cur="$(snap_uas_restarts)"
    delta="$(python3 -c "print(int(float('$cur')-float('$last')))" 2>/dev/null || echo 0)"
    if [ "${delta:-0}" -gt 0 ]; then
      pods="$(kubectl -n "$NS" get pods -l app=sipp-uas \
        -o jsonpath='{range .items[*]}{.metadata.name}=r{.status.containerStatuses[0].restartCount} {end}' 2>/dev/null)"
      exitc="$(vmq 'max(kube_pod_container_status_last_terminated_exitcode{exported_namespace="sip-test",exported_pod=~"sipp-uas.*"})')"
      warn "INVOLUNTARY UAS CRASH (test-infra, NOT SUT): +${delta} restart(s), last exit=${exitc%.*} — [$pods] — flagged; overlapping chaos windows AND the next ${UAS_CRASH_DRAIN_SEC}s (dead-B-leg drain) will be TAINTED"
      date +%s > "$UAS_CRASH_TS_FILE"   # aftermath taint window: read by taint_if_uas_crash
      push_metric "sip_chaos_uas_crash_total ${cur%.*}
sip_chaos_window{type=\"uas_crash\"} 1
sip_chaos_event{type=\"uas_crash\",result=\"involuntary\"} 1"
      printf '{"ts":%s,"type":"uas_crash_involuntary","restart_total":%s,"delta":%d,"exit_code":%s,"pods":"%s","note":"SIPp v3.7.7 timer-wheel abort — LOAD GENERATOR fault, not the SUT"}\n' \
        "$(date +%s)" "${cur%.*}" "$delta" "${exitc%.*}" "$pods" >> "$EVENTS"
    else
      push_metric "sip_chaos_window{type=\"uas_crash\"} 0"
    fi
    last="$cur"
  done
}

# orphan_kill measurement: launch a dedicated 50cps stream, abruptly kill the UAC
# mid-call (no BYE), then assert the B2BUA REAPS the orphaned dialogs. Measured by
# the ghost gap, not a raw active delta, so the ~3-4k churning baseline doesn't
# swamp the signal. With keepalive at 300s the orphans reap ~305s after the kill,
# so ORPHAN_REAP_WAIT must clear 300s. A ghost gap that stays risen = leak.
ORPHAN_GHOST_TOL="${ORPHAN_GHOST_TOL:-250}"
orphan_event() {
  local idx="$1" t0 g0 gpk a0 g1 a1 rise result u0
  t0="$(date +%s)"
  g0="$(snap_ghost)"; a0="$(snap_active)"; u0="$(snap_uas_restarts)"
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
  result="$(taint_if_uas_crash "$result" "$u0" orphan_kill)"
  push_metric "sip_chaos_event{type=\"orphan_kill\",result=\"$result\"} 1
sip_chaos_ghost_rise $rise
sip_chaos_ghost_gap $g1"
  printf '{"ts":%s,"event":%d,"type":"orphan_kill","ghost_before":%s,"ghost_peak":%s,"ghost_after":%s,"ghost_rise":%s,"active_before":%s,"active_after":%s,"result":"%s"}\n' \
    "$t0" "$idx" "${g0%.*}" "${gpk%.*}" "${g1%.*}" "${rise%.*}" "${a0%.*}" "${a1%.*}" "$result" >> "$EVENTS"
  if [ "$result" = "pass" ]; then
    ok "CHAOS #$idx orphan_kill: REAPED — ghost gap $g0 ->(spike $gpk)-> $g1 (rise ${rise}, tol ${ORPHAN_GHOST_TOL})"
  elif [ "$result" = "tainted" ]; then
    warn "CHAOS #$idx orphan_kill: TAINTED by involuntary UAS crash — ghost rise ${rise} not attributable to the SUT (skip investigation)"
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
  local type="$1" idx="$2" t0 c0 cmid c1 result u0
  t0="$(date +%s)"
  c0="$(snap_limiter_conc)"; u0="$(snap_uas_restarts)"
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
  result="$(taint_if_uas_crash "$result" "$u0" "$type")"
  push_metric "sip_chaos_event{type=\"$type\",result=\"$result\"} 1
sip_chaos_limiter_conc{type=\"$type\"} $c1"
  printf '{"ts":%s,"event":%d,"type":"%s","limiter_before":%s,"limiter_mid":%s,"limiter_after":%s,"target":%s,"tol":%s,"result":"%s"}\n' \
    "$t0" "$idx" "$type" "${c0%.*}" "${cmid%.*}" "${c1%.*}" "$LIMITER_TARGET" "$LIMITER_TOL" "$result" >> "$EVENTS"
  if [ "$result" = "pass" ]; then
    ok "CHAOS #$idx $type: limiter reconverged — concurrent $c0 ->(fault $cmid)-> $c1 (target ${LIMITER_TARGET}±${LIMITER_TOL})"
  elif [ "$result" = "tainted" ]; then
    warn "CHAOS #$idx $type: TAINTED by involuntary UAS crash — limiter divergence not attributable to the SUT (skip investigation)"
  else
    warn "CHAOS #$idx $type: limiter did NOT reconverge — concurrent $c1 outside ${LIMITER_TARGET}±${LIMITER_TOL} after ${LIMITER_GRACE}s — FAILURE"
  fi
}

# Aftermath watch (2026-06-12). The #3 peak event destroyed essentially ALL
# standing long calls (~4000), but the teardown cascade ran for ~12 more minutes
# AFTER the gate's measurement window closed — so the event scored "long-loss
# 6.5% (260/4000)" instead of ~100%. After an event's verdict, keep polling the
# long-failed counter until it goes quiet (3 consecutive quiet polls) or
# AFTERMATH_MAX_SEC elapses; a tail above LONG_LOSS_TOL emits a separate
# "<type>_aftermath" FAIL row + metrics. Runs inside the inter-event idle
# (CHAOS_INTERVAL), so it consumes no schedule time.
AFTERMATH_MAX_SEC="${AFTERMATH_MAX_SEC:-540}"
AFTERMATH_POLL_SEC="${AFTERMATH_POLL_SEC:-30}"
long_aftermath_watch() {
  local type="$1" idx="$2" lf_close="$3" ldenom="$4" u0="$5"
  local waited=0 quiet=0 prev cur adf aloss result
  prev="$lf_close"
  while [ "$waited" -lt "$AFTERMATH_MAX_SEC" ] && [ "$quiet" -lt 3 ]; do
    sleep "$AFTERMATH_POLL_SEC"; waited=$(( waited + AFTERMATH_POLL_SEC ))
    cur="$(snap_failed_role long)"
    if python3 -c "exit(0 if float('$cur') > float('$prev') else 1)"; then
      quiet=0
    else
      quiet=$(( quiet + 1 ))
    fi
    prev="$cur"
  done
  adf=$(python3 -c "print(max(0,int(float('$prev'))-int(float('$lf_close'))))")
  [ "$adf" -eq 0 ] && return 0
  aloss=$(python3 -c "print(round(100.0*$adf/$ldenom,1) if $ldenom else 0.0)")
  result=$(python3 -c "print('pass' if float('$aloss')<=$LONG_LOSS_TOL else 'fail')")
  result="$(taint_if_uas_crash "$result" "$u0" "${type}_aftermath")"
  # Quarantine a whole-VM host freeze (WSL2 deschedule): if this fail's loss is
  # recv_timeout-dominated (frozen b2bua event loops — the A-leg's in-dialog request
  # goes unanswered) AND off-SIP-path pods (cdr/limiter) CPU collapsed in-window, it
  # is an INFRA artifact, NOT a SUT teardown (which is BYE-driven => unexpected_msg).
  # Downgrade fail -> tainted. The timeout_recv≫unexpected_msg interlock deliberately
  # leaves a genuine keepalive-BYE tail (unexpected_msg) FAILING the gate, so a real
  # HA regression is never masked. (2026-06-16 idx7 kill_worker_aftermath.)
  if [ "$result" = "fail" ]; then
    local tr um
    tr="$(vmq "sum(increase(sipp_failed_total{role=\"long\",cause=\"timeout_recv\"}[${waited}s]))")"
    um="$(vmq "sum(increase(sipp_failed_total{role=\"long\",cause=\"unexpected_msg\"}[${waited}s]))")"
    if python3 -c "import sys; sys.exit(0 if float('${tr:-0}') > 2*float('${um:-0}') else 1)" \
       && cluster_stalled_in_window "$waited"; then
      warn "  ↳ TAINTED (cluster-stall): off-path pods (cdr/limiter) CPU collapsed in-window = whole-VM host deschedule (WSL2). Long loss is recv_timeout from frozen b2bua event loops (timeout_recv≫unexpected_msg), NOT a SUT teardown — INFRA artifact, do not investigate the SUT."
      push_metric "sip_chaos_event{type=\"${type}_aftermath\",result=\"tainted_cluster_stall\"} 1"
      result="tainted"
    fi
  fi
  push_metric "sip_chaos_event{type=\"${type}_aftermath\",result=\"$result\"} 1
sip_chaos_long_loss_pct{type=\"${type}_aftermath\"} $aloss
sip_chaos_long_failed{type=\"${type}_aftermath\"} $adf"
  printf '{"ts":%s,"event":%d,"type":"%s_aftermath","long_failed":%d,"long_at_risk":%s,"long_loss_pct":%s,"watched_sec":%d,"result":"%s"}\n' \
    "$(date +%s)" "$idx" "$type" "$adf" "$ldenom" "$aloss" "$waited" "$result" >> "$EVENTS"
  if [ "$result" = "fail" ]; then
    warn "CHAOS #$idx $type AFTERMATH: long-call teardown CONTINUED past the gate window — $adf more long failures (${aloss}% > ${LONG_LOSS_TOL}%) over ${waited}s — FAILURE"
  else
    log "CHAOS #$idx $type aftermath: $adf more long failures (${aloss}%) over ${waited}s (within tolerance)"
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
  local sf0 ss0 lf0 lc0 sf1 ss1 lf1 lc1 sds sdf spct ldf ldc ldenom lconc0 lloss res_short res_long u0
  local lim1 res_limiter
  t0="$(date +%s)"
  s0="$(snap_success)"; f0="$(snap_failed)"; g0="$(snap_ghost)"; a0="$(snap_active)"; u0="$(snap_uas_restarts)"
  ss0="$(snap_success_short)"; sf0="$(snap_failed_short)"
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
  ss1="$(snap_success_short)"; sf1="$(snap_failed_short)"
  lf1="$(snap_failed_role long)";   lc1="$(snap_created_role long)"
  # (4) NO LIMITER PINNING (2026-06-12): calls caught mid-setup by the kill held
  # their cap20 slots while SIP-dead, pinning the limiter stream below the cap —
  # invisible to short/long/leak (and inside the old ±10 LIMITER_TOL). By now
  # (Ready + settle + sampling, past the 150 s setup deadline) the stream must be
  # back at the cap; take the MAX of 3 samples so a refill blip can't fail it.
  lim1="$(snap_limiter_conc)"
  for _ in 1 2; do
    sleep 15; s="$(snap_limiter_conc)"
    lim1=$(python3 -c "print(max(float('$lim1'),float('$s')))")
  done
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
  res_limiter=$(python3 -c "print('pass' if abs(float('$lim1')-$LIMITER_TARGET) <= $LIMITER_TOL else 'fail')")
  res_pct="$res_short"   # kept for the legacy sip_chaos_success_pct series
  # Overall pass requires ALL FOUR: short survival, long survival, no leak,
  # and the limiter stream back at its cap (no slot pinning by zombie calls).
  if [ "$(( ds + df ))" -eq 0 ]; then
    result="n/a"                       # baseline streams were down — not a real pass
  elif [ "$res_short" = pass ] && [ "$res_long" = pass ] && [ "$res_leak" = pass ] && [ "$res_limiter" = pass ]; then
    result="pass"
  else
    result="fail"
  fi
  result="$(taint_if_uas_crash "$result" "$u0" kill_worker)"
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
sip_chaos_ghost_gap{type=\"kill_worker\"} $g1
sip_chaos_limiter_conc{type=\"kill_worker\"} $lim1"
  printf '{"ts":%s,"event":%d,"type":"kill_worker","mode":"%s","short_survival_pct":%s,"short_ok":%d,"short_fail":%d,"long_loss_pct":%s,"long_failed":%d,"long_created":%d,"long_at_risk":%d,"blended_success_pct":%s,"ghost_before":%s,"ghost_after":%s,"ghost_rise":%s,"active_after":%s,"concurrent":%s,"limiter_conc":%s,"short_result":"%s","long_result":"%s","leak":"%s","limiter_result":"%s","result":"%s"}\n' \
    "$t0" "$idx" "$mode" "$spct" "$sds" "$sdf" "$lloss" "$ldf" "$ldc" "$ldenom" "$pct" "${g0%.*}" "${g1%.*}" "${rise%.*}" "${a1%.*}" "${conc%.*}" "${lim1%.*}" "$res_short" "$res_long" "$res_leak" "$res_limiter" "$result" >> "$EVENTS"
  case "$result" in
    pass)    ok   "CHAOS #$idx kill_worker($mode): short ${spct}% + long-loss ${lloss}%(tol ${LONG_LOSS_TOL}) + NO leak (ghost rise ${rise}) + limiter at cap (${lim1}/${LIMITER_TARGET}) — PASS" ;;
    n/a)     warn "CHAOS #$idx kill_worker($mode): no baseline calls resolved (streams down?) — n/a" ;;
    tainted) warn "CHAOS #$idx kill_worker($mode): TAINTED by involuntary UAS crash — long-loss ${lloss}%/leak ${rise} NOT attributable to the SUT (skip investigation)" ;;
    *)       warn "CHAOS #$idx kill_worker($mode): FAILURE — short ${spct}%/${thr}% ($res_short); LONG-LOSS ${lloss}% > ${LONG_LOSS_TOL}% ($res_long, ${ldf}/${ldc} long calls); leak ghost rise ${rise} ($res_leak); limiter ${lim1}/${LIMITER_TARGET}±${LIMITER_TOL} ($res_limiter — pinned slots = stuck-in-setup zombies)" ;;
  esac
  # Stamp THIS reboot's epoch for the next reboot's aftermath-taint comparison
  # (written AFTER taint_if_uas_crash ran above, so that read saw the PREVIOUS reboot).
  date +%s > "$LAST_REBOOT_TS_FILE"
  # Watch for a post-window long-teardown tail (consumes inter-event idle only).
  long_aftermath_watch kill_worker "$idx" "$lf1" "$ldenom" "$u0"
}

# cpu_starve_event = the OVERLOAD verification. One worker is made CPU-scarce
# (chaos.sh cpu_starve — NOT a traffic peak), which drives its ELU past the
# panic threshold so the Tier-3 admission gate engages. Three invariants, judged
# together (the experiment's explicit goals):
#
#   (1) IN-DIALOG UNAFFECTED — established long holds (and the 30s short holds
#       spanning the window) keep resolving: long-loss ≤ LONG_LOSS_TOL and the
#       ghost gap returns near baseline. In-dialog requests (re-INVITE/BYE/
#       keepalive OPTIONS) are never gated, so a starve that tears down standing
#       calls is a regression, not overload protection.
#   (2) NEW NON-EMERGENCY MAY BE SHED — the gate is allowed (expected) to 503
#       new non-emergency INVITEs. This is the signal the overload actually
#       ENGAGED: b2bua_overload_rejected_total must rise. If it does NOT, the
#       throttle was too loose to cross the ELU threshold → result n/a (calibrate
#       STARVE_QUOTA_US down), not a pass.
#   (3) NEW EMERGENCY NEVER SHED — short_em (Resource-Priority esnet.0) must NOT
#       fail and emergency admits must keep climbing across the window. Emergency
#       is force-admitted, so ANY short_em loss above EMERG_LOSS_TOL means the
#       exemption broke — a hard fail.
EMERG_LOSS_TOL="${EMERG_LOSS_TOL:-1}"   # max % of emergency short NEW calls allowed
                                        # to fail in-window (force-admit ⇒ ~0)
STARVE_GHOST_TOL="${STARVE_GHOST_TOL:-300}"
cpu_starve_event() {
  # $1=idx  $2=type label (cpu_starve | cpu_starve_all)  $3=chaos cmd (overload | overloadall)
  local idx="$1" ty="${2:-cpu_starve}" cmd="${3:-overload}" tgt
  local t0 g0 g1 s lf0 lc0 lconc0 lf1 lc1 ldf ldc ldenom lloss u0
  local emf0 emc0 emf1 emc1 emdf emdc emloss rej0 rej1 rejd ea0 ea1 ead
  local res_long res_leak res_emerg res_engaged result rise a0 a1
  tgt="$STARVE_TARGET"; [ "$cmd" = "overloadall" ] && tgt="ALL-workers"
  t0="$(date +%s)"
  g0="$(snap_ghost)"; a0="$(snap_active)"; u0="$(snap_uas_restarts)"
  lf0="$(snap_failed_role long)"; lc0="$(snap_created_role long)"; lconc0="$(snap_conc_role long)"
  emf0="$(snap_failed_role short_em)"; emc0="$(snap_created_role short_em)"
  rej0="$(snap_overload_rejected)"; ea0="$(snap_emergency_admitted)"
  local pr0 pr1 prd elu1
  pr0="$(snap_panic_rejected)"
  log "CHAOS #$idx: $ty $tgt (ghost=$g0 overload_rejected=$rej0 panic_elu=$pr0 emergency_admitted=$ea0)"
  push_metric "sip_chaos_run{type=\"$ty\",phase=\"inject\"} 1"
  open_window "$ty"
  # overload[all] = starve (one|all workers) + a small non-emergency peak (calibrated combo).
  STARVE_TARGET="$STARVE_TARGET" STARVE_SECS="$STARVE_SECS" \
    STARVE_QUOTA_US="$STARVE_QUOTA_US" STARVE_PERIOD_US="$STARVE_PERIOD_US" \
    PEAK_CAPS="$STARVE_PEAK_CAPS" PEAK_SECS="$STARVE_SECS" \
    ./chaos.sh "$cmd" >>"$RUNLOG" 2>&1 || true
  sleep "$SETTLE"
  ensure_baseline
  # Ghost-gap FLOOR over a short window (robust to a one-scrape blip).
  g1="$(snap_ghost)"
  for _ in 1 2 3 4 5; do sleep 12; s="$(snap_ghost)"; g1=$(python3 -c "print(min(float('$g1'),float('$s')))"); done
  a1="$(snap_active)"
  lf1="$(snap_failed_role long)"; lc1="$(snap_created_role long)"
  emf1="$(snap_failed_role short_em)"; emc1="$(snap_created_role short_em)"
  rej1="$(snap_overload_rejected)"; ea1="$(snap_emergency_admitted)"
  pr1="$(snap_panic_rejected)"; elu1="$(snap_elu_max)"
  close_window "$ty"
  prd=$(python3 -c "print(max(0,int(float('$pr1'))-int(float('$pr0'))))")
  # In-dialog (long) survival.
  ldf=$(python3 -c "print(max(0,int(float('$lf1'))-int(float('$lf0'))))")
  ldc=$(python3 -c "print(max(0,int(float('$lc1'))-int(float('$lc0'))))")
  ldenom=$(python3 -c "print(max(int(float('$lconc0')), $ldc, ${LONG_LOSS_DENOM_FLOOR:-100}))")
  lloss=$(python3 -c "print(round(100.0*$ldf/$ldenom,1) if $ldenom else 0.0)")
  rise=$(python3 -c "print(round(float('$g1')-float('$g0'),0))")
  # Emergency NEW-call survival (short_em failed / created in-window).
  emdf=$(python3 -c "print(max(0,int(float('$emf1'))-int(float('$emf0'))))")
  emdc=$(python3 -c "print(max(0,int(float('$emc1'))-int(float('$emc0'))))")
  emloss=$(python3 -c "print(round(100.0*$emdf/$emdc,1) if $emdc else 0.0)")
  # Overload engagement + emergency-admit progress.
  rejd=$(python3 -c "print(max(0,int(float('$rej1'))-int(float('$rej0'))))")
  ead=$(python3 -c "print(max(0,int(float('$ea1'))-int(float('$ea0'))))")
  res_long=$(python3 -c "print('pass' if float('$lloss')<=$LONG_LOSS_TOL else 'fail')")
  res_leak=$(python3 -c "print('pass' if float('$rise')<=$STARVE_GHOST_TOL else 'fail')")
  res_emerg=$(python3 -c "print('pass' if float('$emloss')<=$EMERG_LOSS_TOL else 'fail')")
  res_engaged=$(python3 -c "print('yes' if $rejd>0 else 'no')")
  if [ "$res_engaged" = "no" ]; then
    result="n/a"   # overload never tripped — throttle too loose, calibrate STARVE_QUOTA_US down
  elif [ "$res_long" = pass ] && [ "$res_leak" = pass ] && [ "$res_emerg" = pass ]; then
    result="pass"
  else
    result="fail"
  fi
  result="$(taint_if_uas_crash "$result" "$u0" "$ty")"
  push_metric "sip_chaos_event{type=\"$ty\",result=\"$result\"} 1
sip_chaos_long_loss_pct{type=\"$ty\"} $lloss
sip_chaos_emerg_loss_pct{type=\"$ty\"} $emloss
sip_chaos_overload_rejected{type=\"$ty\"} $rejd
sip_chaos_panic_elu_rejected{type=\"$ty\"} $prd
sip_chaos_elu_max{type=\"$ty\"} ${elu1:-0}
sip_chaos_emergency_admitted{type=\"$ty\"} $ead
sip_chaos_ghost_rise{type=\"$ty\"} $rise"
  printf '{"ts":%s,"event":%d,"type":"%s","target":"%s","long_loss_pct":%s,"long_failed":%d,"long_at_risk":%d,"emerg_loss_pct":%s,"emerg_failed":%d,"emerg_created":%d,"overload_rejected_delta":%d,"panic_elu_rejected_delta":%d,"elu_max":%s,"emergency_admitted_delta":%d,"ghost_rise":%s,"long_result":"%s","leak":"%s","emerg_result":"%s","engaged":"%s","result":"%s"}\n' \
    "$t0" "$idx" "$ty" "$tgt" "$lloss" "$ldf" "$ldenom" "$emloss" "$emdf" "$emdc" "$rejd" "$prd" "${elu1:-0}" "$ead" "${rise%.*}" "$res_long" "$res_leak" "$res_emerg" "$res_engaged" "$result" >> "$EVENTS"
  case "$result" in
    pass)    ok   "CHAOS #$idx $ty: OVERLOAD engaged (rejected ${rejd} non-emergency; panic_elu ${prd}; ELU ${elu1}) + emergency safe (loss ${emloss}%, +${ead} admitted) + in-dialog safe (long-loss ${lloss}%, ghost rise ${rise}) — PASS" ;;
    n/a)     warn "CHAOS #$idx $ty: overload did NOT engage (0 non-emergency rejected; ELU peaked ${elu1} vs 0.75 threshold) — throttle too loose, lower STARVE_QUOTA_US (current ${STARVE_QUOTA_US}/${STARVE_PERIOD_US}) — n/a" ;;
    tainted) warn "CHAOS #$idx $ty: TAINTED by involuntary UAS crash — not attributable to the SUT (skip investigation)" ;;
    *)       warn "CHAOS #$idx $ty: FAILURE — emergency loss ${emloss}%/${EMERG_LOSS_TOL}% ($res_emerg); in-dialog long-loss ${lloss}%/${LONG_LOSS_TOL}% ($res_long); leak ghost rise ${rise} ($res_leak) [overload engaged: rejected ${rejd}]" ;;
  esac
  long_aftermath_watch "$ty" "$idx" "$lf1" "$ldenom" "$u0"
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
  # cpu_starve = OVERLOAD via CPU scarcity on ONE worker: asserts in-dialog survival
  # + emergency never shed + non-emergency CAN be shed — see cpu_starve_event.
  if [ "$type" = "cpu_starve" ]; then cpu_starve_event "$idx" cpu_starve overload; return; fi
  # cpu_starve_all = the WORST case: ALL workers starved at once (no healthy peer to
  # absorb traffic), so every emergency call lands on a starved worker. Same gate —
  # emergency must STILL be ≈0-impact and only non-emergency shed.
  if [ "$type" = "cpu_starve_all" ]; then cpu_starve_event "$idx" cpu_starve_all overloadall; return; fi
  local t0 s0 f0 s1 f1 ds df pct result conc u0
  local lf0 lc0 lconc0 lf1 lc1 ldf ldc ldenom lloss res_blend res_long
  t0="$(date +%s)"
  s0="$(snap_success)"; f0="$(snap_failed)"; u0="$(snap_uas_restarts)"
  # AT-RISK long population: long calls FAILED/CREATED/HELD at inject. A new-call
  # burst (peak) must not tear down ESTABLISHED long dialogs — measured APART from
  # the blended success% (short@100cps drowns long loss out of any blend, the same
  # blind spot reboot_event guards). See the long-loss gate below.
  lf0="$(snap_failed_role long)"; lc0="$(snap_created_role long)"; lconc0="$(snap_conc_role long)"
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
  lf1="$(snap_failed_role long)"; lc1="$(snap_created_role long)"
  ds=$(python3 -c "print(max(0, int(float('$s1'))-int(float('$s0'))))")
  df=$(python3 -c "print(max(0, int(float('$f1'))-int(float('$f0'))))")
  pct=$(python3 -c "t=$ds+$df; print(round(100.0*$ds/t,1) if t else 100.0)")
  # LONG loss% = long-failed-delta / at-risk denominator (max of held-at-inject,
  # created-delta, floor) — the same AT-RISK shape reboot_event uses so a stream
  # sitting at its `-l` ceiling (created-delta ~0) still gets a real denominator.
  ldf=$(python3 -c "print(max(0,int(float('$lf1'))-int(float('$lf0'))))")
  ldc=$(python3 -c "print(max(0,int(float('$lc1'))-int(float('$lc0'))))")
  ldenom=$(python3 -c "print(max(int(float('$lconc0')), $ldc, ${LONG_LOSS_DENOM_FLOOR:-100}))")
  lloss=$(python3 -c "print(round(100.0*$ldf/$ldenom,1) if $ldenom else 0.0)")
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
  # rather than a vacuous 100%. Otherwise pass requires BOTH the blended success%
  # bar AND long-dialog survival (long-loss ≤ LONG_LOSS_TOL): a peak that absorbs
  # new calls but tears down established long holds (the txn-channel keepalive-
  # shed cascade) must FAIL, not hide behind a short-dominated blend.
  res_blend=$(python3 -c "print('pass' if float('$pct')>=$thr else 'fail')")
  res_long=$(python3 -c "print('pass' if float('$lloss')<=$LONG_LOSS_TOL else 'fail')")
  if [ "$(( ds + df ))" -eq 0 ]; then
    result="n/a"
  elif [ "$res_blend" = pass ] && [ "$res_long" = pass ]; then
    result="pass"
  else
    result="fail"
  fi
  result="$(taint_if_uas_crash "$result" "$u0" "$type")"

  close_window "$type"
  push_metric "sip_chaos_event{type=\"$type\",result=\"$result\"} 1
sip_chaos_success_pct{type=\"$type\"} $pct
sip_chaos_long_loss_pct{type=\"$type\"} $lloss
sip_chaos_long_failed{type=\"$type\"} $ldf
sip_chaos_long_at_risk{type=\"$type\"} $ldenom
sip_chaos_resolved{type=\"$type\",outcome=\"success\"} $ds
sip_chaos_resolved{type=\"$type\",outcome=\"failed\"} $df"

  printf '{"ts":%s,"event":%d,"type":"%s","success_delta":%d,"failed_delta":%d,"success_pct":%s,"long_loss_pct":%s,"long_failed":%d,"long_at_risk":%d,"concurrent":%s,"blend_result":"%s","long_result":"%s","result":"%s"}\n' \
    "$t0" "$idx" "$type" "$ds" "$df" "$pct" "$lloss" "$ldf" "$ldenom" "${conc%.*}" "$res_blend" "$res_long" "$result" >> "$EVENTS"

  case "$result" in
    pass)    ok   "CHAOS #$idx $type: ${pct}% resolved OK + long-loss ${lloss}%(tol ${LONG_LOSS_TOL}) (thr ${thr}%, Δok=$ds Δfail=$df)" ;;
    n/a)     warn "CHAOS #$idx $type: no baseline calls resolved in window (streams down?) — n/a" ;;
    tainted) warn "CHAOS #$idx $type: TAINTED by involuntary UAS crash — ${pct}%/long-loss ${lloss}% NOT attributable to the SUT (skip investigation)" ;;
    *)       warn "CHAOS #$idx $type: FAILURE — blended ${pct}%/${thr}% ($res_blend); LONG-LOSS ${lloss}% > ${LONG_LOSS_TOL}% ($res_long, ${ldf}/${ldenom} long) (Δok=$ds Δfail=$df)" ;;
  esac
  # Watch for a post-window long-teardown tail (the e3 peak cascade ran ~12 min
  # past the gate and was under-reported 6.5% vs ~100%).
  long_aftermath_watch "$type" "$idx" "$lf1" "$ldenom" "$u0"
}

# --- deploy preflight + readiness gates (issue1) ---------------------------------
# Normalise a k8s CPU quantity (e.g. "2", "500m", "1500m") to millicores (integer).
cpu_to_millis() {  # $1 = quantity
  python3 - "$1" <<'PY' 2>/dev/null || echo 0
import sys, re
q = (sys.argv[1] or "0").strip()
if not q:
    print(0); raise SystemExit
m = re.match(r'^(\d+(?:\.\d+)?)(m?)$', q)
if not m:
    print(0); raise SystemExit
v = float(m.group(1))
print(int(round(v * (1 if m.group(2) == 'm' else 1000))))
PY
}

# Capacity preflight: BEFORE the StatefulSet is applied, verify the 5 tier=load
# sipp-uas replicas (each requesting UAS_CPU_REQ cores) actually fit on the tier=load
# nodes given what is ALREADY scheduled there. Reads node allocatable CPU and the sum
# of existing (non-terminated) pod CPU requests per tier=load node via kubectl/python.
# DEGRADES TO A WARNING (returns 0) if it cannot compute (no jq dependency; tolerant
# of odd numbers) so it never wedges a run on a parsing hiccup — it only HARD-FAILS
# when it can confidently prove the pods will not fit.
UAS_REPLICAS="${UAS_REPLICAS:-5}"          # must match manifests/10-sipp-uas.yaml replicas
UAS_CPU_REQ_CORES="${UAS_CPU_REQ_CORES:-2}" # must match the UAS pod cpu request (millis below)
capacity_preflight() {
  local want_millis allocatable_millis used_millis headroom_millis
  want_millis=$(( UAS_REPLICAS * $(cpu_to_millis "${UAS_CPU_REQ_CORES}") ))
  # Allocatable CPU summed over tier=load nodes.
  local nodes
  nodes="$(kubectl get nodes -l tier=load -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null)"
  if [ -z "$nodes" ]; then
    warn "capacity preflight: no tier=load nodes found (or kubectl unavailable) — skipping (degraded to warning)"
    return 0
  fi
  allocatable_millis=0
  local n a
  while read -r n; do
    [ -n "$n" ] || continue
    a="$(kubectl get node "$n" -o jsonpath='{.status.allocatable.cpu}' 2>/dev/null)"
    allocatable_millis=$(( allocatable_millis + $(cpu_to_millis "$a") ))
  done <<< "$nodes"
  # Existing requested CPU on those nodes (sum of container requests for pods bound
  # to a tier=load node, excluding Succeeded/Failed). Computed entirely in python so
  # a missing field never aborts the loop.
  # The python script lives in a heredoc-to-VARIABLE (a SIMPLE `cat` substitution —
  # the same safe shape cpu_to_millis uses), NOT inline in the pipeline below. A
  # heredoc inside a `$( ... | ... || echo 0 )` pipeline trips bash's RUNTIME parser
  # ("command substitution: syntax error near ||" — invisible to `bash -n`, so it
  # only blows up once tier=load nodes exist and this path runs), AND the `<<'PY'`
  # heredoc would override the pipe as stdin so the kubectl JSON never reached python
  # (used would always be 0). With `python3 -c "$used_py"` the JSON pipes to stdin,
  # the script comes from -c, and $nodes is argv[1].
  local used_py
  used_py="$(cat <<'PY'
import sys, json, re
load_nodes = set(filter(None, (l.strip() for l in sys.argv[1].splitlines())))
def millis(q):
    q = (q or "").strip()
    m = re.match(r'^(\d+(?:\.\d+)?)(m?)$', q)
    if not m: return 0
    return int(round(float(m.group(1)) * (1 if m.group(2) == 'm' else 1000)))
try:
    d = json.load(sys.stdin)
except Exception:
    print(0); raise SystemExit
total = 0
for p in d.get("items", []):
    if p.get("spec", {}).get("nodeName") not in load_nodes:
        continue
    for c in p.get("spec", {}).get("containers", []):
        total += millis(c.get("resources", {}).get("requests", {}).get("cpu"))
print(total)
PY
)"
  used_millis="$(kubectl get pods -A \
      --field-selector=status.phase!=Succeeded,status.phase!=Failed \
      -o json 2>/dev/null \
    | python3 -c "$used_py" "$nodes" 2>/dev/null || echo 0)"
  if [ "${allocatable_millis:-0}" -le 0 ]; then
    warn "capacity preflight: could not read tier=load allocatable CPU — skipping (degraded to warning)"
    return 0
  fi
  headroom_millis=$(( allocatable_millis - used_millis ))
  log "capacity preflight: sipp-uas wants $(( want_millis / 1000 )) CPU (${UAS_REPLICAS}×${UAS_CPU_REQ_CORES}); tier=load allocatable $(( allocatable_millis / 1000 )) CPU, already requested $(( used_millis / 1000 )) CPU, headroom ~$(( headroom_millis / 1000 )) CPU"
  if [ "$want_millis" -gt "$headroom_millis" ]; then
    warn "sipp-uas requests $(( want_millis / 1000 )) CPU across tier=load but headroom is ~$(( headroom_millis / 1000 )) CPU — refusing to deploy (lower sipp-uas replicas/cpu request or free node capacity)"
    kubectl get nodes -l tier=load -o wide 2>/dev/null | tee -a "$RUNLOG" || true
    return 1
  fi
  return 0
}

# Hard readiness gate: refuse to continue after a partial deploy. Asserts the
# required workloads are Ready before baseline traffic starts (issue1: wireup used
# to fall through on a stuck rollout and baseline never produced traffic).
assert_workloads_ready() {
  local ok_all=1
  # sipp-uas: ALL replicas Ready (the StatefulSet that wedged Pending on the 5th pod).
  local desired ready
  # Desired = .spec.replicas (the authoritative target); .status.replicas can lag
  # mid-reconcile and read low, letting the gate pass before the STS is fully scaled.
  desired="$(kubectl -n "$NS" get statefulset sipp-uas -o jsonpath='{.spec.replicas}' 2>/dev/null || echo)"
  ready="$(kubectl -n "$NS" get statefulset sipp-uas -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo 0)"
  if [ -z "$desired" ] || [ "${ready:-0}" != "$desired" ]; then
    warn "sipp-uas not fully ready (${ready:-0}/${desired:-?} replicas Ready)"
    ok_all=0
  fi
  # b2bua workers: all replicas Ready.
  local wdesired wready
  wdesired="$(kubectl -n "$NS" get statefulset b2bua-worker -o jsonpath='{.spec.replicas}' 2>/dev/null || echo)"
  wready="$(kubectl -n "$NS" get statefulset b2bua-worker -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo 0)"
  if [ -z "$wdesired" ] || [ "${wready:-0}" != "$wdesired" ]; then
    warn "b2bua-worker not fully ready (${wready:-0}/${wdesired:-?} replicas Ready)"
    ok_all=0
  fi
  # front proxy: at least one Ready replica.
  local pready
  pready="$(kubectl -n "$NS" get deploy sip-front-proxy -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo 0)"
  if [ "${pready:-0}" -lt 1 ]; then
    warn "sip-front-proxy has no Ready replica"
    ok_all=0
  fi
  if [ "$ok_all" -ne 1 ]; then
    warn "deploy is PARTIAL — dumping pending pods + their reasons for diagnosis:"
    kubectl -n "$NS" get pods -o wide 2>/dev/null | tee -a "$RUNLOG" || true
    kubectl -n "$NS" get pods --field-selector=status.phase=Pending \
      -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null \
      | while read -r pp; do
          [ -n "$pp" ] || continue
          echo "--- describe pod $pp (last events) ---" | tee -a "$RUNLOG"
          kubectl -n "$NS" describe pod "$pp" 2>/dev/null | sed -n '/Events:/,$p' | tee -a "$RUNLOG" || true
        done
    return 1
  fi
  return 0
}

# Traffic-started assertion: within ~60s confirm at least one sipp-uac pod exists AND
# calls are actually being created (sipp_calls_created_total increasing). Fails fast
# with the specific reason so a "clean-looking" zero-traffic run is impossible.
assert_traffic_started() {
  local deadline=$(( $(date +%s) + 60 ))
  local c0 saw_pod=0
  c0="$(vmq 'sum(sipp_calls_created_total)')"
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if kubectl -n "$NS" get pods -l app=sipp-uac -o name 2>/dev/null | grep -q . ; then
      saw_pod=1
      local c1; c1="$(vmq 'sum(sipp_calls_created_total)')"
      if python3 -c "exit(0 if float('$c1') > float('$c0') else 1)" 2>/dev/null; then
        ok "traffic started: sipp-uac pods present and calls created ($c0 -> $c1)"
        return 0
      fi
    fi
    sleep 5
  done
  if [ "$saw_pod" -ne 1 ]; then
    warn "no sipp-uac pod appeared within 60s of starting baseline — traffic NEVER started (check the UAC jobs / scheduling)"
  else
    warn "sipp-uac pods exist but sipp_calls_created_total did not increase within 60s — no calls being placed (check SIPp logs / proxy reachability)"
    kubectl -n "$NS" logs -l app=sipp-uac -c sipp-uac --tail=40 2>/dev/null | tee -a "$RUNLOG" || true
  fi
  return 1
}

wireup() {
  mkdir -p "$RUN_DIR"
  # Bootstrap the cluster if it is ABSENT. The endurance flow historically assumed
  # an already-up cluster — wireup only `deploy`s — so from a COLD machine (no kind
  # cluster) `kind load` + `./run.sh deploy` failed with "no nodes found for cluster
  # / context does not exist" and the run produced ZERO traffic. `./run.sh up` creates
  # the cluster + builds + loads the current-source images + brings up observability —
  # exactly wireup's own build/load block — so when we bootstrap here we SKIP_BUILD the
  # redundant rebuild below and go straight to deploy.
  if ! kind get clusters 2>/dev/null | grep -qx "${CLUSTER:-sip-e2e}"; then
    log "wireup: kind cluster '${CLUSTER:-sip-e2e}' absent — bootstrapping via ./run.sh up (build + load + obs)"
    if ! ./run.sh up >>"$RUNLOG" 2>&1; then
      warn "wireup: ./run.sh up failed to bring up cluster '${CLUSTER:-sip-e2e}' — see $RUNLOG"
      return 1
    fi
    SKIP_BUILD=1   # up just built + loaded current source; skip the redundant rebuild
  fi
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
    log "wireup: proxy env (forwarded to docker build): $(proxy_env_summary)"
    # docker_build (lib/proxy-env.sh) forwards the host proxy as build-args so the
    # rebuild works on a PROXIFIED host; merged NO_PROXY keeps local fetches direct.
    docker_build -f "$REPO_ROOT/deploy/docker/Dockerfile" -t "$SUT_IMAGE" "$REPO_ROOT" >>"$RUNLOG" 2>&1
    docker_build -f "$REPO_ROOT/deploy/docker/Dockerfile.keepalived" -t "$KEEPALIVED_IMAGE" "$REPO_ROOT" >>"$RUNLOG" 2>&1
    docker_build -t sipp:dev "$SIPP_DIR" >>"$RUNLOG" 2>&1
    log "wireup: pulling RabbitMQ image $RABBITMQ_IMAGE (CDR transport)"
    docker image inspect "$RABBITMQ_IMAGE" >/dev/null 2>&1 || docker pull "$RABBITMQ_IMAGE" >>"$RUNLOG" 2>&1
    log "wireup: loading images into kind"
    kind load docker-image "$SUT_IMAGE" --name "$CLUSTER" >>"$RUNLOG" 2>&1
    kind load docker-image "$KEEPALIVED_IMAGE" --name "$CLUSTER" >>"$RUNLOG" 2>&1
    kind load docker-image sipp:dev --name "$CLUSTER" >>"$RUNLOG" 2>&1
    kind load docker-image "$RABBITMQ_IMAGE" --name "$CLUSTER" >>"$RUNLOG" 2>&1
  fi
  # Capacity preflight BEFORE deploy applies the sipp-uas StatefulSet: prove the 5
  # tier=load replicas fit, else fail with a clear headroom message (issue1 — the 5th
  # UAS pod stayed Pending on CPU and deploy timed out). Skippable for odd clusters
  # via CAPACITY_PREFLIGHT=0; degrades to a warning if it cannot compute.
  if [ "${CAPACITY_PREFLIGHT:-1}" = "1" ]; then
    capacity_preflight || { warn "wireup: capacity preflight failed — refusing to deploy"; return 1; }
  fi
  log "wireup: deploy stack (repl on, ${WORKER_REPLICAS} workers) — same path as cluster start"
  REPL_ENABLE=1 WORKER_REPLICAS="$WORKER_REPLICAS" OBS_ENABLE="${OBS_ENABLE:-1}" ./run.sh deploy >>"$RUNLOG" 2>&1
  # imagePullPolicy=IfNotPresent + an unchanged image tag means `kubectl apply`
  # will NOT restart pods onto the freshly-loaded image. Force a rollout so the
  # new binary actually runs, then wait Ready before driving any traffic — else
  # we'd repeat the stale-binary trap above.
  if [ "${SKIP_BUILD:-0}" != "1" ]; then
    log "wireup: rolling workers/proxy/uas onto the freshly-built image"
    kubectl -n "$NS" rollout restart statefulset/b2bua-worker deploy/sip-front-proxy statefulset/sipp-uas deploy/cdr-consumer >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status statefulset/b2bua-worker --timeout=300s >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status deploy/sip-front-proxy --timeout=180s >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status statefulset/sipp-uas --timeout=180s >>"$RUNLOG" 2>&1 || true
    kubectl -n "$NS" rollout status deploy/cdr-consumer --timeout=180s >>"$RUNLOG" 2>&1 || true
  fi
  log "wireup: (re)load observability dashboards/scrape"
  [ -x "$OBS_DIR/install.sh" ] && "$OBS_DIR/install.sh" --apply >>"$RUNLOG" 2>&1 || true
  # HARD readiness gate: refuse to continue (and to start baseline traffic) after a
  # partial deploy. The rollout-status waits above are `|| true`-soft, so without this
  # a stuck StatefulSet (e.g. a Pending UAS pod) would fall through and the run would
  # generate ZERO traffic while looking fine (issue1).
  if ! assert_workloads_ready; then
    warn "sipp-uas/b2bua/proxy not fully ready; refusing to start baseline traffic"
    return 1
  fi
  ok "wireup complete"
}

start_baseline() {
  log "starting baseline streams: long@${LONG_CPS}(${LONG_SHARDS} shards×${LONG_SHARD_CPS}cps) reinvite@${REINVITE_CPS} short_em@${SHORT_EM_CPS}+short_ne@${SHORT_NE_CPS}(=${SHORT_CPS}) abuse@${ABUSE_CAPS} limiter@${LIMITER_CPS}(cap ${LIMITER_TARGET})"
  local job scenario cps role
  while read -r job scenario cps role; do
    [ -n "$job" ] && launch_stream "$job" "$scenario" "$cps" "$role"
  done < <(baseline_specs)
  ABUSE_CAPS="$ABUSE_CAPS" ./chaos.sh abuse up >>"$RUNLOG" 2>&1 || true
  push_metric "sip_endurance_run{phase=\"start\"} 1"
}

stop_streams() {
  log "stopping baseline + abuse streams"
  local job _rest
  # Delete every long shard + the fixed-name streams + the transient chaos jobs.
  { baseline_specs | awk '{print $1}'; printf '%s\n' sipp-uac-peak sipp-uac-orphan; } \
    | while read -r job; do
        kubectl -n "$NS" delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
      done
  ./chaos.sh abuse down >>"$RUNLOG" 2>&1 || true
  push_metric "sip_endurance_run{phase=\"stop\"} 1"
}

run() {
  mkdir -p "$RUN_DIR"; : > "$EVENTS"
  log "=== ENDURANCE RUN $TS (smoke=${SMOKE:-0}) dur=${DURATION}s interval=${CHAOS_INTERVAL}s ==="
  log "results: $RUN_DIR"
  # wireup now hard-fails (returns non-zero) on a partial deploy / failed capacity
  # preflight; propagate that as a run failure instead of barrelling on to start
  # traffic that will never flow (issue1).
  if ! wireup; then
    warn "=== ENDURANCE RUN ABORTED: wire-up failed (deploy partial or capacity preflight failed) — no baseline traffic started ==="
    exit 1
  fi
  # Gate readiness before driving traffic.
  kubectl -n "$NS" wait --for=condition=ready pod -l app=b2bua-worker --timeout=120s >>"$RUNLOG" 2>&1 || true
  kubectl -n "$NS" rollout status deploy/sip-front-proxy --timeout=90s >>"$RUNLOG" 2>&1 || true
  # Hold the UAC streams back so the proxy's worker-discovery informer is warm
  # before traffic starts (avoids the startup failed-call spike).
  log "cluster-settle ${CLUSTER_SETTLE}s (let the proxy discover workers) before starting UAC streams"
  sleep "$CLUSTER_SETTLE"
  start_baseline
  # Assert traffic ACTUALLY started (sipp-uac pods exist + calls are being created)
  # within ~60s, so a silent zero-traffic run fails fast with the specific reason
  # instead of soaking for two hours against nothing (issue1).
  if ! assert_traffic_started; then
    warn "=== ENDURANCE RUN ABORTED: baseline traffic did not start within 60s ==="
    stop_streams || true
    exit 1
  fi
  # Watch for INVOLUNTARY SIPp-UAS crashes (exit-255 timer-wheel aborts) for the
  # whole run, so a load-generator fault is flagged + taints (not blamed on) the SUT.
  uas_crash_watcher & UAS_WATCH_PID=$!
  log "uas_crash_watcher started (pid $UAS_WATCH_PID) — involuntary UAS crashes will be flagged + taint overlapping events"

  local cycle
  if [ "${REBOOT_FOCUS:-0}" = "1" ]; then
    # Reboot-focused run: every chaos event is a b2bua worker reboot (modes
    # alternate graceful/crash inside reboot_event), to hammer the ADR-0014
    # takeover/reclaim/self-release path.
    cycle=(kill_worker)
    log "REBOOT_FOCUS=1 — chaos cycle is b2bua worker reboot only (graceful/crash alternating)"
  else
    cycle=(orphan_kill kill_worker kill_proxy peak cpu_starve cpu_starve_all limiter_kill limiter_netcut)
  fi
  local start now idx=0
  start="$(date +%s)"
  if [ "${NO_CHAOS:-0}" = "1" ]; then
    # Pure baseline soak: zero fault injection — used for the long memory/CPU/
    # fragmentation run. We still loop ensure_baseline on the CHAOS_INTERVAL
    # cadence so an involuntary SIPp UAC self-abort (exit 255) is restarted and
    # steady load is held for the whole window (the only difference from a chaos
    # run is that no chaos_event is injected).
    log "NO_CHAOS=1 — pure baseline soak for ${DURATION}s (stream supervision only, zero fault injection)"
    while :; do
      now="$(date +%s)"
      [ $(( now - start )) -ge "$DURATION" ] && break
      ensure_baseline
      sleep "$CHAOS_INTERVAL"
    done
  else
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
  fi

  [ -n "$UAS_WATCH_PID" ] && kill "$UAS_WATCH_PID" >/dev/null 2>&1 || true
  log "=== run window elapsed — $idx chaos events injected ==="
  local uas_total; uas_total="$(snap_uas_restarts)"
  log "involuntary SIPp-UAS crashes over the run (cumulative restarts): ${uas_total%.*} — see uas_crash_involuntary rows in events.jsonl"
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

trap 'warn "interrupted — stopping streams"; [ -n "$WINDOW_HB_PID" ] && kill "$WINDOW_HB_PID" 2>/dev/null; [ -n "${UAS_WATCH_PID:-}" ] && kill "$UAS_WATCH_PID" 2>/dev/null; stop_streams || true' INT TERM

cmd="${1:-run}"; shift || true
case "$cmd" in
  run)    run ;;
  wireup) wireup ;;
  stop)   stop_streams ;;
  *) printf 'usage: %s {run|wireup|stop}\n' "$0" >&2; exit 1 ;;
esac
