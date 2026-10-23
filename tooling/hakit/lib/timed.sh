# shellcheck shell=bash
# A fault held on the target pods for the event's hold (sourced after
# incident.sh): inject applies it to every target and proves it in place,
# heal undoes it and proves it gone, twice, the second time a second later
# (an inject killed on its timeout may still land). The proof is
# pod-faults.sh's residue word (`netem`, `reject`, `frozen`). A target that
# no longer exists, or exists under a new uid, carries no fault: its netns and
# cgroup are new.

# Refuse host-network targets of a netns fault unless the profile allows it:
# a host-network pod's netns is its node's, so the fault hits the whole node.
hk_refuse_host_network() { # allow (true|false)
  local pod n pods
  [ "$1" = true ] && return 0
  pods="$(hk_targets)"
  while read -r pod _ _; do
    [ -n "$pod" ] || continue
    n="$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.spec.hostNetwork}')" || hk_die "$pod cannot be read"
    [ "$n" != true ] || hk_die "$pod is a host-network pod: this fault would hit its whole node (set allow_host_network)"
  done <<<"$pods"
}

# Apply `fn pod args…` to each target and check `word` in its residue. Facts:
# the pods, the applied instant, each pod's restart count before.
hk_timed_inject() { # word fn [args…]
  local word="$1" fn="$2" pod restarts="{}" n residue targets
  shift 2
  targets="$(hk_targets)"
  while read -r pod _ _; do
    [ -n "$pod" ] || continue
    n="$(hk_restarts "$pod")" || hk_die "$pod cannot be read"
    restarts="$(jq -c --arg p "$pod" --argjson n "${n:-null}" '. + {($p): $n}' <<<"$restarts")"
    "$fn" "$pod" "$@" >&2 || hk_die "$fn on $pod failed"
    residue="$(pf_pod_residue "$pod")" || hk_die "the state of $pod cannot be read after $fn"
    grep -qx "$word" <<<"$residue" || hk_die "$word is not in place on $pod after $fn"
  done <<<"$targets"
  echo "$restarts" > "$HAKIT_EVENT_DIR/restarts_before"
  jq -cn --argjson pods "$(hk_target_pods_json)" --argjson at "$(hk_now_ms)" --argjson r "$restarts" \
    '{pods: $pods, applied_at_ms: $at, restarts_before: $r}'
}

# One pass of the heal over the targets: undo on each target still the same
# pod and prove `word` gone. Prints "pod state" lines.
hk_timed_undo() { # word fn
  local word="$1" fn="$2" pod uid now residue targets
  targets="$(hk_targets)"
  while read -r pod uid _; do
    [ -n "$pod" ] || continue
    now="$(hk_uid "$pod")" || hk_die "$pod cannot be read"
    if [ -z "$now" ]; then
      echo "$pod gone"
    elif [ "$now" != "$uid" ]; then
      echo "$pod replaced"
    else
      "$fn" "$pod" >&2 || hk_die "$fn on $pod failed"
      residue="$(pf_pod_residue "$pod")" || hk_die "the state of $pod cannot be read after $fn"
      if grep -qx "$word" <<<"$residue"; then hk_die "$word is still on $pod after $fn"; fi
      echo "$pod healed"
    fi
  done <<<"$targets"
}

# Undo with `fn pod` on each target and prove it gone; idempotent. Facts: per
# pod `healed`, `replaced` or `gone`, and its restarts during the fault (a
# liveness restart the fault caused).
hk_timed_heal() { # word fn
  local states pod state before after rb facts="{}"
  states="$(hk_timed_undo "$1" "$2")"
  sleep "${HK_RECHECK_S:-1}"
  hk_timed_undo "$1" "$2" >/dev/null
  rb="$(hk_state restarts_before)"
  [ -n "$rb" ] || rb='{}'
  while read -r pod state; do
    [ -n "$pod" ] || continue
    before="$(jq -r --arg p "$pod" '.[$p] // "null"' <<<"$rb")"
    after="$(hk_restarts "$pod")" || hk_die "$pod cannot be read"
    [ "$state" = healed ] || after=""
    facts="$(jq -c --arg p "$pod" --arg s "$state" --argjson b "$before" --argjson a "${after:-null}" \
      '. + {($p): {state: $s, restarts: (if $a == null or $b == null then null else $a - $b end)}}' <<<"$facts")"
  done <<<"$states"
  jq -cn --argjson f "$facts" --argjson at "$(hk_now_ms)" '{pods: $f, healed_at_ms: $at}'
}
