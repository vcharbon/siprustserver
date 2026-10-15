# shellcheck shell=bash
# Chaos Mesh resources of an incident (sourced after incident.sh): one per
# event and incident, named after both, in the target namespace. Its status
# is the applied proof (AllInjected), its deletion through Chaos Mesh's
# finalizer the undone proof. Chaos Mesh refuses host-network pods.

hk_cm_name() { printf 'hakit-%s-%s' "$HAKIT_EVENT" "$HAKIT_INCIDENT" | tr '_+A-Z' '--a-z' | cut -c1-63 | sed 's/-*$//'; }

# A label selector of equality terms (`a=b,c=d`) as a JSON map.
hk_cm_labels() { # selector
  local term out="{}"
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

# Chaos Mesh's mode for the target pick.
hk_cm_mode() { if [ "$HAKIT_TARGET_PICK" = all ]; then echo all; else echo one; fi; }

# Refuse a selector holding host-network pods: Chaos Mesh would leave the
# resource uninjected.
hk_cm_refuse_host_network() { # selector
  local n
  n="$(kubectl -n "$NS" get pods -l "$1" -o json | jq '[.items[] | select(.spec.hostNetwork == true)] | length')"
  [ "$n" -eq 0 ] || hk_die "$1 selects host-network pods, which Chaos Mesh refuses"
}

# Apply the resource read on stdin (its metadata is set here) and wait until
# it reports AllInjected; on a refusal or the timeout, print its records and
# fail. Prints the injected instant (epoch ms).
hk_cm_apply() { # kind  timeout_s
  local kind="$1" timeout_s="$2" name deadline st msg
  name="$(hk_cm_name)"
  jq -c --arg n "$name" --arg ns "$NS" --arg run "$HAKIT_RUN" \
    '.metadata = {name: $n, namespace: $ns, labels: {"hakit.io/run": $run}}' | kubectl apply -f - >&2
  deadline=$(( $(date +%s) + timeout_s ))
  while :; do
    st="$(kubectl -n "$NS" get "$kind" "$name" -o jsonpath='{.status.conditions[?(@.type=="AllInjected")].status}' 2>/dev/null || true)"
    [ "$st" = True ] && { hk_now_ms; return 0; }
    if [ "$(date +%s)" -ge "$deadline" ]; then
      msg="$(kubectl -n "$NS" get "$kind" "$name" -o jsonpath='{range .status.experiment.containerRecords[*]}{.id}{": "}{.phase}{" "}{.events[-1:].message}{"\n"}{end}' 2>/dev/null || true)"
      echo "$HAKIT_INCIDENT: $kind $name not injected within ${timeout_s}s: ${msg:-no status}" >&2
      return 1
    fi
    sleep 0.2
  done
}

# The pods a resource injected, as a JSON array.
hk_cm_pods() { # kind
  kubectl -n "$NS" get "$1" "$(hk_cm_name)" -o json \
    | jq -c '[.status.experiment.containerRecords[]? | select(.selectorKey == ".") | .id | split("/")[1]] | unique'
}

# Delete this event's resource and wait until it is gone: Chaos Mesh's
# finalizer recovers every injected pod first. A resource that injected
# nothing and is stuck on its finalizer has nothing to recover: its finalizer
# is dropped. Absent is done. Prints the instant.
hk_cm_delete() { # kind  timeout_s
  local kind="$1" timeout_s="$2" name injected
  name="$(hk_cm_name)"
  if ! kubectl -n "$NS" delete "$kind" "$name" --ignore-not-found --wait=true --timeout="${timeout_s}s" >&2; then
    injected="$(kubectl -n "$NS" get "$kind" "$name" -o json 2>/dev/null \
      | jq '[.status.experiment.containerRecords[]? | select(.phase == "Injected")] | length' || echo 0)"
    [ "${injected:-0}" -eq 0 ] || hk_die "$kind $name not recovered within ${timeout_s}s"
    kubectl -n "$NS" patch "$kind" "$name" --type=merge -p '{"metadata":{"finalizers":[]}}' >&2 || true
  fi
  hk_now_ms
}
