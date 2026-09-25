#!/usr/bin/env bash
# Cap the TOTAL memory the kind cluster may use, so it can never starve the WSL2
# host (a parallel `cargo` build + a leaking worker once OOM'd the host and
# killed an unrelated test; on a 16 GiB box the cluster + rust-analyzer + the
# observability stack also page-cache-thrashed the VHDX — see 2026-06-13). kind
# has no native per-node memory limit, so we `docker update` the node containers
# after the cluster exists.
#
# Caps are TIER-AWARE, not flat: the control-plane (etcd+apiserver) and the edge
# nodes (front proxy — a 512 Mi pod) are cheap, so they get small caps; only the
# app nodes (b2bua pool) and load nodes (sipp UAS 2-replica + co-located UAC
# generators) need real headroom. A flat per-node cap forced a bad trade — big
# enough for load = wasteful on edge/CP. Tier is read from the kube node label
# `tier` (set in cluster.yaml); if kubectl/labels aren't available yet we fall
# back to the conservative uniform WORKER_CAP_MB.
#
# Each cap is set with `--memory-swap == --memory` (a true ceiling, no swap
# growth), so the sum of per-node hard caps is the cluster's absolute cgroup
# ceiling. We refuse if that sum would exceed TOTAL_CAP_MB. Re-run after any
# `kind create` (run.sh `up` calls this). Pod-level limits (20-worker.yaml: 2Gi)
# still apply inside; this is the node-container backstop.
#
# App-node rule: APP_CAP_MB >= APP_POD_LIMIT_MB (the worker pod's memory limit)
# + APP_COTENANT_MB (the limits of the other pods an app node can hold) +
# NODE_OVERHEAD_MB (the node's own systemd/containerd/kubelet and DaemonSets).
# Below it the node cgroup OOM-kills the worker before its own pod limit applies;
# the script refuses such a cap.
#
# Override per-invocation:  TOTAL_CAP_MB=8192 EDGE_CAP_MB=1024 ./cap-kind-memory.sh
set -euo pipefail

CLUSTER="${CLUSTER:-sip-e2e}"
TOTAL_CAP_MB="${TOTAL_CAP_MB:-9728}"   # 9.5 GiB whole-cluster hard ceiling. Host is ~16 GiB
                                       # WSL2; leave room for rust-analyzer (~2.5 GiB), the
                                       # observability stack (~1.9 GiB) and the page cache.
CP_CAP_MB="${CP_CAP_MB:-1536}"         # control-plane reservation (etcd+apiserver ~1 GiB)
EDGE_CAP_MB="${EDGE_CAP_MB:-768}"      # edge node: front proxy pod is 512 Mi + kubelet
APP_POD_LIMIT_MB="${APP_POD_LIMIT_MB:-2048}"  # the worker pod's memory limit (20-worker.yaml 2Gi)
APP_COTENANT_MB="${APP_COTENANT_MB:-0}"       # summed limits of the other pods on one app node
NODE_OVERHEAD_MB="${NODE_OVERHEAD_MB:-512}"   # node system.slice ~160Mi + DaemonSets ~200Mi, measured
APP_CAP_MB="${APP_CAP_MB:-$(( APP_POD_LIMIT_MB + APP_COTENANT_MB + NODE_OVERHEAD_MB ))}"
LOAD_CAP_MB="${LOAD_CAP_MB:-1536}"     # load node: sipp UAS + co-located UAC generators
INFRA_CAP_MB="${INFRA_CAP_MB:-1536}"   # infra node: broker (1Gi limit) + limiter + CDR consumer
WORKER_CAP_MB="${WORKER_CAP_MB:-1280}" # fallback when a worker's tier label is unknown

app_floor=$(( APP_POD_LIMIT_MB + APP_COTENANT_MB + NODE_OVERHEAD_MB ))
[ "$APP_CAP_MB" -lt "$app_floor" ] && {
  echo "APP_CAP_MB ${APP_CAP_MB}m is below worker limit ${APP_POD_LIMIT_MB}m + co-tenants ${APP_COTENANT_MB}m + node overhead ${NODE_OVERHEAD_MB}m = ${app_floor}m: the node would OOM-kill the worker before its pod limit"; exit 1; }

# -a: cap stopped nodes too (their config applies on next start).
mapfile -t nodes < <(docker ps -a --filter "name=${CLUSTER}" --format '{{.Names}}')
[ "${#nodes[@]}" -eq 0 ] && { echo "no kind nodes for cluster '${CLUSTER}' — nothing to cap"; exit 0; }

# Tier of a kind node from its kube label (kind node name == container name).
# Empty if kubectl is missing, the API isn't up yet, or the node has no label.
node_tier() {
  command -v kubectl >/dev/null 2>&1 || return 0
  kubectl get node "$1" -o jsonpath='{.metadata.labels.tier}' 2>/dev/null || true
}

# Pick the cap for a worker node from its tier, with a uniform fallback.
worker_cap_for() {  # name -> echoes capMB
  case "$(node_tier "$1")" in
    edge) echo "$EDGE_CAP_MB" ;;
    app)  echo "$APP_CAP_MB"  ;;
    load) echo "$LOAD_CAP_MB" ;;
    infra) echo "$INFRA_CAP_MB" ;;
    *)    echo "$WORKER_CAP_MB" ;;  # unlabelled / kubectl not ready
  esac
}

cps=(); workers=()
for c in "${nodes[@]}"; do
  case "$c" in
    *control-plane) cps+=("$c") ;;
    *)              workers+=("$c") ;;
  esac
done

# Pre-compute every node's cap so we can guard the host BEFORE mutating anything:
# refuse if the resulting whole-cluster ceiling would exceed TOTAL_CAP_MB (raise
# TOTAL_CAP_MB, or lower a per-tier cap, to proceed).
declare -A cap_of
projected=0
for c in "${cps[@]}";     do cap_of[$c]="$CP_CAP_MB";              projected=$(( projected + CP_CAP_MB )); done
for c in "${workers[@]}"; do cap_of[$c]="$(worker_cap_for "$c")"; projected=$(( projected + cap_of[$c] )); done
[ "$projected" -gt "$TOTAL_CAP_MB" ] && {
  echo "projected cluster cap ${projected}m exceeds TOTAL_CAP_MB ${TOTAL_CAP_MB}m — raise TOTAL_CAP_MB or lower a per-tier cap"; exit 1; }

cap_node() {  # name capMB
  docker update --memory "${2}m" --memory-swap "${2}m" "$1" >/dev/null
  printf '  capped %-26s -> %4sm\n' "$1" "$2"
}

total=0
for c in "${cps[@]}" "${workers[@]}"; do cap_node "$c" "${cap_of[$c]}"; total=$(( total + cap_of[$c] )); done

printf '  kind total ceiling = %sm (~%s.%02d GiB) <= budget %sm\n' \
  "$total" "$(( total / 1024 ))" "$(( (total % 1024) * 100 / 1024 ))" "$TOTAL_CAP_MB"
