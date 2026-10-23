# shellcheck shell=bash
# The helpers the base incident executables source (ADR-0049 §2.3, §2.4): kubectl
# pinned to the lab's context, the event's target pods kept in its event
# directory, facts as one JSON object on stdout. The pod and node fault
# mechanisms are pod-faults.sh's; chaos-mesh.sh drives Chaos Mesh resources.
#
# Every executable runs with `set -euo pipefail` and inherit_errexit: a command
# that fails ends the step, inside a command substitution too. Bash suspends
# errexit inside a condition (`if`, `||`, `&&`), so every helper checks its own
# reads (`|| return 1`). A step concludes only from a positive answer: a read
# that fails is a failure (or, in a probe, not yet), never an empty answer.
# HK_RECHECK_S (1) is the heals' re-check delay and HK_CRASH_WAIT_S (10) the
# bound of a crash's proof of death; tests shorten them.
set -euo pipefail
shopt -s inherit_errexit

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
# The number of non-empty lines on stdin.
hk_count() { awk 'NF { n++ } END { print n + 0 }'; }
# The lines of an event-directory file; nothing when the file is absent (the
# step that writes it never ran).
hk_state() { if [ -f "$HAKIT_EVENT_DIR/$1" ]; then cat "$HAKIT_EVENT_DIR/$1"; fi; }

# "name uid node" of every live pod of a selector (the target's by default):
# Ready and not being deleted.
hk_live_pods() { # [selector]
  local rows
  rows="$(kubectl -n "$NS" get pods -l "${1:-$HAKIT_TARGET_SELECTOR}" -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.metadata.uid}{" "}{.spec.nodeName}{" "}{.status.conditions[?(@.type=="Ready")].status}{" "}{.metadata.deletionTimestamp}{"\n"}{end}')" || return 1
  awk 'NF == 4 && $4 == "True" { print $1 " " $2 " " $3 }' <<<"$rows"
}

# Pick the event's target pods (one at random, or all) into `targets`, and the
# number of live pods into `live_before`; refuses when none is live.
hk_pick_targets() {
  local live
  live="$(hk_live_pods)" || hk_die "the pods of $HAKIT_TARGET_SELECTOR cannot be read"
  [ -n "$live" ] || hk_die "no live pod matches $HAKIT_TARGET_SELECTOR in $NS"
  if [ "$HAKIT_TARGET_PICK" = all ]; then
    printf '%s\n' "$live" > "$HAKIT_EVENT_DIR/targets"
  else
    shuf -n 1 <<<"$live" > "$HAKIT_EVENT_DIR/targets"
  fi
  hk_count <<<"$live" > "$HAKIT_EVENT_DIR/live_before"
}
# "name uid node" per target, as inject picked them (nothing before inject).
hk_targets() { hk_state targets; }
hk_target_pods_json() { hk_targets | awk '{ print $1 }' | jq -Rnc '[inputs | select(length > 0)]'; }

# The current uid of a pod, empty when it does not exist.
hk_uid() { kubectl -n "$NS" get pod "$1" --ignore-not-found -o jsonpath='{.metadata.uid}'; }

# Delete every target pod: grace 0 is an API force-delete (the processes are
# left to their kubelet), a negative grace the pod's own grace period, else
# that many seconds. Facts: the pods, their uids, the live pods before, the
# delete instant.
hk_delete_targets() { # grace_s
  local grace="$1" pod at targets
  targets="$(hk_targets)"
  at="$(hk_now_ms)"
  while read -r pod _ _; do
    [ -n "$pod" ] || continue
    if [ "$grace" -eq 0 ]; then
      kubectl -n "$NS" delete pod "$pod" --grace-period=0 --force --wait=false >&2
    elif [ "$grace" -lt 0 ]; then
      kubectl -n "$NS" delete pod "$pod" --wait=false >&2
    else
      kubectl -n "$NS" delete pod "$pod" --grace-period="$grace" --wait=false >&2
    fi
  done <<<"$targets"
  echo "$at" > "$HAKIT_EVENT_DIR/killed_at_ms"
  jq -cn --argjson pods "$(hk_target_pods_json)" --argjson uids "$(awk '{ print $2 }' <<<"$targets" | jq -Rnc '[inputs | select(length > 0)]')" \
    --argjson before "$(cat "$HAKIT_EVENT_DIR/live_before")" --argjson at "$at" \
    '{pods: $pods, uids: $uids, live_before: $before, deleted_at_ms: $at}'
}

# Probe after a delete: not yet while a deleted pod (by uid) still exists or
# fewer pods are live than before, or while the pods cannot be read. Done:
# each replacement that kept its name (a StatefulSet's), its earliest
# container start and its Ready instant.
hk_probe_replaced() {
  local rows live n before pod uid rep ruid ready_t started_t facts="[]" targets
  rows="$(kubectl -n "$NS" get pods -l "$HAKIT_TARGET_SELECTOR" -o json)" || exit "$HK_NOT_YET"
  targets="$(hk_targets)"
  while read -r pod uid _; do
    [ -n "$uid" ] || continue
    if jq -e --arg u "$uid" 'any(.items[]; .metadata.uid == $u)' <<<"$rows" >/dev/null; then exit "$HK_NOT_YET"; fi
  done <<<"$targets"
  live="$(jq -r '.items[] | select(.metadata.deletionTimestamp == null)
      | select(any(.status.conditions[]?; .type == "Ready" and .status == "True"))
      | "\(.metadata.name) \(.metadata.uid) \(.status.conditions[] | select(.type == "Ready") | .lastTransitionTime) \([.status.containerStatuses[]?.state.running.startedAt // empty] | min // "")"' <<<"$rows")"
  n="$(hk_count <<<"$live")"
  before="$(cat "$HAKIT_EVENT_DIR/live_before")"
  [ "$n" -ge "$before" ] || exit "$HK_NOT_YET"
  while read -r pod _ _; do
    [ -n "$pod" ] || continue
    rep="$(awk -v p="$pod" '$1 == p' <<<"$live")"
    [ -n "$rep" ] || continue
    read -r _ ruid ready_t started_t <<<"$rep"
    facts="$(jq -c --arg pod "$pod" --arg uid "$ruid" --argjson ready "$(hk_ms "$ready_t")" \
      --argjson started "$(hk_ms "$started_t")" '. + [{pod: $pod, uid: $uid, started_at_ms: $started, ready_at_ms: $ready}]' <<<"$facts")"
  done <<<"$targets"
  jq -cn --argjson r "$facts" --argjson live "$n" '{replacements: $r, live_pods: $live}'
}

# The summed container restart count of a pod; empty when it does not exist.
hk_restarts() { # pod
  local rcs
  rcs="$(kubectl -n "$NS" get pod "$1" --ignore-not-found -o jsonpath='{range .status.containerStatuses[*]}{.restartCount}{"\n"}{end}')" || return 1
  awk 'NF { n += $1; seen = 1 } END { if (seen) print n }' <<<"$rcs"
}
