# shellcheck shell=bash
# Chaos Mesh resources of an incident (sourced after incident.sh): one per
# event and incident in the target namespace, named
# `hakit-<sha1(run/event/incident)[:10]>-<incident>` and created with
# `kubectl create`, so a name already taken is refused. Its AllInjected
# condition is the applied proof; its deletion through Chaos Mesh's finalizer
# the undone proof. Chaos Mesh refuses host-network pods.

hk_cm_name() {
  local h p
  h="$(printf '%s/%s/%s' "$HAKIT_RUN" "$HAKIT_EVENT" "$HAKIT_INCIDENT" | sha1sum)"
  p="$(printf '%s' "$HAKIT_INCIDENT" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9-]/-/g')"
  printf 'hakit-%s-%s' "${h:0:10}" "$p" | cut -c1-63 | sed 's/-*$//'
}

# A label selector of equality terms (`a=b,c=d`) as a JSON map.
hk_cm_labels() { # selector
  local term out="{}" terms
  IFS=, read -ra terms <<<"$1"
  for term in "${terms[@]}"; do
    [[ "$term" =~ ^([A-Za-z0-9./_-]+)=([A-Za-z0-9._-]*)$ ]] || hk_die "selector term '$term' is not an equality Chaos Mesh can select on"
    out="$(jq -c --arg k "${BASH_REMATCH[1]}" --arg v "${BASH_REMATCH[2]}" '. + {($k): $v}' <<<"$out")"
  done
  echo "$out"
}

# The Chaos Mesh pod selector of a label selector in the target namespace.
hk_cm_selector() { # selector
  jq -cn --arg ns "$NS" --argjson l "$(hk_cm_labels "$1")" '{namespaces: [$ns], labelSelectors: $l}'
}

# The Chaos Mesh selector of named pods in the target namespace.
hk_cm_pods_selector() { # pod…
  printf '%s\n' "$@" | jq -Rnc --arg ns "$NS" '{pods: {($ns): [inputs | select(length > 0)]}}'
}

# Chaos Mesh's mode for the target pick.
hk_cm_mode() { if [ "$HAKIT_TARGET_PICK" = all ]; then echo all; else echo one; fi; }

# Refuse a selector holding host-network pods: Chaos Mesh would leave the
# resource uninjected.
hk_cm_refuse_host_network() { # selector
  local pods n
  pods="$(kubectl -n "$NS" get pods -l "$1" -o json)" || hk_die "the pods of $1 cannot be read"
  n="$(jq '[.items[] | select(.spec.hostNetwork == true)] | length' <<<"$pods")"
  [ "$n" -eq 0 ] || hk_die "$1 selects host-network pods, which Chaos Mesh refuses"
}

# Create the resource read on stdin (its metadata is set here) and wait until
# it reports AllInjected; on a refusal or the timeout, print its records and
# fail. Prints the injected instant (epoch ms).
hk_cm_apply() { # kind  timeout_s
  local kind="$1" timeout_s="$2" name deadline st msg doc
  name="$(hk_cm_name)"
  doc="$(jq -c --arg n "$name" --arg ns "$NS" --arg run "$HAKIT_RUN" \
    '.metadata = {name: $n, namespace: $ns, labels: {"hakit.io/run": $run}}')"
  kubectl create -f - >&2 <<<"$doc"
  deadline=$(( $(date +%s) + timeout_s ))
  while :; do
    st="$(kubectl -n "$NS" get "$kind" "$name" -o jsonpath='{.status.conditions[?(@.type=="AllInjected")].status}')" || st=unreadable
    if [ "$st" = True ]; then hk_now_ms; return 0; fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      msg="$(kubectl -n "$NS" get "$kind" "$name" -o jsonpath='{range .status.experiment.containerRecords[*]}{.id}{": "}{.phase}{" "}{.events[-1:].message}{"\n"}{end}')" || msg="unreadable"
      echo "$HAKIT_INCIDENT: $kind $name not injected within ${timeout_s}s: ${msg:-no status}" >&2
      return 1
    fi
    sleep 0.2
  done
}

# The pods a resource injected, as a JSON array.
hk_cm_pods() { # kind
  local doc
  doc="$(kubectl -n "$NS" get "$1" "$(hk_cm_name)" -o json)" || return 1
  jq -c '[.status.experiment.containerRecords[]? | select(.selectorKey == ".") | .id | split("/")[1]] | unique' <<<"$doc"
}

# Whether this event's resource exists: `present` or `absent`; fails when the
# API cannot answer.
hk_cm_presence() { # kind
  local out
  out="$(kubectl -n "$NS" get "$1" "$(hk_cm_name)" --ignore-not-found -o name)" || return 1
  if [ -n "$out" ]; then echo present; else echo absent; fi
}

# Delete this event's resource and wait until it is gone: Chaos Mesh's
# finalizer recovers every injected pod first. A resource that injected
# nothing and is stuck on its finalizer has nothing to recover: its finalizer
# is dropped. Proven absent, and again a second later. Prints the instant.
hk_cm_delete() { # kind  timeout_s
  local kind="$1" timeout_s="$2" name doc injected at presence
  name="$(hk_cm_name)"
  if ! kubectl -n "$NS" delete "$kind" "$name" --ignore-not-found --wait=true --timeout="${timeout_s}s" >&2; then
    doc="$(kubectl -n "$NS" get "$kind" "$name" -o json)"
    injected="$(jq '[.status.experiment.containerRecords[]? | select(.phase == "Injected")] | length' <<<"$doc")"
    [ "$injected" -eq 0 ] || hk_die "$kind $name not recovered within ${timeout_s}s"
    kubectl -n "$NS" patch "$kind" "$name" --type=merge -p '{"metadata":{"finalizers":[]}}' >&2
  fi
  presence="$(hk_cm_presence "$kind")" || hk_die "$kind $name cannot be read after its deletion"
  [ "$presence" = absent ] || hk_die "$kind $name still exists after its deletion"
  at="$(hk_now_ms)"
  sleep "${HK_RECHECK_S:-1}"
  presence="$(hk_cm_presence "$kind")" || hk_die "$kind $name cannot be read a second after its deletion"
  [ "$presence" = absent ] || hk_die "$kind $name exists again a second after its deletion"
  echo "$at"
}
