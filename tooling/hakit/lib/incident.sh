# shellcheck shell=bash
# The helpers the base incident executables source (ADR-0049 §2.3, §2.4): kubectl
# pinned to the lab's context, the event's target pods kept in its event
# directory, facts as one JSON object on stdout. The pod and node fault
# mechanisms are pod-faults.sh's; chaos-mesh.sh drives Chaos Mesh resources.
#
# Every executable runs with `set -euo pipefail`; a helper that cannot do its
# part prints why on stderr and exits non-zero (the step fails).
set -euo pipefail

NS="$HAKIT_TARGET_NAMESPACE"
kubectl() { command kubectl --context "$HAKIT_KUBE_CONTEXT" "$@"; }
# shellcheck source=pod-faults.sh
source "$HAKIT_LIB/pod-faults.sh"

# The exit code of a probe that is not done yet.
HK_NOT_YET=75

hk_die() { echo "$HAKIT_INCIDENT: $*" >&2; exit 1; }
hk_now_ms() { date +%s%3N; }
# An RFC 3339 instant in epoch ms, or `null` when empty.
hk_ms() { if [ -n "${1:-}" ]; then date -d "$1" +%s%3N; else echo null; fi; }

# "name uid node" of every live pod of a selector (the target's by default):
# Ready and not being deleted.
hk_live_pods() { # [selector]
  kubectl -n "$NS" get pods -l "${1:-$HAKIT_TARGET_SELECTOR}" -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.metadata.uid}{" "}{.spec.nodeName}{" "}{.status.conditions[?(@.type=="Ready")].status}{" "}{.metadata.deletionTimestamp}{"\n"}{end}' \
    | awk 'NF == 4 && $4 == "True" { print $1 " " $2 " " $3 }'
}

# Pick the event's target pods (one at random, or all) into `targets`, and the
# number of live pods into `live_before`; refuses when none is live.
hk_pick_targets() {
  local live
  live="$(hk_live_pods)"
  [ -n "$live" ] || hk_die "no live pod matches $HAKIT_TARGET_SELECTOR in $NS"
  if [ "$HAKIT_TARGET_PICK" = all ]; then
    printf '%s\n' "$live" > "$HAKIT_EVENT_DIR/targets"
  else
    shuf -n 1 <<<"$live" > "$HAKIT_EVENT_DIR/targets"
  fi
  grep -c . <<<"$live" > "$HAKIT_EVENT_DIR/live_before"
}
# "name uid node" per target, as inject picked them (empty before inject).
hk_targets() { cat "$HAKIT_EVENT_DIR/targets" 2>/dev/null || true; }
hk_target_pods_json() { hk_targets | awk '{ print $1 }' | jq -Rnc '[inputs | select(length > 0)]'; }

# Delete every target pod: grace 0 is a crash (forced), a negative grace the
# pod's own grace period, else that many seconds. Facts: the pods, their uids,
# the live pods before, the delete instant.
hk_delete_targets() { # grace_s
  local grace="$1" pod uid _node at
  at="$(hk_now_ms)"
  while read -r pod uid _node; do
    if [ "$grace" -eq 0 ]; then
      kubectl -n "$NS" delete pod "$pod" --grace-period=0 --force --wait=false >&2
    elif [ "$grace" -lt 0 ]; then
      kubectl -n "$NS" delete pod "$pod" --wait=false >&2
    else
      kubectl -n "$NS" delete pod "$pod" --grace-period="$grace" --wait=false >&2
    fi
  done < <(hk_targets)
  echo "$at" > "$HAKIT_EVENT_DIR/killed_at_ms"
  jq -cn --argjson pods "$(hk_target_pods_json)" --argjson uids "$(hk_targets | awk '{ print $2 }' | jq -Rnc '[inputs]')" \
    --argjson before "$(cat "$HAKIT_EVENT_DIR/live_before")" --argjson at "$at" \
    '{pods: $pods, uids: $uids, live_before: $before, deleted_at_ms: $at}'
}

# Probe after a delete: not yet while a deleted pod (by uid) still exists or
# fewer pods are live than before. Done: the facts of each replacement that
# kept its name (a StatefulSet's), its container start and its Ready instant.
hk_probe_replaced() {
  local rows live before pod uid _node rep facts="[]"
  rows="$(kubectl -n "$NS" get pods -l "$HAKIT_TARGET_SELECTOR" -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.metadata.uid}{" "}{.status.conditions[?(@.type=="Ready")].status}{" "}{.status.conditions[?(@.type=="Ready")].lastTransitionTime}{" "}{.status.containerStatuses[0].state.running.startedAt}{" "}{.metadata.deletionTimestamp}{"\n"}{end}')" || exit "$HK_NOT_YET"
  while read -r pod uid _node; do
    if awk -v u="$uid" '$2 == u { f = 1 } END { exit !f }' <<<"$rows"; then exit "$HK_NOT_YET"; fi
  done < <(hk_targets)
  live="$(awk 'NF == 5 && $3 == "True"' <<<"$rows")"
  before="$(cat "$HAKIT_EVENT_DIR/live_before")"
  [ "$(grep -c . <<<"$live" || true)" -ge "$before" ] || exit "$HK_NOT_YET"
  while read -r pod uid _node; do
    rep="$(awk -v p="$pod" '$1 == p' <<<"$live")"
    [ -n "$rep" ] || continue
    read -r _ ruid _ ready_t started_t <<<"$rep"
    facts="$(jq -c --arg pod "$pod" --arg uid "$ruid" --argjson ready "$(hk_ms "$ready_t")" \
      --argjson started "$(hk_ms "$started_t")" '. + [{pod: $pod, uid: $uid, started_at_ms: $started, ready_at_ms: $ready}]' <<<"$facts")"
  done < <(hk_targets)
  jq -cn --argjson r "$facts" --argjson live "$(grep -c . <<<"$live" || true)" '{replacements: $r, live_pods: $live}'
}

# The summed container restart count of a pod, or empty when it is gone.
hk_restarts() { # pod
  kubectl -n "$NS" get pod "$1" -o jsonpath='{range .status.containerStatuses[*]}{.restartCount}{"\n"}{end}' 2>/dev/null \
    | awk '{ n += $1 } END { if (NR) print n }'
}
