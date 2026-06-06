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
TOTAL_CAP_MB="${TOTAL_CAP_MB:-12288}"  # 12 GiB whole-cluster hard ceiling (host safety;
                                       # WSL2 host is ~20 GiB so this leaves margin)
CP_CAP_MB="${CP_CAP_MB:-2048}"         # control-plane reservation (etcd/apiserver)
WORKER_CAP_MB="${WORKER_CAP_MB:-1536}" # per worker-NODE cap (1.5 GiB) — headroom for the
                                       # load nodes (UAS 2-replica + co-located UAC
                                       # generators + b2bua keepalive traffic). This is the
                                       # kind-NODE docker cgroup ceiling; pod-level limits
                                       # still apply inside.

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
# Cap each worker node at WORKER_CAP_MB directly (a predictable per-node target, not an
# even split of a leftover budget) so the load nodes get a stable 1.5 GiB regardless of
# node count. Guard the WSL2 host: refuse if the resulting whole-cluster ceiling would
# exceed TOTAL_CAP_MB (raise TOTAL_CAP_MB, or lower WORKER_CAP_MB, to proceed).
worker_cap="$WORKER_CAP_MB"
projected=$(( cp_total + worker_cap * ${#workers[@]} ))
[ "$projected" -gt "$TOTAL_CAP_MB" ] && {
  echo "projected cluster cap ${projected}m exceeds TOTAL_CAP_MB ${TOTAL_CAP_MB}m — raise TOTAL_CAP_MB or lower WORKER_CAP_MB"; exit 1; }

cap_node() {  # name capMB
  docker update --memory "${2}m" --memory-swap "${2}m" "$1" >/dev/null
  printf '  capped %-26s -> %4sm\n' "$1" "$2"
}

total=0
for c in "${cps[@]}";     do cap_node "$c" "$CP_CAP_MB";  total=$(( total + CP_CAP_MB ));  done
for c in "${workers[@]}"; do cap_node "$c" "$worker_cap"; total=$(( total + worker_cap )); done

printf '  kind total ceiling = %sm (~%s.%02d GiB) <= budget %sm\n' \
  "$total" "$(( total / 1024 ))" "$(( (total % 1024) * 100 / 1024 ))" "$TOTAL_CAP_MB"
