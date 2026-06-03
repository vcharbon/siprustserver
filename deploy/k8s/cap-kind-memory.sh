#!/usr/bin/env bash
# Cap the TOTAL memory the kind cluster may use, so it can never starve the WSL2
# host (a parallel `cargo` build + a leaking worker once OOM'd the host and
# killed an unrelated test). kind has no native per-node memory limit, so we
# `docker update` the node containers after the cluster exists.
#
# We enforce a whole-cluster BUDGET (TOTAL_CAP_MB, default 9 GiB): the
# control-plane gets a fixed reservation (CP_CAP_MB) and the remainder is split
# evenly across the worker nodes. Because each cap is set with
# `--memory-swap == --memory` (a true ceiling, no swap growth) and we floor the
# per-worker share, sum(per-node hard caps) <= TOTAL_CAP_MB *by construction* —
# the cluster's cgroup memory ceiling can never exceed the budget regardless of
# node count. Re-run after any `kind create` (run.sh `up` calls this). Pod-level
# limits (20-worker.yaml: 1Gi) still apply inside; this is the node-container
# backstop.
#
# Override the budget per-invocation:  TOTAL_CAP_MB=8192 ./cap-kind-memory.sh
set -euo pipefail

CLUSTER="${CLUSTER:-sip-e2e}"
TOTAL_CAP_MB="${TOTAL_CAP_MB:-9216}"   # 9 GiB whole-cluster hard ceiling
CP_CAP_MB="${CP_CAP_MB:-2048}"         # control-plane reservation (etcd/apiserver)

# -a: cap stopped nodes too (their config applies on next start).
mapfile -t nodes < <(docker ps -a --filter "name=${CLUSTER}" --format '{{.Names}}')
[ "${#nodes[@]}" -eq 0 ] && { echo "no kind nodes for cluster '${CLUSTER}' — nothing to cap"; exit 0; }

cps=(); workers=()
for c in "${nodes[@]}"; do
  case "$c" in
    *control-plane) cps+=("$c") ;;
    *)              workers+=("$c") ;;
  esac
done

cp_total=$(( ${#cps[@]} * CP_CAP_MB ))
worker_budget=$(( TOTAL_CAP_MB - cp_total ))
[ "$worker_budget" -lt 0 ] && {
  echo "control-plane reservation (${cp_total}m) exceeds TOTAL_CAP_MB (${TOTAL_CAP_MB}m)"; exit 1; }
# Floor the per-worker share so the sum never rounds up over budget.
worker_cap=0
[ "${#workers[@]}" -gt 0 ] && worker_cap=$(( worker_budget / ${#workers[@]} ))

cap_node() {  # name capMB
  docker update --memory "${2}m" --memory-swap "${2}m" "$1" >/dev/null
  printf '  capped %-26s -> %4sm\n' "$1" "$2"
}

total=0
for c in "${cps[@]}";     do cap_node "$c" "$CP_CAP_MB";  total=$(( total + CP_CAP_MB ));  done
for c in "${workers[@]}"; do cap_node "$c" "$worker_cap"; total=$(( total + worker_cap )); done

printf '  kind total ceiling = %sm (~%s.%02d GiB) <= budget %sm\n' \
  "$total" "$(( total / 1024 ))" "$(( (total % 1024) * 100 / 1024 ))" "$TOTAL_CAP_MB"
