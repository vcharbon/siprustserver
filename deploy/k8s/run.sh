#!/usr/bin/env bash
# Thin k8s endurance/load runner for the containerized Rust SIP SUT.
#
#   sipp UAC (tier=load) -> sip-front-proxy (tier=edge, LB+HMAC stickiness)
#                        -> b2bua-worker pool (tier=app) -> sipp UAS (tier=load)
#
# Deliberately minimal: deploy-all, run SIPp at a list of CAPS, sample per-pod
# CPU% + RSS from /proc, tear down. As of S11 the cluster topology
# (./cluster.yaml), the SIPp build context (./sipp/Dockerfile) and the SIPp
# scenarios (./sipp/scenarios/) are VENDORED COPIES (no longer symlinks into the
# sibling sipjsserver checkout) so the Rust SUT runner stands alone and the two
# can diverge — especially the endurance/chaos scenarios. Reuses the SAME kind
# cluster name (`sip-e2e`).
#
# >>> WSL ONE-CLUSTER CONSTRAINT <<<
# Only one kind cluster runs at a time on this host. `up` (and `all`) FIRST run
# `kind delete cluster --name sip-e2e` — which destroys ANY existing sip-e2e
# cluster, including sipjsserver's. That is the intended "stop the other, run
# this" switch. Run `down` when done so the host is free for the other SUT.
#
# Usage:
#   ./run.sh up                      # (re)create cluster + build/load images
#   ./run.sh deploy                  # apply uas + workers + proxy, wait ready
#   ./run.sh caps 200 30             # 200 cps for 30s, sample CPU/mem
#   ./run.sh sweep 30 50 100 200 400 # run a list of caps, 30s sampling each
#   ./run.sh all 30 50 100 200 400   # up + deploy + sweep (leaves cluster up)
#   ./run.sh down                    # delete the cluster
#
# Env: SUT_IMAGE=siprustserver:dev  WORKER_REPLICAS=2  CLUSTER=sip-e2e  NS=sip-test
set -euo pipefail
cd "$(dirname "$0")"
HERE="$(pwd)"
REPO_ROOT="$(cd ../.. && pwd)"

CLUSTER="${CLUSTER:-sip-e2e}"
NS="${NS:-sip-test}"
SUT_IMAGE="${SUT_IMAGE:-siprustserver:dev}"
WORKER_REPLICAS="${WORKER_REPLICAS:-2}"
RESULTS="${RESULTS:-$HERE/results}"
SIPP_DIR="$HERE/sipp"          # vendored sipp build context + scenarios
SCENARIOS="$SIPP_DIR/scenarios"
# Replication: off by default for the plain load/endurance sweep (the chaos
# suite — chaos.sh — sets REPL_ENABLE=1). REPL_PORT is the cluster-wide repl TCP
# port (peer.host:REPL_PORT), templated into the worker manifest + headless svc.
REPL_ENABLE="${REPL_ENABLE:-0}"
REPL_PORT="${REPL_PORT:-9092}"
SCENARIO="${SCENARIO:-uac-basic.xml}"   # UAC scenario the load sweep drives
export SUT_IMAGE WORKER_REPLICAS REPL_ENABLE REPL_PORT SCENARIO

log() { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
die() { printf '\033[1;31m!! %s\033[0m\n' "$*" >&2; exit 1; }

preflight() {
  for t in kind kubectl docker envsubst; do command -v "$t" >/dev/null || die "missing tool: $t"; done
  [ -f cluster.yaml ]          || die "cluster.yaml missing"
  [ -f "$SIPP_DIR/Dockerfile" ] || die "sipp/Dockerfile missing (vendored sipp build context)"
  [ -d "$SCENARIOS" ]          || die "sipp/scenarios/ missing (vendored sipp scenarios)"
}

up() {
  preflight
  log "tearing down any existing '$CLUSTER' cluster (WSL one-cluster switch)"
  kind delete cluster --name "$CLUSTER" 2>/dev/null || true
  log "creating cluster '$CLUSTER' from shared topology"
  kind create cluster --name "$CLUSTER" --config cluster.yaml --wait 120s

  log "building SUT image $SUT_IMAGE"
  docker build -f "$REPO_ROOT/deploy/docker/Dockerfile" -t "$SUT_IMAGE" "$REPO_ROOT"
  log "building sipp:dev from vendored context ($SIPP_DIR)"
  docker build -t sipp:dev "$SIPP_DIR"
  log "loading images into kind"
  kind load docker-image "$SUT_IMAGE" --name "$CLUSTER"
  kind load docker-image sipp:dev --name "$CLUSTER"
}

deploy() {
  preflight
  kubectl apply -f manifests/00-namespace.yaml
  log "building sipp-scenarios ConfigMap from vendored scenarios"
  kubectl -n "$NS" create configmap sipp-scenarios \
    --from-file="$SCENARIOS/" -o yaml --dry-run=client | kubectl apply -f -

  log "deploying sipp-uas + b2bua workers (repl=${REPL_ENABLE}, repl_port=${REPL_PORT})"
  kubectl apply -f manifests/10-sipp-uas.yaml
  # RBAC for the worker's EndpointSlice informer (needed when REPL_ENABLE=1; a
  # harmless ServiceAccount/Role otherwise).
  kubectl apply -f manifests/15-worker-rbac.yaml
  envsubst < manifests/20-worker.yaml | kubectl apply -f -
  kubectl -n "$NS" rollout status deploy/sipp-uas --timeout=120s
  kubectl -n "$NS" rollout status statefulset/b2bua-worker --timeout=120s

  # Resolve worker pod IPs -> proxy's static registry (IP literals required).
  log "resolving worker pod IPs for the proxy registry"
  local ips entries i=0
  ips="$(kubectl -n "$NS" get pods -l app=b2bua-worker \
         -o jsonpath='{range .items[*]}{.metadata.name} {.status.podIP}{"\n"}{end}' | sort)"
  entries=""
  while read -r name ip; do
    [ -z "$ip" ] && continue
    entries="${entries:+$entries,}w${i}@${ip}:5060"
    i=$((i+1))
  done <<< "$ips"
  [ -n "$entries" ] || die "no worker pod IPs resolved"
  export PROXY_WORKERS="$entries"
  log "PROXY_WORKERS=$PROXY_WORKERS"

  log "deploying sip-front-proxy"
  envsubst < manifests/30-proxy.yaml | kubectl apply -f -
  kubectl -n "$NS" rollout status deploy/sip-front-proxy --timeout=120s
  log "stack ready"
  kubectl -n "$NS" get pods -o wide
}

# sample_pod <label-selector> <window-seconds> -> prints "pod cpu% rss_mb" lines
sample_pods() {
  local sel="$1" window="$2"
  local tck pods
  pods="$(kubectl -n "$NS" get pods -l "$sel" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')"
  for pod in $pods; do
    tck="$(kubectl -n "$NS" exec "$pod" -- getconf CLK_TCK 2>/dev/null || echo 100)"
    local s0 u0 st0 s1 u1 st1 rss
    read -r u0 st0 <<< "$(kubectl -n "$NS" exec "$pod" -- cat /proc/1/stat 2>/dev/null | awk '{print $14, $15}')"
    sleep "$window"
    read -r u1 st1 <<< "$(kubectl -n "$NS" exec "$pod" -- cat /proc/1/stat 2>/dev/null | awk '{print $14, $15}')"
    rss="$(kubectl -n "$NS" exec "$pod" -- awk '/VmRSS/{print $2}' /proc/1/status 2>/dev/null)"
    local djiff cpu rss_mb
    djiff=$(( (u1+st1) - (u0+st0) ))
    cpu=$(awk -v d="$djiff" -v t="$tck" -v w="$window" 'BEGIN{printf "%.1f", (d/t)/w*100}')
    rss_mb=$(awk -v r="${rss:-0}" 'BEGIN{printf "%.1f", r/1024}')
    printf "%s %s %s\n" "$pod" "$cpu" "$rss_mb"
  done
}

# caps <cps> <seconds>
caps() {
  local cps="$1" secs="$2"
  mkdir -p "$RESULTS"
  local ramp=8 sample=$(( secs > 8 ? secs-8 : secs ))
  export CAPS="$cps" MAX_CALLS=$(( cps * (secs+20) )) UAC_JOB_NAME="sipp-uac-${cps}"
  kubectl -n "$NS" delete job "$UAC_JOB_NAME" --ignore-not-found >/dev/null 2>&1 || true
  log "cap=$cps: launching UAC job (${secs}s), ramp ${ramp}s, sample ${sample}s"
  envsubst < manifests/40-sipp-uac-job.yaml | kubectl apply -f -
  sleep "$ramp"
  echo "  --- worker/proxy CPU% + RSS(MB) over ${sample}s @ ${cps} cps ---"
  { sample_pods "app=b2bua-worker" "$sample"; sample_pods "app=sip-front-proxy" 2; } | tee "$RESULTS/cap${cps}.txt"
  # Capture UAC call outcome before deleting the job.
  # Capture UAC call outcome before deleting the job. Guard the whole block:
  # grep returns non-zero on no-match, which must not abort the sweep.
  set +e
  local upod stats ok fail total
  upod="$(kubectl -n "$NS" get pods -l app=sipp-uac --sort-by=.metadata.creationTimestamp -o jsonpath='{.items[-1:].metadata.name}' 2>/dev/null)"
  if [ -n "$upod" ]; then
    stats="$(kubectl -n "$NS" logs "$upod" 2>/dev/null)"
    ok="$(printf '%s' "$stats"   | grep -aE 'Successful call'     | tail -1 | grep -oE '[0-9]+' | tail -1)"
    fail="$(printf '%s' "$stats" | grep -aE 'Failed call'         | tail -1 | grep -oE '[0-9]+' | tail -1)"
    total="$(printf '%s' "$stats"| grep -aE 'Total Calls created' | tail -1 | grep -oE '[0-9]+' | tail -1)"
    printf "  UAC: total=%s successful=%s failed=%s\n" "${total:-?}" "${ok:-?}" "${fail:-?}" | tee -a "$RESULTS/cap${cps}.txt"
  fi
  set -e
  kubectl -n "$NS" delete job "$UAC_JOB_NAME" --ignore-not-found >/dev/null 2>&1 || true
  sleep 6   # let in-flight calls drain before the next cap
}

sweep() {
  local secs="$1"; shift
  printf "\n==== CAP SWEEP (sampling %ss each) ====\n" "$secs"
  for c in "$@"; do caps "$c" "$secs"; done
  printf "\nPer-cap CPU%%/RSS in %s/. Scrape app metrics with:\n  kubectl -n %s port-forward deploy/sip-front-proxy 9090 & curl localhost:9090/metrics\n" "$RESULTS" "$NS"
}

down() { kind delete cluster --name "$CLUSTER"; }

cmd="${1:-}"; shift || true
case "$cmd" in
  up)     up ;;
  deploy) deploy ;;
  caps)   caps "$@" ;;
  sweep)  sweep "$@" ;;
  all)    up; deploy; sweep "$@" ;;
  down)   down ;;
  *) die "usage: $0 {up|deploy|caps <cps> <secs>|sweep <secs> <cps...>|all <secs> <cps...>|down}" ;;
esac
