#!/usr/bin/env bash
# Goal-3 (S11) HA-replication CHAOS suite for the Rust SIP SUT on kind.
#
# This is the real-clock, real-TCP, real-k8s acceptance for peer-to-peer call
# replication (ADR-0011): it stands up the full stack WITH replication enabled,
# drives long-hold dialogs through the proxy, KILLS the worker holding a dialog
# mid-call, and asserts the dialog SURVIVES — the in-dialog BYE lands on the
# backup worker (which holds the replica) and is answered 200. That is "call
# survival + convergence", the goal-3 bar.
#
# It is deliberately a SHELL script (not a `cargo test`): a real kind cluster +
# image builds are slow and WSL2-flaky, so it must not gate `cargo test
# --workspace`. Run it explicitly when you want the real chaos signal.
#
#   ./chaos.sh failover        # up + deploy(repl) + hold-failover under pod kill
#   ./chaos.sh up              # just (re)create cluster + build/load images (repl)
#   ./chaos.sh deploy          # just deploy the repl-enabled stack
#   ./chaos.sh kill            # inject one worker kill against a running stack
#   ./chaos.sh down            # tear the cluster down
#
# Env knobs:
#   CALLS=30           total hold dialogs to place
#   CPS=3              calls/sec (CALLS/CPS should be << the 15s hold so all
#                      dialogs are simultaneously in-hold when we kill)
#   KILL_TARGET=b2bua-worker-0   pod to kill mid-hold
#   PASS_THRESHOLD=90  min % successful calls to PASS (best-effort failover, X5)
#   KEEP=1             leave the cluster up after the run (default tear down off)
#
# Shares cluster name `sip-e2e` (WSL one-cluster switch) — see README/run.sh.
set -euo pipefail
cd "$(dirname "$0")"
HERE="$(pwd)"

NS="${NS:-sip-test}"
CALLS="${CALLS:-30}"
CPS="${CPS:-3}"
KILL_TARGET="${KILL_TARGET:-b2bua-worker-0}"
PASS_THRESHOLD="${PASS_THRESHOLD:-90}"
SCENARIO="uac-hold-failover.xml"
JOB="sipp-uac-failover"

# Replication ON, ≥2 workers so a primary + backup live on different app nodes.
export REPL_ENABLE=1
export WORKER_REPLICAS="${WORKER_REPLICAS:-2}"
export SCENARIO

log()  { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
ok()   { printf '\033[1;32mPASS: %s\033[0m\n' "$*" >&2; }
fail() { printf '\033[1;31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

# Reuse run.sh for the heavy lifting (image build, cluster, base deploy) so the
# two stay in lock-step on topology + image. run.sh reads REPL_ENABLE/SCENARIO
# from the environment we exported above.
up()     { REPL_ENABLE=1 ./run.sh up; }
deploy() { REPL_ENABLE=1 ./run.sh deploy; }
down()   { ./run.sh down; }

# Wait until every worker reports Ready (re-hydrated + backup-current via the
# /ready probe the StatefulSet's readinessProbe consumes).
wait_ready() {
  log "waiting for all workers to be Ready (re-hydrated + backup-current)"
  kubectl -n "$NS" rollout status statefulset/b2bua-worker --timeout=120s
  kubectl -n "$NS" wait --for=condition=ready pod -l app=b2bua-worker --timeout=120s
}

# Launch the hold-failover UAC job: CALLS dialogs at CPS, each INVITE/ACK/15s
# hold/BYE. SIPp exits 0 only if every call succeeded.
launch_calls() {
  export UAC_JOB_NAME="$JOB" CAPS="$CPS" MAX_CALLS="$CALLS"
  kubectl -n "$NS" delete job "$JOB" --ignore-not-found >/dev/null 2>&1 || true
  log "launching $CALLS hold dialogs @ ${CPS}cps (scenario=$SCENARIO)"
  envsubst < manifests/40-sipp-uac-job.yaml | kubectl apply -f -
  kubectl -n "$NS" wait --for=condition=ready pod -l app=sipp-uac --timeout=60s || true
}

# Inject ONE fault: delete the pod holding (statistically) a share of the live
# dialogs while they sit in hold. StatefulSet recreates it under a fresh
# incarnation; the surviving worker serves the backup replica so in-dialog BYEs
# still terminate cleanly.
kill_worker() {
  log "CHAOS: killing $KILL_TARGET mid-hold"
  kubectl -n "$NS" delete pod "$KILL_TARGET" --grace-period=0 --force >/dev/null 2>&1 || true
  kubectl -n "$NS" get pods -l app=b2bua-worker -o wide || true
}

# Read the UAC job's SIPp stats and assert the success rate clears the bar.
assert_survival() {
  log "waiting for the UAC job to finish (hold + BYE)"
  # Job completes when all calls finish (success or fail); give the 15s hold +
  # failover slack room.
  kubectl -n "$NS" wait --for=condition=complete "job/$JOB" --timeout=120s 2>/dev/null \
    || kubectl -n "$NS" wait --for=condition=failed "job/$JOB" --timeout=10s 2>/dev/null || true

  local upod stats ok_n fail_n total_n
  upod="$(kubectl -n "$NS" get pods -l app=sipp-uac --sort-by=.metadata.creationTimestamp -o jsonpath='{.items[-1:].metadata.name}')"
  [ -n "$upod" ] || fail "no UAC pod found"
  stats="$(kubectl -n "$NS" logs "$upod" 2>/dev/null || true)"
  ok_n="$(printf '%s' "$stats"   | grep -aE 'Successful call'     | tail -1 | grep -oE '[0-9]+' | tail -1)"
  fail_n="$(printf '%s' "$stats" | grep -aE 'Failed call'         | tail -1 | grep -oE '[0-9]+' | tail -1)"
  total_n="$(printf '%s' "$stats"| grep -aE 'Total Calls created' | tail -1 | grep -oE '[0-9]+' | tail -1)"
  ok_n="${ok_n:-0}"; fail_n="${fail_n:-0}"; total_n="${total_n:-0}"

  printf '\n  hold-failover result: total=%s successful=%s failed=%s\n' "$total_n" "$ok_n" "$fail_n" >&2
  [ "$total_n" -gt 0 ] || fail "no calls were created"
  local pct=$(( ok_n * 100 / total_n ))
  printf '  success rate = %s%% (threshold %s%%)\n\n' "$pct" "$PASS_THRESHOLD" >&2
  if [ "$pct" -ge "$PASS_THRESHOLD" ]; then
    ok "call survival under worker kill: ${pct}% >= ${PASS_THRESHOLD}%"
  else
    fail "call survival ${pct}% < ${PASS_THRESHOLD}% — dialogs did not fail over to the backup replica"
  fi
}

failover() {
  up
  deploy
  wait_ready
  launch_calls
  # Let the dialogs reach the hold state, then kill mid-hold.
  local settle=$(( CALLS / CPS + 4 ))
  log "letting dialogs establish (~${settle}s) before the kill"
  sleep "$settle"
  kill_worker
  assert_survival
  if [ "${KEEP:-0}" = "1" ]; then
    log "KEEP=1 — leaving cluster up (./chaos.sh down to tear down)"
  else
    down
  fi
}

cmd="${1:-failover}"; shift || true
case "$cmd" in
  failover) failover ;;
  up)       up ;;
  deploy)   deploy; wait_ready ;;
  kill)     kill_worker ;;
  assert)   assert_survival ;;
  down)     down ;;
  *) fail "usage: $0 {failover|up|deploy|kill|assert|down}" ;;
esac
