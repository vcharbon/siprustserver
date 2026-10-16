# shellcheck shell=bash source-path=SCRIPTDIR
# The kind cluster of the lab (kind-lab.sh): the cluster itself on a pinned
# docker bridge, the external SIP plane (sipext), the images, and the
# teardown. Sourcing defines knobs and functions and runs nothing.
#
# `up` refuses a cluster that already exists: one left by a failed bring-up
# survives for analysis, and only `down` (or FORCE_RECREATE=1) destroys it.
#
# Env: CLUSTER=sip-e2e NS=sip-test KIND_WAIT=300s ROLLOUT_TIMEOUT=300s
#      SUT_IMAGE=siprustserver:dev KEEPALIVED_IMAGE=siprustserver-keepalived:dev
#      RABBITMQ_IMAGE=rabbitmq:3.13-management FORCE_RECREATE=0
#      the network plan of deploy/k8s/lib/net-env.sh, the host checks of
#      deploy/k8s/lib/host-checks.sh
K8S_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../k8s" && pwd)"
REPO_ROOT="$(cd "$K8S_DIR/../.." && pwd)"

log() { printf '\033[1;36m>> %s\033[0m\n' "$*" >&2; }
die() { printf '\033[1;31m!! %s\033[0m\n' "$*" >&2; exit 1; }

# shellcheck source=../../k8s/lib/net-env.sh
source "$K8S_DIR/lib/net-env.sh"
# shellcheck source=../../k8s/lib/proxy-env.sh
source "$K8S_DIR/lib/proxy-env.sh"
# shellcheck source=../../k8s/lib/host-checks.sh
source "$K8S_DIR/lib/host-checks.sh"
# shellcheck source=../../k8s/lib/kube-env.sh
source "$K8S_DIR/lib/kube-env.sh"

CLUSTER="${CLUSTER:-sip-e2e}"
NS="${NS:-sip-test}"
KIND_WAIT="${KIND_WAIT:-300s}"
ROLLOUT_TIMEOUT="${ROLLOUT_TIMEOUT:-300s}"
SUT_IMAGE="${SUT_IMAGE:-siprustserver:dev}"
KEEPALIVED_IMAGE="${KEEPALIVED_IMAGE:-siprustserver-keepalived:dev}"
RABBITMQ_IMAGE="${RABBITMQ_IMAGE:-rabbitmq:3.13-management}"
CLUSTER_CONFIG="$K8S_DIR/cluster.yaml"
SIPP_DIR="$K8S_DIR/sipp"

lab_preflight() {
  local t
  for t in kind kubectl docker helm; do command -v "$t" >/dev/null || die "missing tool: $t"; done
  [ -f "$CLUSTER_CONFIG" ] || die "cluster.yaml missing"
  [ -f "$SIPP_DIR/Dockerfile" ] || die "sipp/Dockerfile missing (the SIPp image build context)"
}

up() {
  lab_preflight
  check_host
  sync_clock_at_bringup
  if kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    if [ "${FORCE_RECREATE:-0}" = "1" ]; then
      log "FORCE_RECREATE=1: deleting the existing '$CLUSTER' cluster"
      kind delete cluster --name "$CLUSTER" 2>/dev/null || true
    else
      die "cluster '$CLUSTER' already exists, left intact for analysis: destroy it with 'down' (or FORCE_RECREATE=1)"
    fi
  fi
  # A failed bring-up leaves the partial cluster for analysis.
  trap up_failed EXIT

  # The kind bridge on the immutable SIP_SUBNET keeps the keepalived VIP on
  # subnet across recreates; an existing bridge with that subnet is reused,
  # since the host observability stack stays attached to it.
  log "pinning the kind docker bridge to $SIP_SUBNET (VIP $PROXY_VIP on subnet)"
  if [ "$(docker network inspect kind --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}' 2>/dev/null | tr ' ' '\n' | grep -cF "$SIP_SUBNET")" != "1" ]; then
    docker network rm kind >/dev/null 2>&1 || true
    docker network create --driver bridge --subnet "$SIP_SUBNET" --gateway "$SIP_GATEWAY" kind >/dev/null
  fi
  export KIND_EXPERIMENTAL_DOCKER_NETWORK=kind

  log "creating cluster '$CLUSTER'"
  kind create cluster --name "$CLUSTER" --config "$CLUSTER_CONFIG" --wait "$KIND_WAIT"
  # The node memory caps are the host-starvation backstop: a refused cap stops `up`.
  log "capping kind node memory"
  CLUSTER="$CLUSTER" "$K8S_DIR/cap-kind-memory.sh" \
    || die "kind node memory caps refused (cap-kind-memory.sh above): the cluster is up but uncapped"
  sipext_up

  log "proxy env (forwarded to docker build): $(proxy_env_summary)"
  log "building SUT image $SUT_IMAGE"
  docker_build -f "$REPO_ROOT/deploy/docker/Dockerfile" -t "$SUT_IMAGE" "$REPO_ROOT"
  log "building sipp:dev ($SIPP_DIR)"
  docker_build -t sipp:dev "$SIPP_DIR"
  log "building keepalived sidecar image $KEEPALIVED_IMAGE"
  docker_build -f "$REPO_ROOT/deploy/docker/Dockerfile.keepalived" -t "$KEEPALIVED_IMAGE" "$REPO_ROOT"
  log "pulling RabbitMQ image $RABBITMQ_IMAGE"
  docker image inspect "$RABBITMQ_IMAGE" >/dev/null 2>&1 || docker pull "$RABBITMQ_IMAGE"
  log "loading images into kind"
  # sipp:dev stays on the host: every SIPp generator is a docker container on sipext.
  kind_load "$SUT_IMAGE"
  kind_load "$KEEPALIVED_IMAGE"
  kind_load "$RABBITMQ_IMAGE"
  trap - EXIT
}

up_failed() {
  local rc=$?
  [ "$rc" -eq 0 ] || log "up failed (rc=$rc): cluster $CLUSTER left intact (kubectl --context kind-$CLUSTER get pods -A)"
}

# The external SIP plane: a docker bridge with masquerade off, the generators
# and callees on it, and the two tier=edge nodes dual-homed to it (the only
# components on both planes; isolation_smoke holds that).
sipext_up() {
  if [ "$(docker network inspect "$SIPEXT_NET" --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}' 2>/dev/null | tr ' ' '\n' | grep -cF "$SIPEXT_SUBNET")" != "1" ]; then
    docker network rm "$SIPEXT_NET" >/dev/null 2>&1 || true
    docker network create --driver bridge \
      --subnet "$SIPEXT_SUBNET" --gateway "$SIPEXT_GATEWAY" \
      -o com.docker.network.bridge.enable_ip_masquerade=false \
      "$SIPEXT_NET" >/dev/null
    log "sipext: created no-NAT bridge $SIPEXT_NET ($SIPEXT_SUBNET)"
  fi
  # Sorted, so edge1/edge2 get .11/.12 deterministically; the new interface is
  # SIPEXT_IF, which keepalived tracks and the external VIP floats on.
  local edges node ip n=0
  edges="$(kubectl get nodes -l tier=edge -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' | sort)"
  [ -n "$edges" ] || die "sipext: no tier=edge nodes found (cluster not up?)"
  for node in $edges; do
    case "$n" in
      0) ip="$SIPEXT_EDGE1_IP" ;;
      1) ip="$SIPEXT_EDGE2_IP" ;;
      *) die "sipext: more than 2 tier=edge nodes" ;;
    esac
    if ! docker network inspect "$SIPEXT_NET" --format '{{range .Containers}}{{.Name}} {{end}}' | grep -qw "$node"; then
      docker network connect --ip "$ip" "$SIPEXT_NET" "$node"
      log "sipext: connected edge node $node as $ip"
    fi
    n=$((n + 1))
  done
  [ "$n" -eq 2 ] || log "sipext: WARNING expected 2 edge nodes, connected $n"
}

# The shared dependencies run on the tier=infra node (cluster.yaml): a cluster
# made before it has none, and they would stay Pending.
require_infra_node() {
  kubectl get nodes -l tier=infra -o name 2>/dev/null | grep -q . \
    || die "no tier=infra node in cluster '$CLUSTER': recreate it (down, then up)"
}

# The cluster, every container labelled sipext-run=$CLUSTER and the sipext bridge.
down() {
  kind delete cluster --name "$CLUSTER"
  docker ps -aq --filter "label=sipext-run=$CLUSTER" | xargs -r docker rm -f >/dev/null 2>&1 || true
  docker network rm "$SIPEXT_NET" >/dev/null 2>&1 || true
}
