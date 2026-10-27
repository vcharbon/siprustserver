#!/usr/bin/env bash
# Weight the CPU of each kind node container by tier, so the SUT's CPU limit
# holds as a share on a shared host. Every node container sits in the same
# parent cgroup with the same default weight, so under host contention the app
# node gets no more than any other node: its pods wait for CPU while their own
# cgroup is far below its limit. The app tier gets the largest weight, infra
# and edge (the SUT's dependencies and the load generators) a smaller one, the
# control plane keeps the default. A weight is a share under contention, never
# a reservation: an idle node takes nothing from anyone.
#
# The weight is set with `docker update --cpu-shares` (cgroup v2 maps the
# shares to `cpu.weight`); it persists on the node container and is re-applied
# on a live cluster by re-running this script. Tier is read from the kube node
# label `tier` (set in cluster.yaml), as cap-kind-memory.sh reads it.
#
# Override per-invocation:  APP_CPU_WEIGHT=5000 ./weight-kind-cpu.sh
set -euo pipefail

CLUSTER="${CLUSTER:-sip-e2e}"
APP_CPU_WEIGHT="${APP_CPU_WEIGHT:-10000}"    # app nodes: the SUT pods
INFRA_CPU_WEIGHT="${INFRA_CPU_WEIGHT:-1000}" # infra node: broker, limiter, CDR consumer
EDGE_CPU_WEIGHT="${EDGE_CPU_WEIGHT:-1000}"   # edge nodes: front proxy, pod-placed load generators
OTHER_CPU_WEIGHT="${OTHER_CPU_WEIGHT:-100}"  # control plane and unlabelled nodes: the default

for w in "$APP_CPU_WEIGHT" "$INFRA_CPU_WEIGHT" "$EDGE_CPU_WEIGHT" "$OTHER_CPU_WEIGHT"; do
  [[ "$w" =~ ^[0-9]+$ ]] && [ "$w" -ge 1 ] && [ "$w" -le 10000 ] || {
    echo "CPU weight '$w' is not in 1..10000 (cgroup v2 cpu.weight)"; exit 1; }
done

mapfile -t nodes < <(docker ps -a --filter "name=${CLUSTER}" --format '{{.Names}}')
[ "${#nodes[@]}" -eq 0 ] && { echo "no kind nodes for cluster '${CLUSTER}' — nothing to weight"; exit 0; }

# Tier of a kind node from its kube label (kind node name == container name).
node_tier() {
  command -v kubectl >/dev/null 2>&1 || return 0
  kubectl get node "$1" -o jsonpath='{.metadata.labels.tier}' 2>/dev/null || true
}

weight_for() {  # name -> echoes weight
  case "$(node_tier "$1")" in
    app)   echo "$APP_CPU_WEIGHT" ;;
    infra) echo "$INFRA_CPU_WEIGHT" ;;
    edge)  echo "$EDGE_CPU_WEIGHT" ;;
    *)     echo "$OTHER_CPU_WEIGHT" ;;
  esac
}

# The cpu-shares docker maps to `weight` on cgroup v2, the inverse of
# weight = 1 + (shares - 2) * 9999 / 262142, rounded up so it maps back exactly.
shares_for() {  # weight -> echoes shares
  echo $(( 2 + (($1 - 1) * 262142 + 9998) / 9999 ))
}

for c in "${nodes[@]}"; do
  w="$(weight_for "$c")"
  docker update --cpu-shares "$(shares_for "$w")" "$c" >/dev/null
  printf '  cpu weight %-26s -> %5s\n' "$c" "$w"
done
