#!/usr/bin/env bash
# The kind lab from the charts. The cluster, the external SIP plane, the host
# preflight (cgroups, the sysctls of ADR-0033) and the images come from
# deploy/k8s/run.sh's library, its `up` with its own observability path off;
# the SUT and the in-cluster collectors are two helm releases on the kind
# values (deploy/helm/README.md).
#
#   kind-lab.sh up                   cluster + sipext bridge + images, kind-loaded
#   kind-lab.sh deploy [helm args]   the collectors, Chaos Mesh, the SIPp UAS, the
#                                    SUT, then the VIP and isolation smokes and
#                                    the series check; helm args go to the SUT
#                                    release (e.g. --set worker.replicas=3)
#   kind-lab.sh chaos-mesh           Chaos Mesh alone (chaos-mesh-values-kind.yaml)
#   kind-lab.sh uas                  (re)start the SIPp callees (the hakit base UAS)
#   kind-lab.sh down                 the cluster and the sipext plane
#
# Env: CHAOS_MESH_VERSION=2.8.4 (the pinned chart and images); CHAOS_MESH=0
# skips it (the runner then sees no `chaos-mesh` capability)
# The host stack (VictoriaMetrics :8428, VictoriaLogs :9428, Grafana :3333) is
# deploy/observability's docker compose stack, outside the cluster: `deploy`
# needs it up and never starts it.
#
# Env: CLUSTER=sip-e2e NS=sip-test OBS_NS=observability ROLLOUT_TIMEOUT=300s
set -euo pipefail
HELM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OBS_NS="${OBS_NS:-observability}"
# run.sh's observability path (install.sh, the raw kind-addons) stays off: the
# collectors are the siprust-observability release.
export OBS_ENABLE=0
# shellcheck source-path=SCRIPTDIR source=../k8s/run.sh
source "$HELM_DIR/../k8s/run.sh"

# run.sh's up() preflight without envsubst: the chart path renders no manifest
# through it.
preflight() {
  local t
  for t in kind kubectl docker helm; do command -v "$t" >/dev/null || die "missing tool: $t"; done
  [ -f "$CLUSTER_CONFIG" ] || die "cluster.yaml missing"
  [ -f "$SIPP_DIR/Dockerfile" ] || die "sipp/Dockerfile missing (vendored sipp build context)"
  [ -d "$SCENARIOS" ] || die "sipp/scenarios/ missing (vendored sipp scenarios)"
}

helm_release() { # <release> <chart dir> <namespace> [helm args]
  local release="$1" chart="$2" ns="$3"; shift 3
  log "helm upgrade --install $release ($ns)"
  helm upgrade --install "$release" "$chart" --kube-context "kind-$CLUSTER" \
    -n "$ns" --create-namespace -f "$chart/values-kind.yaml" "$@" \
    --wait --timeout "$ROLLOUT_TIMEOUT"
}

# A namespace run.sh deployed holds the same object names without helm's
# ownership, which helm refuses to adopt.
require_no_raw_deploy() {
  local owner
  kubectl -n "$NS" get statefulset b2bua-worker >/dev/null 2>&1 || return 0
  owner="$(kubectl -n "$NS" get statefulset b2bua-worker \
    -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}')"
  [ "$owner" = "Helm" ] \
    || die "namespace $NS holds a run.sh deploy; delete it first: kubectl --context kind-$CLUSTER delete namespace $NS"
}

require_host_stack() {
  local url
  for url in http://127.0.0.1:8428/health http://127.0.0.1:9428/health; do
    curl -fsS --noproxy '*' -m 2 "$url" >/dev/null \
      || die "host stack down ($url): bring deploy/observability's compose stack up first"
  done
}

# Chaos Mesh, the backend of the partition, IO and clock incidents, pinned.
CHAOS_MESH_VERSION="${CHAOS_MESH_VERSION:-2.8.4}"
chaos_mesh_release() {
  log "helm upgrade --install chaos-mesh $CHAOS_MESH_VERSION (chaos-mesh)"
  helm upgrade --install chaos-mesh chaos-mesh --repo https://charts.chaos-mesh.org \
    --version "$CHAOS_MESH_VERSION" --kube-context "kind-$CLUSTER" -n chaos-mesh --create-namespace \
    -f "$HELM_DIR/chaos-mesh-values-kind.yaml" --set "images.tag=v$CHAOS_MESH_VERSION" \
    --wait --timeout "$ROLLOUT_TIMEOUT"
}

# The SIPp callees: the HA tooling's base UAS (tooling/hakit/content/scenarios)
# with the key defaults it declares, at the addresses run.sh's callees take,
# so every catalog shape (a callee that re-INVITEs among them) is served. The
# scenario is copied into each container, never mounted: a callee outlives the
# checkout that started it.
HAKIT_DIR="$(cd "$HELM_DIR/../../tooling/hakit" && pwd)"
hakit_uas_up() {
  local prefix="${SIPEXT_UAS_IP%.*}" base="${SIPEXT_UAS_IP##*.}" i ip keys
  docker network inspect "$SIPEXT_NET" >/dev/null 2>&1 || die "sipext network missing: '$0 up' creates it"
  mapfile -t keys < <(uv run --quiet --locked --project "$HAKIT_DIR" hakit sipp-keys uas-basic.xml) \
    || die "hakit sipp-keys failed (needs uv)"
  mkdir -p "$SIPEXT_GEN_DIR"
  printf 'SEQUENTIAL\n' > "$SIPEXT_GEN_DIR/uas-targets.csv"
  for i in $(seq 0 $(( SIPP_UAS_REPLICAS - 1 ))); do
    ip="${prefix}.$(( base + i ))"
    docker rm -f "sipp-uas-$i" >/dev/null 2>&1 || true
    docker create --name "sipp-uas-$i" --label "sipext-run=$CLUSTER" \
      --restart unless-stopped \
      --network "$SIPEXT_NET" --ip "$ip" \
      --cpus "$SIPP_UAS_CPUS" --memory "$SIPP_UAS_MEM" \
      sipp:dev sipp -sf /scenarios/uas-basic.xml "${keys[@]}" -i "$ip" -p 5060 \
        -l 60000 -recv_timeout 900000 \
        -trace_err -ringbuffer_files 1 -ringbuffer_size 16777216 >/dev/null
    docker cp "$HAKIT_DIR/content/scenarios" "sipp-uas-$i:/scenarios" >/dev/null
    docker start "sipp-uas-$i" >/dev/null
    printf '%s\n' "$ip" >> "$SIPEXT_GEN_DIR/uas-targets.csv"
    log "sipp-uas-$i up at $ip (docker, sipext, hakit uas-basic.xml)"
  done
}

lab_deploy() {
  preflight
  require_infra_node
  require_no_raw_deploy
  require_host_stack
  helm_release siprust-observability "$HELM_DIR/siprust-observability" "$OBS_NS" \
    --set "clusterName=kind-$CLUSTER"
  if [ "${CHAOS_MESH:-1}" = 1 ]; then chaos_mesh_release; else log "Chaos Mesh skipped (CHAOS_MESH=0)"; fi
  # The UAS containers first: the first one is the workers' scripted callee.
  hakit_uas_up
  helm_release siprust "$HELM_DIR/siprust" "$NS" "$@"
  vip_smoke || die "vip-smoke: the external VIP ${SIPEXT_VIP}:${SIP_PORT} answers nothing; kubectl -n $NS logs deploy/sip-front-proxy -c keepalived"
  isolation_smoke
  obs_check stack || die "observability: series missing after deploy (kubectl -n $OBS_NS get pods)"
  log "lab ready"
  kubectl -n "$NS" get pods -o wide
}

case "${1:-}" in
  up) up ;;
  deploy) shift; lab_deploy "$@" ;;
  chaos-mesh) chaos_mesh_release ;;
  uas) hakit_uas_up ;;
  down) down ;;
  *) die "usage: $0 {up|deploy [helm args]|chaos-mesh|uas|down}" ;;
esac
