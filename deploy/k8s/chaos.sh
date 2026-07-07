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
#   ./chaos.sh bringback       # failover + restart the killed primary + prove it
#                              #   re-hydrates and serves a fresh batch (reclaim)
#   ./chaos.sh up              # just (re)create cluster + build/load images (repl)
#   ./chaos.sh deploy          # just deploy the repl-enabled stack
#   ./chaos.sh kill            # inject one worker kill against a running stack
#   ./chaos.sh recover         # wait the killed primary back + assert re-hydration
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
# >>> SOURCEABLE LIBRARY (issue 025) <<<
# This file doubles as a function library: all logic lives in functions and the
# subcommand dispatch is chaos_main(), executed ONLY when the script is run
# directly. A downstream overlay (living in a DIFFERENT directory) can set knobs,
# `source /path/to/deploy/k8s/chaos.sh` (executes nothing), then call/override
# the primitives (kill_worker, kill_proxy, peak, cpu_starve*, assert_survival,
# wait_brought_back, ...) or dispatch via `chaos_main kill`. The formerly-inline
# kill/assert kubectl targets are parameterized (defaults reproduce the
# historical behaviour exactly):
#   WORKER_SELECTOR / WORKER_STS / WORKER_CONTAINER     worker pods / workload / container
#   PROXY_SELECTOR / PROXY_DEPLOY / PROXY_VRRP_CONTAINER proxy pods / workload / VIP owner
#   UAC_SELECTOR / ORPHAN_SELECTOR / LIMITER_SELECTOR / LIMITER_DEPLOY
#   MANIFEST_DIR        directory the UAC job template is rendered from
# NOTE: the cluster-lifecycle helpers are namespaced chaos_up/chaos_deploy/
# chaos_down (they shell out to run.sh) so they never collide with run.sh's own
# up/deploy/down when an overlay sources BOTH files.
#
# Shares cluster name `sip-e2e` (WSL one-cluster switch) — see README/run.sh.
set -euo pipefail
# Resolve our own directory WITHOUT a top-level `cd` (a source-time cd would leak
# into any script sourcing this library — issue 025); every path below that used
# to be cwd-relative is now anchored on $K8S_DIR instead.
K8S_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NS="${NS:-sip-test}"
CALLS="${CALLS:-30}"
CPS="${CPS:-3}"
KILL_TARGET="${KILL_TARGET:-b2bua-worker-0}"
PASS_THRESHOLD="${PASS_THRESHOLD:-90}"
SCENARIO="uac-hold-failover.xml"
JOB="sipp-uac-failover"
MANIFEST_DIR="${MANIFEST_DIR:-$K8S_DIR/manifests}"

# Kill-target / assert knobs (issue 025): the label selectors, workload names and
# container names the chaos primitives act on — extracted from the formerly-
# inline kubectl calls so a downstream overlay can retarget them. KILL_TARGET/
# STARVE_TARGET pick the POD; these pick the POPULATION each primitive
# selects/waits on. Defaults reproduce the historical behaviour exactly.
WORKER_SELECTOR="${WORKER_SELECTOR:-app=b2bua-worker}"       # worker pod label
WORKER_STS="${WORKER_STS:-statefulset/b2bua-worker}"         # worker workload (rollout gate)
WORKER_CONTAINER="${WORKER_CONTAINER:-b2bua-worker}"         # container cpu_starve throttles
PROXY_SELECTOR="${PROXY_SELECTOR:-app=sip-front-proxy}"      # proxy pod label
PROXY_DEPLOY="${PROXY_DEPLOY:-deploy/sip-front-proxy}"       # proxy workload (rollout gate)
PROXY_VRRP_CONTAINER="${PROXY_VRRP_CONTAINER:-keepalived}"   # sidecar that owns the VIP
UAC_SELECTOR="${UAC_SELECTOR:-app=sipp-uac}"                 # UAC pods (assert_survival reads logs)
LIMITER_SELECTOR="${LIMITER_SELECTOR:-app=call-limiter}"     # limiter pod label
LIMITER_DEPLOY="${LIMITER_DEPLOY:-deploy/call-limiter}"      # limiter workload (rollout gate)
ORPHAN_SELECTOR="${ORPHAN_SELECTOR:-role=orphan}"            # orphan-stream pods (matches the
                                                             # ROLE label in the UAC job template)

# Replication ON, ≥2 workers so a primary + backup live on different app nodes.
export REPL_ENABLE=1
export WORKER_REPLICAS="${WORKER_REPLICAS:-2}"
export SCENARIO
# Front-proxy HA VIP + LB port (ADR-0012 D7): from the shared lib (subnet→VIP
# derivation, all three runners agree). UAC streams target PROXY_VIP, not the Service.
source "$K8S_DIR/lib/net-env.sh"
source "$K8S_DIR/lib/kube-env.sh"   # pin every kubectl to context kind-$CLUSTER
LIMITER_CAP="${LIMITER_CAP:-20}"
export LIMITER_CAP
# Pod-resource envsubst vars for the shared 40-sipp-uac-job template (envsubst has
# no default syntax, so EVERY render site must export them — endurance.sh sizes
# these per-role; the chaos/abuse/orphan/peak/failover streams here are transient
# and low-concurrency, so a modest default is fine).
# Request lowered 2 -> 1 (limit unchanged at 8): these chaos/abuse/orphan/peak
# streams are transient and low-concurrency, so a 1-core RESERVATION is plenty and
# the 2-core default added needless scheduling pressure on the 2-load-node cluster
# (issue1). The burst limit (8) is untouched, so a brief peak is still uncapped.
export UAC_CPU_REQ="${UAC_CPU_REQ:-1}" UAC_CPU_LIM="${UAC_CPU_LIM:-8}" \
       UAC_MEM_REQ="${UAC_MEM_REQ:-384Mi}" UAC_MEM_LIM="${UAC_MEM_LIM:-1536Mi}"

log()  { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
ok()   { printf '\033[1;32mPASS: %s\033[0m\n' "$*" >&2; }
warn() { printf '\033[1;33mWARN: %s\033[0m\n' "$*" >&2; }
fail() { printf '\033[1;31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }

# Reuse run.sh for the heavy lifting (image build, cluster, base deploy) so the
# two stay in lock-step on topology + image. run.sh reads REPL_ENABLE/SCENARIO
# from the environment we exported above. Invoked by absolute path (no cd — this
# file is sourceable) and namespaced chaos_* so these can never collide with
# run.sh's own up/deploy/down when an overlay sources both files.
chaos_up()     { REPL_ENABLE=1 "$K8S_DIR/run.sh" up; }
chaos_deploy() { REPL_ENABLE=1 "$K8S_DIR/run.sh" deploy; }
chaos_down()   { "$K8S_DIR/run.sh" down; }

# Wait until every worker reports Ready (re-hydrated + backup-current via the
# /ready probe the StatefulSet's readinessProbe consumes).
wait_ready() {
  log "waiting for all workers to be Ready (re-hydrated + backup-current)"
  kubectl -n "$NS" rollout status "$WORKER_STS" --timeout=120s
  kubectl -n "$NS" wait --for=condition=ready pod -l "$WORKER_SELECTOR" --timeout=120s
}

# Launch the hold-failover UAC job: CALLS dialogs at CPS, each INVITE/ACK/15s
# hold/BYE. SIPp exits 0 only if every call succeeded.
launch_calls() {
  export UAC_JOB_NAME="$JOB" CAPS="$CPS" MAX_CALLS="$CALLS" \
         MAX_CONCURRENT="${MAX_CONCURRENT:-$(( CPS * 600 ))}" ROLE="${ROLE:-failover}"
  kubectl -n "$NS" delete job "$JOB" --ignore-not-found >/dev/null 2>&1 || true
  log "launching $CALLS hold dialogs @ ${CPS}cps (scenario=$SCENARIO)"
  envsubst < "$MANIFEST_DIR/40-sipp-uac-job.yaml" | kubectl apply -f -
  kubectl -n "$NS" wait --for=condition=ready pod -l "$UAC_SELECTOR" --timeout=60s || true
}

# Inject ONE fault: delete the pod holding (statistically) a share of the live
# dialogs while they sit in hold. StatefulSet recreates it under a fresh
# incarnation; the surviving worker serves the backup replica so in-dialog BYEs
# still terminate cleanly.
kill_worker() {
  local grace="${KILL_GRACE:-0}"
  log "CHAOS: killing $KILL_TARGET mid-hold (grace=${grace}s)"
  # Capture the kill wall-clock (Unix epoch ms) at the instant of the delete and
  # publish it to KILL_TS_FILE (if set). The loadgen back-dates its chaos marker
  # to this timestamp, so the marker tracks the REAL kill instant rather than the
  # POST-receipt time — robust to any port-forward / plumbing latency on the flag
  # path. See loadgen_chaos_flag (endurance.sh) + ChaosLog::record_at.
  local kill_ms
  kill_ms="$(date +%s%3N)"
  [ -n "${KILL_TS_FILE:-}" ] && printf '%s' "$kill_ms" > "$KILL_TS_FILE" 2>/dev/null || true
  if [ "$grace" -gt 0 ]; then
    # Graceful: the worker drains (flushes its changelog to the backup +
    # self-503s via B2BUA_DRAIN_GRACE_MS) so in-dialog BYEs land on a hydrated
    # replica — models a rolling restart. grace=0 (default) is a true crash.
    kubectl -n "$NS" delete pod "$KILL_TARGET" --grace-period="$grace" >/dev/null 2>&1 || true
  else
    kubectl -n "$NS" delete pod "$KILL_TARGET" --grace-period=0 --force >/dev/null 2>&1 || true
  fi
  kubectl -n "$NS" get pods -l "$WORKER_SELECTOR" -o wide || true
}

# ---------------------------------------------------------------------------
# Extended chaos primitives (proxy kill, traffic peak, abuse stream) + a VM
# metric push so every chaos event lands in Grafana. Used standalone and by the
# endurance orchestrator (endurance.sh).
# ---------------------------------------------------------------------------

# VictoriaMetrics Prometheus import endpoint (host stack). Each chaos event is
# pushed as an instant sample so the dashboard can mark + count it.
VM_IMPORT="${VM_IMPORT:-http://127.0.0.1:8428/api/v1/import/prometheus}"
push_metric() {
  # $1 = one or more newline-separated exposition lines.
  curl -s --max-time 4 -X POST "$VM_IMPORT" --data-binary "$1" >/dev/null 2>&1 || true
}

# Find the proxy pod that currently OWNS the VIP (the VRRP master): the master
# carries ${PROXY_VIP} on eth0, the backup only on lo. Echoes the pod name (empty
# if none found).
proxy_master_pod() {
  local p
  for p in $(kubectl -n "$NS" get pods -l "$PROXY_SELECTOR" -o jsonpath='{.items[*].metadata.name}' 2>/dev/null); do
    if kubectl -n "$NS" exec "$p" -c "$PROXY_VRRP_CONTAINER" -- ip -4 addr show dev eth0 2>/dev/null | grep -q "${PROXY_VIP}/"; then
      echo "$p"; return 0
    fi
  done
  return 0
}

# CHAOS: kill the VIP MASTER proxy (ADR-0012 D7 HA). The backup is warm and claims
# the VIP via VRRP in <2s; because the advertised address is the stable VIP, in-
# dialog BYE/keepalives and new calls keep flowing through the survivor. The
# Deployment then restores the killed pod as a fresh backup. Killing ONLY the
# master (not `-l app=...`, which would take down both replicas) is the real
# failover test.
kill_proxy() {
  local master
  master="$(proxy_master_pod)"
  [ -n "$master" ] || master="$(kubectl -n "$NS" get pods -l "$PROXY_SELECTOR" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)"
  log "CHAOS: killing VIP-master proxy ${master:-<none>} (backup takes over the VIP)"
  push_metric 'sip_chaos_event{type="kill_proxy",phase="start"} 1'
  [ -n "$master" ] && kubectl -n "$NS" delete pod "$master" --grace-period=0 --force >/dev/null 2>&1 || true
  log "waiting for the proxy Deployment to restore 2 Ready replicas"
  kubectl -n "$NS" rollout status "$PROXY_DEPLOY" --timeout=120s || true
  kubectl -n "$NS" get pods -l "$PROXY_SELECTOR" -o wide || true
  push_metric 'sip_chaos_event{type="kill_proxy",result="pass"} 1'
}

# CHAOS: a short, sharp ${PEAK_CAPS}cps burst of short calls on top of the
# baseline — the traffic-peak event. Launches a fire-and-forget UAC job and
# deletes it after PEAK_SECS.
PEAK_CAPS="${PEAK_CAPS:-200}"
PEAK_SECS="${PEAK_SECS:-30}"
# A peak is a flood of NEW calls; for the overload experiment those should be
# NON-emergency (sheddable) so a concurrent cpu_starve can actually 503 them —
# emergency bursts (Resource-Priority esnet.0) are force-admitted and would just
# add load without exercising the shed. Default to the non-emergency short
# scenario; override PEAK_SCENARIO=uac-endurance-short.xml for an emergency flood.
PEAK_SCENARIO="${PEAK_SCENARIO:-uac-endurance-short-noemerg.xml}"
peak() {
  local job="sipp-uac-peak"
  log "CHAOS: traffic peak ${PEAK_CAPS}cps for ${PEAK_SECS}s (scenario=$PEAK_SCENARIO)"
  push_metric "sip_chaos_event{type=\"peak\",phase=\"start\"} 1
sip_chaos_active{type=\"peak\"} 1"
  export UAC_JOB_NAME="$job" CAPS="$PEAK_CAPS" \
         MAX_CALLS=$(( PEAK_CAPS * (PEAK_SECS + 10) )) \
         MAX_CONCURRENT="${MAX_CONCURRENT:-$(( PEAK_CAPS * 600 ))}" \
         SCENARIO="$PEAK_SCENARIO" ROLE="peak"
  kubectl -n "$NS" delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
  envsubst < "$MANIFEST_DIR/40-sipp-uac-job.yaml" | kubectl apply -f -
  sleep "$PEAK_SECS"
  kubectl -n "$NS" delete job "$job" --ignore-not-found >/dev/null 2>&1 || true
  push_metric "sip_chaos_event{type=\"peak\",result=\"pass\"} 1
sip_chaos_active{type=\"peak\"} 0"
}

# CHAOS: cpu_starve — make ONE b2bua worker pod overloaded by SHRINKING the CPU
# available to its container (NOT by piling on traffic). This is the faithful
# "the platform itself is overloaded" lever: the old `peak` event just added
# 200cps of NEW INVITEs, which saturated the SIPp generators long before it
# stressed the platform, so the SUT's overload tiers never actually engaged. CPU
# scarcity DOES engage them — the worker's ELU sampler (a 100ms tokio interval
# whose reading is its own scheduling lag, crates/b2bua/src/overload.rs) lands
# chronically late under a tight CPU quota, the published `elu` EWMA climbs past
# B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD (0.75), and the Tier-3 admission gate
# (overload.should_admit) starts returning the stateless `503 overload` for NEW
# NON-emergency INVITEs while ALWAYS admitting emergency ones (Resource-Priority
# esnet.0) and NEVER touching established in-dialog requests (re-INVITE/BYE/in-
# dialog OPTIONS are not gated). That is exactly the trio the experiment asserts:
#   (1) in-dialog calls unaffected, (2) new calls MAY be rejected, (3) but only
#   non-emergency ones.
#
# Mechanism (cgroup v2, reversible, NO pod restart so no reclaim/failover noise):
# resolve the target pod's node (a kind docker container) + its b2bua container
# cgroup, write a tight `cpu.max` quota onto that container's cgroup for
# STARVE_SECS, then restore the prior value (default "max" = unlimited; the
# worker manifest sets no CPU limit). Driven from the host via `docker exec` into
# the kind node — that is where /sys/fs/cgroup is writable as root; the container
# itself mounts it read-only. Requires the kind provider (docker) on the host.
#
# Knobs (defaults): STARVE_TARGET=b2bua-worker-0  STARVE_SECS=60
#   STARVE_QUOTA_US=40000  STARVE_PERIOD_US=100000   (40000/100000 = 0.4 CPU)
# 0.4 CPU is the starting guess: enough sustained backlog (the worker wants
# ~1-2 cores under the steady endurance load) to peg ELU high and trip the gate,
# while still letting the cheap /ready GET answer within its ~4s failure budget
# so the pod stays IN the LB (a pod that drops out reroutes new calls to the
# healthy worker and the gate is never exercised). EXPECT TO CALIBRATE on the
# first live run — too tight drops /ready (looks like a kill, not an overload);
# too loose never crosses 0.75 ELU. Watch b2bua_overload_rejected_total rise and
# the worker stay Ready; nudge STARVE_QUOTA_US until both hold.
# Defaults CALIBRATED 2026-06-20 on the default endurance profile (long@5,
# short 50em+50ne, reinvite@5, limiter@2; 2 workers on 24-core nodes). A worker
# draws ~0.22 core at that baseline and ~0.33 under the +120cps non-emergency
# peak the `overload` combo adds. 0.30 core (30000/100000) is JUST BELOW that
# demand: ELU crosses 0.75 → the gate 503s the non-emergency EXCESS while the
# worker keeps serving emergency + in-dialog (measured: ~42 non-emergency shed,
# ~0 emergency loss, 0 in-dialog loss, worker stays Ready). LOWER caps over-starve
# it (0.20 → ~3.6% emergency collateral; 0.08 → worker can't serve anything,
# emergency + in-dialog both fail) — a fixed CPU cap is not relieved by shedding,
# so too-tight starves emergency too. RE-CALIBRATE if the baseline load, worker
# count, or node size changes (it is demand-relative, not absolute).
STARVE_TARGET="${STARVE_TARGET:-b2bua-worker-0}"
STARVE_SECS="${STARVE_SECS:-90}"
STARVE_QUOTA_US="${STARVE_QUOTA_US:-30000}"
STARVE_PERIOD_US="${STARVE_PERIOD_US:-100000}"

# Echo "NODE CGROUP_DIR" for $STARVE_TARGET's b2bua container, or nothing on miss.
# Resolves pod→node (node name == kind docker container name) and the container
# id→cgroup scope dir under the node's /sys/fs/cgroup (driver-agnostic find on the
# container id, so it works for both the systemd and cgroupfs cgroup drivers).
_starve_locate() {
  local pod="$1" node cid dir
  node="$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.spec.nodeName}' 2>/dev/null)"
  [ -n "$node" ] || { warn "cpu_starve: pod $pod has no node (not scheduled?)"; return 1; }
  cid="$(kubectl -n "$NS" get pod "$pod" \
        -o jsonpath="{.status.containerStatuses[?(@.name==\"$WORKER_CONTAINER\")].containerID}" 2>/dev/null)"
  cid="${cid##*/}"   # strip the containerd://… scheme
  [ -n "$cid" ] || { warn "cpu_starve: no $WORKER_CONTAINER container id for $pod"; return 1; }
  # The leaf cgroup is the container's own scope (…<cid>.scope or …<cid>); pick the
  # one that actually has a cpu.max knob.
  dir="$(docker exec "$node" sh -c "find /sys/fs/cgroup -type d -name '*${cid}*' 2>/dev/null \
        | while read -r d; do [ -f \"\$d/cpu.max\" ] && { echo \"\$d\"; break; }; done")"
  [ -n "$dir" ] || { warn "cpu_starve: no cpu.max cgroup for container $cid on node $node"; return 1; }
  echo "$node $dir"
}

# Throttle ONE pod's b2bua container cgroup to STARVE_QUOTA_US/STARVE_PERIOD_US.
# Echoes a "node|dir|prev" restore tuple on success (nothing on failure) so the
# caller can restore it later — this is what lets cpu_starve_all throttle every
# worker, hold them ALL down for one window, then restore them together.
_starve_apply() {  # $1=pod
  local pod="$1" loc node dir prev
  loc="$(_starve_locate "$pod")" || return 1
  node="${loc%% *}"; dir="${loc#* }"
  prev="$(docker exec "$node" cat "$dir/cpu.max" 2>/dev/null || echo max)"
  if ! docker exec "$node" sh -c "echo '${STARVE_QUOTA_US} ${STARVE_PERIOD_US}' > '$dir/cpu.max'"; then
    warn "cpu_starve: failed to write cpu.max for $pod (cgroup v2 + writable node fs required)"
    return 1
  fi
  log "cpu_starve: $pod throttled ${STARVE_QUOTA_US}/${STARVE_PERIOD_US} (cgroup=$dir node=$node, was '$prev')"
  echo "$node|$dir|$prev"
}
# Restore one "node|dir|prev" tuple (idempotent; falls back to 'max' = unlimited).
# NOTE: the parses MUST be on separate statements, not one `local a=.. b=${a}`
# line — under `set -u` the shell expands every RHS while building the `local`
# arg list, BEFORE local assigns anything, so a same-line back-reference reads as
# unbound and aborts the restore (which once left both workers stuck throttled).
_starve_restore() {  # $1="node|dir|prev"
  local tuple="$1" node dir prev
  node="${tuple%%|*}"; tuple="${tuple#*|}"
  dir="${tuple%%|*}"; prev="${tuple#*|}"
  docker exec "$node" sh -c "echo '$prev' > '$dir/cpu.max'" >/dev/null 2>&1 \
    || docker exec "$node" sh -c "echo max > '$dir/cpu.max'" >/dev/null 2>&1 || true
  log "cpu_starve: restored $dir cpu.max='$prev' on $node"
}

cpu_starve() {
  local pod="$STARVE_TARGET" tup
  log "CHAOS: cpu_starve — throttle $pod to ${STARVE_QUOTA_US}/${STARVE_PERIOD_US} CPU for ${STARVE_SECS}s (overload via CPU scarcity, no restart)"
  tup="$(_starve_apply "$pod")" || { warn "cpu_starve: could not throttle $pod — skipping"; return 1; }
  push_metric "sip_chaos_event{type=\"cpu_starve\",phase=\"start\"} 1
sip_chaos_active{type=\"cpu_starve\",pod=\"$pod\"} 1"
  sleep "$STARVE_SECS"
  _starve_restore "$tup"
  push_metric "sip_chaos_event{type=\"cpu_starve\",result=\"pass\"} 1
sip_chaos_active{type=\"cpu_starve\",pod=\"$pod\"} 0"
}

# CHAOS: cpu_starve_all — throttle EVERY b2bua worker to the same quota at once.
# The WORST case for emergency protection: with a single worker starved (cpu_starve)
# the healthy peer absorbs the LB-shed/re-routed traffic, so an emergency call
# always has a clean worker to land on. Starving ALL workers removes that escape
# hatch — every emergency call now lands on a starved, overloaded worker. This
# verifies the SUT still admits + serves emergency (≈0 impact) and still sheds only
# NON-emergency, even when the whole platform is overloaded with nowhere to fail
# over. Same reversible cgroup mechanism, applied to all workers for ONE window.
cpu_starve_all() {
  local pods p tup tups=()
  pods="$(kubectl -n "$NS" get pods -l "$WORKER_SELECTOR" -o jsonpath='{.items[*].metadata.name}' 2>/dev/null)"
  [ -n "$pods" ] || { warn "cpu_starve_all: no b2bua-worker pods found"; return 1; }
  log "CHAOS: cpu_starve_all — throttle ALL workers [$pods] to ${STARVE_QUOTA_US}/${STARVE_PERIOD_US} for ${STARVE_SECS}s (WORST case: no healthy peer)"
  push_metric "sip_chaos_event{type=\"cpu_starve_all\",phase=\"start\"} 1
sip_chaos_active{type=\"cpu_starve_all\"} 1"
  for p in $pods; do
    if tup="$(_starve_apply "$p")"; then tups+=("$tup"); else warn "cpu_starve_all: could not throttle $p (continuing)"; fi
  done
  if [ "${#tups[@]}" -eq 0 ]; then
    warn "cpu_starve_all: no workers throttled — skipping"
    push_metric "sip_chaos_active{type=\"cpu_starve_all\"} 0"; return 1
  fi
  # SAFETY: restore every throttled worker even if the hold is interrupted
  # (Ctrl-C / kill) — a stranded cpu.max quota silently degrades the cluster.
  _STARVE_TUPS=("${tups[@]}")
  trap '_t="$?"; for __t in "${_STARVE_TUPS[@]:-}"; do [ -n "$__t" ] && _starve_restore "$__t"; done; trap - INT TERM EXIT; exit "$_t"' INT TERM
  log "cpu_starve_all: ${#tups[@]} worker(s) throttled; holding ${STARVE_SECS}s"
  sleep "$STARVE_SECS"
  for tup in "${tups[@]}"; do _starve_restore "$tup"; done
  trap - INT TERM
  log "cpu_starve_all: restored ${#tups[@]} worker(s)"
  push_metric "sip_chaos_event{type=\"cpu_starve_all\",result=\"pass\"} 1
sip_chaos_active{type=\"cpu_starve_all\"} 0"
}

# CONVENIENCE: run a cpu_starve AND a concurrent mini traffic peak, so the worker
# is both CPU-scarce and seeing extra new-call pressure — the "starve + mini
# peek, mix proportions" experiment. PEAK_CAPS sizes the mini peak (keep it small,
# e.g. 20-50, since the point is platform overload, not generator overload);
# STARVE_* size the throttle. Both run for STARVE_SECS.
overload_mix() {
  local peak_secs="${PEAK_SECS:-$STARVE_SECS}"
  log "CHAOS: overload_mix — cpu_starve($STARVE_TARGET) + mini peak ${PEAK_CAPS}cps, concurrently"
  PEAK_SECS="$peak_secs" peak &
  local peak_pid=$!
  cpu_starve
  wait "$peak_pid" 2>/dev/null || true
}

# overload_all = cpu_starve_all + concurrent non-emergency peak. The WORST-case
# overload experiment: EVERY worker CPU-scarce AND extra new-call pressure, with
# no healthy peer to absorb anything. Asserts emergency stays ≈0-impact platform-wide.
overload_all() {
  local peak_secs="${PEAK_SECS:-$STARVE_SECS}"
  log "CHAOS: overload_all — cpu_starve_all + peak ${PEAK_CAPS}cps (every worker starved, no failover headroom)"
  PEAK_SECS="$peak_secs" peak &
  local peak_pid=$!
  cpu_starve_all
  wait "$peak_pid" 2>/dev/null || true
}

# Background ABUSE stream at ${ABUSE_CAPS}cps (default 1). Long-lived job running
# an abuse archetype (in-dialog OPTIONS flood by default) for the whole window.
ABUSE_CAPS="${ABUSE_CAPS:-1}"
ABUSE_SCENARIO="${ABUSE_SCENARIO:-uac-abuse-options-flood.xml}"
ABUSE_JOB="sipp-uac-abuse"
abuse_up() {
  log "ABUSE: starting ${ABUSE_CAPS}cps stream (${ABUSE_SCENARIO})"
  export UAC_JOB_NAME="$ABUSE_JOB" CAPS="$ABUSE_CAPS" \
         MAX_CALLS="${ABUSE_MAX_CALLS:-1000000}" \
         MAX_CONCURRENT="${MAX_CONCURRENT:-10000}" \
         SCENARIO="$ABUSE_SCENARIO" ROLE="abuse"
  kubectl -n "$NS" delete job "$ABUSE_JOB" --ignore-not-found >/dev/null 2>&1 || true
  envsubst < "$MANIFEST_DIR/40-sipp-uac-job.yaml" | kubectl apply -f -
  push_metric 'sip_chaos_active{type="abuse"} 1'
}
abuse_down() {
  log "ABUSE: stopping stream"
  kubectl -n "$NS" delete job "$ABUSE_JOB" --ignore-not-found >/dev/null 2>&1 || true
  push_metric 'sip_chaos_active{type="abuse"} 0'
}

# CHAOS: launch a DEDICATED ${ORPHAN_CAPS}cps stream, let calls establish, then
# ABRUPTLY kill the UAC mid-call (--grace-period=0, no BYE). Every in-flight
# dialog is orphaned on the B2BUA — the exact condition that leaked calls
# forever before the terminating-safety-timeout reaper fix. The worker must reap
# them via keepalive timeout (in-dialog OPTIONS get no answer) within ~a minute.
# endurance.sh measures b2bua_active_calls before/after to detect a regression.
ORPHAN_CAPS="${ORPHAN_CAPS:-50}"
ORPHAN_BUILD_SECS="${ORPHAN_BUILD_SECS:-20}"
ORPHAN_JOB="sipp-uac-orphan"
orphan_kill() {
  log "CHAOS: orphan_kill — ${ORPHAN_CAPS}cps for ${ORPHAN_BUILD_SECS}s then abrupt UAC kill (no BYE)"
  push_metric 'sip_chaos_event{type="orphan_kill",phase="start"} 1'
  export UAC_JOB_NAME="$ORPHAN_JOB" CAPS="$ORPHAN_CAPS" \
         MAX_CALLS=$(( ORPHAN_CAPS * (ORPHAN_BUILD_SECS + 120) )) \
         MAX_CONCURRENT="${MAX_CONCURRENT:-$(( ORPHAN_CAPS * 600 ))}" \
         SCENARIO="uac-endurance-short.xml" ROLE="orphan"
  kubectl -n "$NS" delete job "$ORPHAN_JOB" --ignore-not-found >/dev/null 2>&1 || true
  envsubst < "$MANIFEST_DIR/40-sipp-uac-job.yaml" | kubectl apply -f - >/dev/null
  kubectl -n "$NS" wait --for=condition=ready pod -l "$ORPHAN_SELECTOR" --timeout=40s >/dev/null 2>&1 || true
  sleep "$ORPHAN_BUILD_SECS"   # let ~ORPHAN_CAPS*ORPHAN_BUILD_SECS dialogs establish
  log "CHAOS: abruptly killing the orphan UAC mid-call (dialogs orphaned on the B2BUA)"
  kubectl -n "$NS" delete pod -l "$ORPHAN_SELECTOR" --grace-period=0 --force >/dev/null 2>&1 || true
  kubectl -n "$NS" delete job "$ORPHAN_JOB" --grace-period=0 --force --ignore-not-found >/dev/null 2>&1 || true
  push_metric 'sip_chaos_event{type="orphan_kill",phase="killed"} 1'
}

# CHAOS: kill the (single-replica) shared call-limiter. It is a SPOF for the
# limiter FUNCTION only: while it is down the b2bua fails OPEN (admits with no
# holds, 150ms budget), so calls keep flowing — the cap simply stops being
# enforced. The Deployment (strategy: Recreate) brings a fresh, empty pod back;
# active calls' refresh timers re-populate its counters within ~LIMITER_WINDOW.
limiter_kill() {
  log "CHAOS: killing the shared call-limiter pod (b2bua fails open while it's down)"
  push_metric 'sip_chaos_event{type="limiter_kill",phase="start"} 1'
  kubectl -n "$NS" delete pod -l "$LIMITER_SELECTOR" --grace-period=0 --force >/dev/null 2>&1 || true
  log "waiting for call-limiter to come back Ready"
  kubectl -n "$NS" rollout status "$LIMITER_DEPLOY" --timeout=90s || true
  kubectl -n "$NS" get pods -l "$LIMITER_SELECTOR" -o wide || true
  push_metric 'sip_chaos_event{type="limiter_kill",result="pass"} 1'
}

# CHAOS: a NETWORK interruption to the shared call-limiter WITHOUT killing it.
# `tc netem loss 100%` on the limiter pod's eth0 black-holes all traffic for
# NETCUT_SECS, so worker->limiter admits/releases/refreshes time out (150ms
# budget) and the b2bua fails open — same observable effect as a kill but the
# pod (and its in-memory counters) stay intact, so on restore the counters are
# still warm. Requires NET_ADMIN + iproute2 (set on 50-call-limiter / image).
NETCUT_SECS="${NETCUT_SECS:-60}"
limiter_netcut() {
  local pod
  pod="$(kubectl -n "$NS" get pod -l "$LIMITER_SELECTOR" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)"
  if [ -z "$pod" ]; then log "limiter_netcut: no call-limiter pod found"; return; fi
  log "CHAOS: limiter_netcut — 100% packet loss on $pod eth0 for ${NETCUT_SECS}s (pod stays up)"
  push_metric 'sip_chaos_event{type="limiter_netcut",phase="start"} 1
sip_chaos_active{type="limiter_netcut"} 1'
  kubectl -n "$NS" exec "$pod" -- tc qdisc add dev eth0 root netem loss 100% >/dev/null 2>&1 \
    || warn "limiter_netcut: tc add failed (NET_ADMIN/iproute2 present?)"
  sleep "$NETCUT_SECS"
  kubectl -n "$NS" exec "$pod" -- tc qdisc del dev eth0 root >/dev/null 2>&1 || true
  log "limiter_netcut: removed netem on $pod (connectivity restored)"
  push_metric 'sip_chaos_event{type="limiter_netcut",result="pass"} 1
sip_chaos_active{type="limiter_netcut"} 0'
}

# Read the UAC job's SIPp stats and assert the success rate clears the bar.
assert_survival() {
  log "waiting for the UAC job to finish (hold + BYE)"
  # Job completes when all calls finish (success or fail); give the 15s hold +
  # failover slack room.
  kubectl -n "$NS" wait --for=condition=complete "job/$JOB" --timeout=120s 2>/dev/null \
    || kubectl -n "$NS" wait --for=condition=failed "job/$JOB" --timeout=10s 2>/dev/null || true

  local upod stats ok_n fail_n total_n
  upod="$(kubectl -n "$NS" get pods -l "$UAC_SELECTOR" --sort-by=.metadata.creationTimestamp -o jsonpath='{.items[-1:].metadata.name}')"
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
  chaos_up
  chaos_deploy
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
    chaos_down
  fi
}

# Wait for the StatefulSet to RE-CREATE the killed pod and for it to re-hydrate
# to Ready. The /ready probe only flips 200 once the fresh worker has bootstrap
# re-pulled from its peer (reclaim its pri partition) and gone backup-current, so
# "Ready again" is the bring-back gate: if re-hydration is broken the pod never
# becomes Ready and this times out.
wait_brought_back() {
  log "waiting for $KILL_TARGET to be re-created + re-hydrate to Ready (bring-back)"
  # The old pod is terminating; give the StatefulSet a moment to spawn the
  # replacement under the same name before we wait on it.
  sleep 5
  kubectl -n "$NS" wait --for=condition=ready "pod/$KILL_TARGET" --timeout=150s \
    || fail "bring-back: $KILL_TARGET did not re-hydrate to Ready after restart"
  kubectl -n "$NS" get pod "$KILL_TARGET" -o wide || true
}

# Assert the brought-back worker actually RE-PULLED state from its peer (not just
# came up empty): its repl_pull_applied counter must be > 0. A fresh worker that
# reclaims its calls on reboot drains the peer's compacted changelog — zero here
# means re-hydration silently delivered nothing (the goal-3 failure mode).
assert_rehydrated() {
  log "asserting $KILL_TARGET re-pulled state from its peer (repl_pull_applied > 0)"
  local pf applied
  kubectl -n "$NS" port-forward "$KILL_TARGET" 19091:9091 >/dev/null 2>&1 &
  pf=$!
  sleep 3
  applied="$(curl -s --max-time 4 localhost:19091/metrics 2>/dev/null \
    | grep -aE '^b2bua_repl_pull_applied_total ' | grep -oE '[0-9]+$' | tail -1)"
  kill "$pf" 2>/dev/null || true
  applied="${applied:-0}"
  printf '  %s repl_pull_applied_total = %s\n' "$KILL_TARGET" "$applied" >&2
  if [ "$applied" -gt 0 ]; then
    ok "bring-back re-hydration: $KILL_TARGET re-pulled $applied entries from its peer"
  else
    fail "bring-back: $KILL_TARGET re-pulled 0 entries — re-hydration delivered nothing"
  fi
}

# Bring-back / reclaim acceptance: shut a primary mid-hold (its dialogs fail over
# to the backup), let the StatefulSet restart it, prove it re-hydrates, then place
# a SECOND batch of dialogs on the recovered topology and assert THEY survive too
# — i.e. the brought-back worker serves traffic again, not just boots.
bringback() {
  chaos_up
  chaos_deploy
  wait_ready
  launch_calls
  local settle=$(( CALLS / CPS + 4 ))
  log "letting batch-1 dialogs establish (~${settle}s) before the kill"
  sleep "$settle"
  kill_worker
  # (1) batch-1 survived the kill (failover to the backup replica).
  assert_survival
  # (2) the killed primary comes back + re-hydrates.
  wait_brought_back
  assert_rehydrated
  # (3) the recreated pod has a fresh IP, but the proxy now discovers workers from
  # k8s EndpointSlices (ADR-0012 D4): the informer picks up the new IP on its own,
  # so NO proxy redeploy is needed (this used to re-bake PROXY_WORKERS). Just
  # re-gate readiness before driving traffic again.
  wait_ready
  # (4) batch-2: NEW dialogs on the recovered topology must all succeed.
  JOB="sipp-uac-bringback"
  launch_calls
  log "letting batch-2 dialogs run to completion (no kill — pure serve)"
  assert_survival
  ok "bring-back: recovered topology served a fresh batch after primary restart"
  if [ "${KEEP:-0}" = "1" ]; then
    log "KEEP=1 — leaving cluster up (./chaos.sh down to tear down)"
  else
    chaos_down
  fi
}

# Subcommand dispatch — the surface every existing caller (endurance.sh, docs,
# direct CLI use) depends on: names and behaviour are FROZEN (issue 025).
chaos_main() {
  local cmd="${1:-failover}"; shift || true
  case "$cmd" in
    failover)  failover ;;
    bringback) bringback ;;
    up)        chaos_up ;;
    deploy)    chaos_deploy; wait_ready ;;
    kill)      kill_worker ;;
    proxykill) kill_proxy ;;
    peak)      peak ;;
    cpustarve) cpu_starve ;;
    cpustarveall) cpu_starve_all ;;
    overload)  overload_mix ;;
    overloadall) overload_all ;;
    orphankill) orphan_kill ;;
    limiterkill) limiter_kill ;;
    limiternetcut) limiter_netcut ;;
    abuse)     case "${1:-up}" in up) abuse_up ;; down) abuse_down ;; *) fail "usage: $0 abuse {up|down}" ;; esac ;;
    recover)   wait_brought_back; assert_rehydrated ;;
    assert)    assert_survival ;;
    down)      chaos_down ;;
    *) fail "usage: $0 {failover|bringback|up|deploy|kill|proxykill|peak|cpustarve|cpustarveall|overload|overloadall|orphankill|limiterkill|limiternetcut|abuse {up|down}|recover|assert|down}" ;;
  esac
}

# Dispatch ONLY when executed directly; `source chaos.sh` defines + defaults and
# runs nothing. (The if-form — not `[[ ... ]] && chaos_main` — so that sourcing
# returns 0 and cannot trip a `set -e` caller.)
if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  chaos_main "$@"
fi
