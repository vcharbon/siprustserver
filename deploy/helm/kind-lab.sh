#!/usr/bin/env bash
# The kind lab from the charts. The cluster, the external SIP plane, the host
# preflight (cgroups, the sysctls of ADR-0033) and the images come from
# deploy/k8s/run.sh's library, its `up` with its own observability path off;
# the SUT and the in-cluster collectors are two helm releases on the kind
# values (deploy/helm/README.md).
#
#   kind-lab.sh up                   cluster + sipext bridge + images, kind-loaded
#   kind-lab.sh deploy [helm args]   the collectors, the SIPp UAS, the SUT, then
#                                    the VIP and isolation smokes and the series
#                                    check; helm args go to the SUT release
#                                    (e.g. --set worker.replicas=3)
#   kind-lab.sh down                 the cluster and the sipext plane
#
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

lab_deploy() {
  command -v helm >/dev/null || die "missing tool: helm"
  require_infra_node
  require_no_raw_deploy
  require_host_stack
  helm_release siprust-observability "$HELM_DIR/siprust-observability" "$OBS_NS"
  # The UAS containers first: the first one is the workers' scripted callee.
  sipp_uas_up
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
  down) down ;;
  *) die "usage: $0 {up|deploy [helm args]|down}" ;;
esac
