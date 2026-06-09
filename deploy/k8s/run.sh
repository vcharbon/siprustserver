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
#   ./run.sh up                      # (re)create cluster + build/load images + observability
#   ./run.sh deploy                  # apply uas + workers + proxy, wait ready
#   ./run.sh obs                     # (re)deploy observability stack only (idempotent)
#   ./run.sh caps 200 30             # 200 cps for 30s, sample CPU/mem
#   ./run.sh sweep 30 50 100 200 400 # run a list of caps, 30s sampling each
#   ./run.sh all 30 50 100 200 400   # up + deploy + sweep (leaves cluster up)
#   ./run.sh down                    # delete the cluster
#
# Observability (VictoriaMetrics + Grafana + vmagent/KSM/node-exporter/fluent-bit)
# is brought up automatically by `up` via deploy/observability/install.sh and
# survives across runs (host docker-compose); the in-cluster scrapers are
# re-applied on every `up` since recreating the cluster wipes them. Grafana:
# http://localhost:3333 (anonymous admin). Set OBS_ENABLE=0 to skip.
#
# Env: SUT_IMAGE=siprustserver:dev  WORKER_REPLICAS=2  CLUSTER=sip-e2e  NS=sip-test
#      OBS_ENABLE=1
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
# Front-proxy HA (ADR-0012 D7): the proxy sits behind a keepalived VRRP VIP. SIPp
# UAC streams target the VIP, not the Service. PROXY_VIP is baked into the proxy
# manifest literally; PROXY_TARGET (what envsubst stamps into the UAC job) defaults
# to it. LIMITER_CAP feeds the -key xapi JSON in the UAC job (default 20).
PROXY_VIP="${PROXY_VIP:-172.20.255.250}"
PROXY_TARGET="${PROXY_TARGET:-$PROXY_VIP}"
LIMITER_CAP="${LIMITER_CAP:-20}"
KEEPALIVED_IMAGE="${KEEPALIVED_IMAGE:-siprustserver-keepalived:dev}"
# CDR transport: RabbitMQ broker (55-rabbitmq) + cdr-consumer (56-cdr-consumer).
# The broker is a public image pulled + side-loaded into kind so the run is
# offline-capable like every other image.
RABBITMQ_IMAGE="${RABBITMQ_IMAGE:-rabbitmq:3.13-management}"
export SUT_IMAGE WORKER_REPLICAS REPL_ENABLE REPL_PORT SCENARIO PROXY_VIP PROXY_TARGET LIMITER_CAP RABBITMQ_IMAGE
# Observability: VictoriaMetrics + Grafana host stack + in-cluster vmagent/KSM/
# node-exporter/fluent-bit. Deployed automatically on `up` so the freshly
# (re)created cluster always has scraping wired and Grafana dashboards loaded.
# Set OBS_ENABLE=0 to skip (e.g. CI without docker compose).
OBS_ENABLE="${OBS_ENABLE:-1}"
OBS_DIR="$REPO_ROOT/deploy/observability"

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

  # Cap each kind node's memory so the cluster can never starve the WSL2 host
  # (an uncapped node + a parallel cargo build OOM'd the host once). Pod limits
  # still apply inside; this is the node-container backstop.
  log "capping kind node memory (host-starvation backstop)"
  CLUSTER="$CLUSTER" "$REPO_ROOT/deploy/k8s/cap-kind-memory.sh" || true

  log "building SUT image $SUT_IMAGE"
  docker build -f "$REPO_ROOT/deploy/docker/Dockerfile" -t "$SUT_IMAGE" "$REPO_ROOT"
  log "building sipp:dev from vendored context ($SIPP_DIR)"
  docker build -t sipp:dev "$SIPP_DIR"
  log "building keepalived sidecar image $KEEPALIVED_IMAGE (proxy VIP, ADR-0012 D7)"
  docker build -f "$REPO_ROOT/deploy/docker/Dockerfile.keepalived" -t "$KEEPALIVED_IMAGE" "$REPO_ROOT"
  log "pulling RabbitMQ image $RABBITMQ_IMAGE (CDR transport)"
  docker image inspect "$RABBITMQ_IMAGE" >/dev/null 2>&1 || docker pull "$RABBITMQ_IMAGE"
  log "loading images into kind"
  kind load docker-image "$SUT_IMAGE" --name "$CLUSTER"
  kind load docker-image sipp:dev --name "$CLUSTER"
  kind load docker-image "$KEEPALIVED_IMAGE" --name "$CLUSTER"
  kind load docker-image "$RABBITMQ_IMAGE" --name "$CLUSTER"

  obs   # bring up / refresh observability against the new cluster
}

# Deploy (or re-apply) the observability stack: host VM+Grafana via docker
# compose + the in-cluster scrapers (vmagent/KSM/node-exporter/fluent-bit).
# install.sh is idempotent; recreating the cluster wipes the in-cluster
# `observability` namespace, so this must re-run after every `up`.
obs() {
  [ "$OBS_ENABLE" = "1" ] || { log "OBS_ENABLE=0 — skipping observability"; return 0; }
  [ -x "$OBS_DIR/install.sh" ] || die "observability installer missing: $OBS_DIR/install.sh"
  log "deploying observability (Grafana http://localhost:3333, VictoriaMetrics :8428)"
  "$OBS_DIR/install.sh" --bootstrap
}

deploy() {
  preflight
  kubectl apply -f manifests/00-namespace.yaml
  log "building sipp-scenarios ConfigMap from vendored scenarios"
  kubectl -n "$NS" create configmap sipp-scenarios \
    --from-file="$SCENARIOS/" -o yaml --dry-run=client | kubectl apply -f -

  # SIPp stat->Prometheus exporter script, mounted into every UAC job's native
  # sidecar (manifests/40-sipp-uac-job.yaml). Built here so the reporting path
  # is wired through the same `deploy` as the rest of the stack.
  log "building sipp-exporter ConfigMap from vendored exporter"
  kubectl -n "$NS" create configmap sipp-exporter \
    --from-file="$SIPP_DIR/exporter/" -o yaml --dry-run=client | kubectl apply -f -

  log "deploying sipp-uas + b2bua workers (repl=${REPL_ENABLE}, repl_port=${REPL_PORT})"
  kubectl apply -f manifests/10-sipp-uas.yaml
  # RBAC for the worker's EndpointSlice informer (needed when REPL_ENABLE=1; a
  # harmless ServiceAccount/Role otherwise).
  kubectl apply -f manifests/15-worker-rbac.yaml
  # Shared call limiter (single replica). Deployed before the workers so its
  # ClusterIP DNS resolves at worker startup. Inert until a decision returns
  # `call_limiter` entries, so it is a no-op for the scripted load runner.
  envsubst < manifests/50-call-limiter.yaml | kubectl apply -f -
  # CDR transport: RabbitMQ broker + the dedicated CDR metrics consumer. Broker
  # first (before the workers) so its ClusterIP DNS resolves at worker startup;
  # workers fail-open (drop CDRs) if it is briefly unavailable.
  log "deploying RabbitMQ (CDR transport) + cdr-consumer"
  envsubst < manifests/55-rabbitmq.yaml | kubectl apply -f -
  # CDR infra is best-effort telemetry, NOT on the call path: the workers
  # fail-open (drop CDRs) if the broker is slow/absent. So wait for it, but never
  # let an unhealthy broker/consumer abort the run (these gates are `|| true`,
  # unlike the call-path worker/proxy/uas gates below).
  kubectl -n "$NS" rollout status deploy/rabbitmq --timeout=120s || true
  envsubst < manifests/56-cdr-consumer.yaml | kubectl apply -f -
  envsubst < manifests/20-worker.yaml | kubectl apply -f -
  kubectl -n "$NS" rollout status statefulset/sipp-uas --timeout=120s
  kubectl -n "$NS" rollout status statefulset/b2bua-worker --timeout=120s
  kubectl -n "$NS" rollout status deploy/cdr-consumer --timeout=120s || true

  # The proxy now discovers the worker pool from k8s EndpointSlices (ADR-0012 D4)
  # — the SAME informer the b2bua replication engine uses (ADR-0011 X7) — so the
  # old "resolve worker pod IPs -> PROXY_WORKERS" bake is GONE (and with it the
  # chaos.sh proxy redeploy after a worker kill: a restarted worker's new IP flows
  # through the watch automatically).
  #
  # The worker *id* is still the pod name (EndpointSlice targetRef.name = POD_NAME
  # = B2BUA_ORDINAL = replication membership ordinal): the proxy signs it into the
  # w_pri/w_bak stickiness cookie, the b2bua echoes w_pri into the callRef primary
  # and w_bak into topology.bak (the changelog peer key). If those disagreed, the
  # changelog would bump under a peer no puller subscribes as → repl_pull_applied
  # stays 0 and the backup holds no replicas. EndpointSlice targetRef.name makes
  # callRef ownership, the cookie, and replication membership agree by construction
  # (see 20-worker.yaml / 25-proxy-rbac.yaml).
  log "deploying sip-front-proxy (k8s EndpointSlice worker discovery)"
  kubectl apply -f manifests/25-proxy-rbac.yaml
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
  export ROLE="${ROLE:-load}" MAX_CONCURRENT="${MAX_CONCURRENT:-$(( cps * 600 ))}"
  # Pod-resource envsubst vars for the shared 40-sipp-uac-job template (no default
  # syntax in envsubst — every render site must export them).
  export UAC_CPU_REQ="${UAC_CPU_REQ:-2}" UAC_CPU_LIM="${UAC_CPU_LIM:-8}" \
         UAC_MEM_REQ="${UAC_MEM_REQ:-384Mi}" UAC_MEM_LIM="${UAC_MEM_LIM:-1536Mi}"
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
  obs)    obs ;;
  caps)   caps "$@" ;;
  sweep)  sweep "$@" ;;
  all)    up; deploy; sweep "$@" ;;
  down)   down ;;
  *) die "usage: $0 {up|deploy|obs|caps <cps> <secs>|sweep <secs> <cps...>|all <secs> <cps...>|down}" ;;
esac
