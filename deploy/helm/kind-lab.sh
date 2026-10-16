#!/usr/bin/env bash
# The kind lab from the charts. The cluster, the external SIP plane, the host
# preflight (cgroups, the sysctls of ADR-0033) and the images come from
# deploy/k8s/run.sh's library, its `up` with its own observability path off;
# the SUT and the in-cluster collectors are two helm releases on the kind
# values (deploy/helm/README.md).
#
#   kind-lab.sh up                   cluster + sipext bridge + images, kind-loaded
#   kind-lab.sh image <tag> [docker build args]
#                                    build one more image with the lab's build
#                                    wrapper (proxy settings) and kind-load it
#   kind-lab.sh deploy [helm args]   the collectors, Chaos Mesh, the SIPp UAS, the
#                                    SUT, then the VIP and isolation smokes (when
#                                    the SUT runs a proxy) and the series check;
#                                    helm args go to the SUT release (e.g. --set
#                                    worker.replicas=3)
#   kind-lab.sh template [helm args] the SUT release as deploy would install it,
#                                    rendered, nothing applied
#   kind-lab.sh chaos-mesh           Chaos Mesh alone (chaos-mesh-values-kind.yaml)
#   kind-lab.sh uas                  (re)start the SIPp callees (the hakit base UAS)
#   kind-lab.sh down                 the cluster and the sipext plane
#
# The SUT release is the siprust chart by default. A product chart that
# depends on it is deployed instead with SUT_CHART (its directory), SUT_RELEASE
# (its release name) and SUT_SUBCHART (the name it gives the siprust
# dependency): the siprust kind values are passed nested under that name, then
# the product chart's own values-kind.yaml, then the helm args.
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

# The SUT release: the siprust chart, or the product chart SUT_CHART names.
SUT_CHART="${SUT_CHART:-$HELM_DIR/siprust}"
SUT_RELEASE="${SUT_RELEASE:-siprust}"
SUT_SUBCHART="${SUT_SUBCHART:-}"

# The siprust kind values nested under SUT_SUBCHART, as a values file of the
# product chart; its path on stdout. Nothing when the SUT is the siprust chart.
sut_kind_values() {
  local out
  [ -n "$SUT_SUBCHART" ] || return 0
  out="$(mktemp "${TMPDIR:-/tmp}/sut-kind-values.XXXXXX.yaml")"
  { printf '%s:\n' "$SUT_SUBCHART"; sed 's/^/  /' "$HELM_DIR/siprust/values-kind.yaml"; } > "$out"
  printf '%s\n' "$out"
}

# A product chart's dependencies, built from their file:// sources.
sut_dependencies() {
  [ -z "$SUT_SUBCHART" ] || helm dependency update "$SUT_CHART" >/dev/null \
    || die "helm dependency update $SUT_CHART failed"
}

# `helm <verb> <release> <chart>` of the SUT with its whole value set: the
# nested siprust kind values, the chart's values-kind.yaml, then the args.
sut_helm() { # <verb...> -- [helm args]
  local verb=() nested rc=0
  while [ "$1" != -- ]; do verb+=("$1"); shift; done
  shift
  sut_dependencies
  nested="$(sut_kind_values)"
  helm "${verb[@]}" "$SUT_RELEASE" "$SUT_CHART" --kube-context "kind-$CLUSTER" -n "$NS" \
    ${nested:+-f "$nested"} -f "$SUT_CHART/values-kind.yaml" "$@" || rc=$?
  [ -z "$nested" ] || rm -f "$nested"
  return "$rc"
}

sut_release() { # [helm args]
  log "helm upgrade --install $SUT_RELEASE ($NS, $SUT_CHART)"
  sut_helm upgrade --install -- --create-namespace "$@" --wait --timeout "$ROLLOUT_TIMEOUT"
}

sut_template() { # [helm args]
  sut_helm template -- "$@"
}

# A namespace run.sh deployed holds the same object names without helm's
# ownership, which helm refuses to adopt; another release's objects likewise.
require_no_raw_deploy() {
  local owner release
  kubectl -n "$NS" get statefulset b2bua-worker >/dev/null 2>&1 || return 0
  owner="$(kubectl -n "$NS" get statefulset b2bua-worker \
    -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}')"
  [ "$owner" = "Helm" ] \
    || die "namespace $NS holds a run.sh deploy; delete it first: kubectl --context kind-$CLUSTER delete namespace $NS"
  release="$(kubectl -n "$NS" get statefulset b2bua-worker \
    -o jsonpath='{.metadata.annotations.meta\.helm\.sh/release-name}')"
  [ "$release" = "$SUT_RELEASE" ] \
    || die "namespace $NS holds release $release; uninstall it first: helm --kube-context kind-$CLUSTER -n $NS uninstall $release"
}

# One more image, built with the lab's build wrapper and loaded into the cluster.
lab_image() { # <tag> [docker build args]
  [ $# -ge 1 ] || die "usage: $0 image <tag> [docker build args]"
  local tag="$1"; shift
  docker_build -t "$tag" "$@"
  log "loading $tag into kind"
  kind_load "$tag"
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
  sut_release "$@"
  if kubectl -n "$NS" get deploy sip-front-proxy >/dev/null 2>&1; then
    vip_smoke || die "vip-smoke: the external VIP ${SIPEXT_VIP}:${SIP_PORT} answers nothing; kubectl -n $NS logs deploy/sip-front-proxy -c keepalived"
    isolation_smoke
  else
    log "no front proxy in $NS: the VIP and isolation smokes do not apply"
  fi
  obs_check stack || die "observability: series missing after deploy (kubectl -n $OBS_NS get pods)"
  log "lab ready"
  kubectl -n "$NS" get pods -o wide
}

case "${1:-}" in
  up) up ;;
  image) shift; lab_image "$@" ;;
  deploy) shift; lab_deploy "$@" ;;
  template) shift; sut_template "$@" ;;
  chaos-mesh) chaos_mesh_release ;;
  uas) hakit_uas_up ;;
  down) down ;;
  *) die "usage: $0 {up|image <tag> [build args]|deploy [helm args]|template [helm args]|chaos-mesh|uas|down}" ;;
esac
