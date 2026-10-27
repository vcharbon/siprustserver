#!/usr/bin/env bash
# Weight the CPU of each kind node container by tier, so the SUT's pods get a
# large share of the host under contention. Every node container sits in the
# same parent cgroup with the same default weight, so under host contention the
# app node gets no more than any other node: its pods wait for CPU while their
# own cgroup is far below its limit. The app tier gets the maximum weight,
# infra and edge (the SUT's dependencies and the load generators) a raised one,
# the control plane is left at the default. A weight is a share under
# contention, never a reservation: an idle node takes nothing from anyone.
#
# The knobs are docker CPU shares (`docker update --cpu-shares`, no root); the
# runtime maps them onto cgroup v2 `cpu.weight` by its own formula, which
# differs between runc versions, so the weight each node got is read back from
# inside the node and printed. Shares 262144 map to the maximum weight 10000
# under every formula. The weight persists on the node container; re-running
# this script re-applies it on a live cluster. Tier is the kube node label
# `tier` (set in cluster.yaml), read from the cluster's own context.
#
# Override per-invocation:  EDGE_CPU_SHARES=8192 ./weight-kind-cpu.sh
set -euo pipefail

CLUSTER="${CLUSTER:-sip-e2e}"
APP_CPU_SHARES="${APP_CPU_SHARES:-262144}"   # app nodes: the SUT pods (cpu.weight 10000)
INFRA_CPU_SHARES="${INFRA_CPU_SHARES:-26214}" # infra node: broker, limiter, CDR consumer
EDGE_CPU_SHARES="${EDGE_CPU_SHARES:-26214}"   # edge nodes: front proxy, pod-placed load generators

for v in APP_CPU_SHARES INFRA_CPU_SHARES EDGE_CPU_SHARES; do
  [[ "${!v}" =~ ^[0-9]+$ ]] || { echo "$v='${!v}' is not a number of CPU shares"; exit 1; }
  printf -v "$v" '%d' "$((10#${!v}))"
  [ "${!v}" -ge 2 ] && [ "${!v}" -le 262144 ] || { echo "$v=${!v} is not in 2..262144 (docker CPU shares)"; exit 1; }
done

nodes_out="$(docker ps -a --filter "label=io.x-k8s.kind.cluster=${CLUSTER}" --format '{{.Names}}')" \
  || { echo "docker ps failed: cannot list the kind nodes of '${CLUSTER}'"; exit 1; }
mapfile -t nodes <<<"$nodes_out"
[ -z "${nodes[0]:-}" ] && { echo "no kind nodes for cluster '${CLUSTER}' — nothing to weight"; exit 0; }

# Tier of a kind node from its kube label (kind node name == container name).
node_tier() {
  kubectl --context "kind-${CLUSTER}" get node "$1" -o jsonpath='{.metadata.labels.tier}'
}

declare -A shares_of
apps=0
for c in "${nodes[@]}"; do
  tier="$(node_tier "$c")" || { echo "cannot read the tier label of node $c (kubectl, context kind-${CLUSTER})"; exit 1; }
  case "$tier" in
    app)   shares_of[$c]="$APP_CPU_SHARES"; apps=$(( apps + 1 )) ;;
    infra) shares_of[$c]="$INFRA_CPU_SHARES" ;;
    edge)  shares_of[$c]="$EDGE_CPU_SHARES" ;;
    *)     shares_of[$c]="" ;;  # control plane, unlabelled: the default weight
  esac
done
[ "$apps" -gt 0 ] || { echo "no node of '${CLUSTER}' reads tier=app: refusing to weight a cluster with no app tier"; exit 1; }

for c in "${nodes[@]}"; do
  [ -n "${shares_of[$c]}" ] && docker update --cpu-shares "${shares_of[$c]}" "$c" >/dev/null
  weight="$(docker exec "$c" cat /sys/fs/cgroup/cpu.weight)"
  printf '  cpu weight %-26s -> %5s%s\n' "$c" "$weight" "${shares_of[$c]:+ (shares ${shares_of[$c]})}"
  if [ "${shares_of[$c]}" = "$APP_CPU_SHARES" ] && [ "$APP_CPU_SHARES" -eq 262144 ] && [ "$weight" != "10000" ]; then
    echo "app node $c reads cpu.weight $weight, not 10000"; exit 1
  fi
done
