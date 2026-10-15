# shellcheck shell=bash
# A fault held on the target pods for the event's hold (sourced after
# incident.sh): inject applies it to every target and proves it in place,
# heal undoes it and proves it gone. The proof is pod-faults.sh's residue
# word (`netem`, `reject`, `frozen`). A target replaced meanwhile (a new uid)
# carries no fault: its netns and cgroup are new.

# The current uid of a pod, empty when it is gone.
hk_uid() { kubectl -n "$NS" get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null || true; }

# Apply `fn pod args…` to each target and check `word` in its residue. Facts:
# the pods, the applied instant, each pod's restart count before.
hk_timed_inject() { # word fn [args…]
  local word="$1" fn="$2" pod restarts="{}"
  shift 2
  hk_pick_targets
  while read -r pod _ _; do
    restarts="$(jq -c --arg p "$pod" --argjson n "$(hk_restarts "$pod" || echo null)" '. + {($p): $n}' <<<"$restarts")"
    "$fn" "$pod" "$@" >&2 || hk_die "$fn on $pod failed"
    grep -qx "$word" < <(pf_pod_residue "$pod") || hk_die "$word is not in place on $pod after $fn"
  done < <(hk_targets)
  echo "$restarts" > "$HAKIT_EVENT_DIR/restarts_before"
  jq -cn --argjson pods "$(hk_target_pods_json)" --argjson at "$(hk_now_ms)" --argjson r "$restarts" \
    '{pods: $pods, applied_at_ms: $at, restarts_before: $r}'
}

# Undo with `fn pod` on each target still the same pod and check `word` gone
# from its residue. Idempotent. Facts: per pod `healed`, `replaced` or `gone`,
# and its restarts during the fault (a liveness restart the fault caused).
hk_timed_heal() { # word fn
  local word="$1" fn="$2" pod uid now state facts="{}" before after
  while read -r pod uid _; do
    now="$(hk_uid "$pod")"
    if [ -z "$now" ]; then
      state=gone
    elif [ "$now" != "$uid" ]; then
      state=replaced
    else
      "$fn" "$pod" >&2 || true
      if grep -qx -e "$word" -e unreadable < <(pf_pod_residue "$pod"); then
        hk_die "$word is still on $pod after $fn"
      fi
      state=healed
    fi
    before="$(jq -r --arg p "$pod" '.[$p] // "null"' "$HAKIT_EVENT_DIR/restarts_before" 2>/dev/null || echo null)"
    after="$(hk_restarts "$pod" || true)"
    facts="$(jq -c --arg p "$pod" --arg s "$state" --argjson b "$before" --argjson a "${after:-null}" \
      '. + {($p): {state: $s, restarts: (if $a == null or $b == null then null else $a - $b end)}}' <<<"$facts")"
  done < <(hk_targets)
  jq -cn --argjson f "$facts" --argjson at "$(hk_now_ms)" '{pods: $f, healed_at_ms: $at}'
}
