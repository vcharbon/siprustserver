# shellcheck shell=bash disable=SC2016 # the $ of a go-template is never the shell's
# The image roll of the kind lab (kind-lab.sh deploy, after the SUT release):
# `image` reloads a tag in place, so a pod template naming that tag does not
# change and helm rolls nothing. Every Deployment, StatefulSet and DaemonSet
# of NS with a live pod whose container runs an image other than the one its
# tag names on the pod's node is restarted, then waited for. Sourced after
# lib/cluster.sh (log, die, kubectl on the lab's context); reads NS and
# ROLLOUT_TIMEOUT. A kind node is the docker container of the node's name.

# One line per started container, init containers included, of each pod of
# NS not being deleted: <owner kind> <owner name> <pod> <node> <spec image>
# <imageID>. A pod with no controller has owner `- -`.
image_roll_containers() {
  local row='{{$owner}} {{$p.metadata.name}} {{$p.spec.nodeName}} {{$image}} {{.imageID}}{{"\n"}}'
  kubectl -n "$NS" get pods -o go-template='{{range .items}}{{if not .metadata.deletionTimestamp}}{{$p := .}}{{$owner := "- -"}}{{range .metadata.ownerReferences}}{{if .controller}}{{$owner = printf "%s %s" .kind .name}}{{end}}{{end}}{{range .status.initContainerStatuses}}{{$n := .name}}{{$image := ""}}{{range $p.spec.initContainers}}{{if eq .name $n}}{{$image = .image}}{{end}}{{end}}{{if .imageID}}'"$row"'{{end}}{{end}}{{range .status.containerStatuses}}{{$n := .name}}{{$image := ""}}{{range $p.spec.containers}}{{if eq .name $n}}{{$image = .image}}{{end}}{{end}}{{if .imageID}}'"$row"'{{end}}{{end}}{{end}}{{end}}'
}

# One line per ReplicaSet of NS a Deployment controls: <replicaset> <deployment>.
image_roll_replicasets() {
  kubectl -n "$NS" get replicasets -o go-template='{{range .items}}{{$rs := .metadata.name}}{{range .metadata.ownerReferences}}{{if and .controller (eq .kind "Deployment")}}{{$rs}} {{.name}}{{"\n"}}{{end}}{{end}}{{end}}'
}

# One line per object of a kind with an updateStrategy (statefulsets,
# daemonsets): <name> <strategy type> <partition, 0 when unset>.
image_roll_strategies() { # <resource>
  kubectl -n "$NS" get "$1" -o go-template='{{range .items}}{{.metadata.name}} {{with .spec.updateStrategy}}{{or .type "RollingUpdate"}} {{with .rollingUpdate}}{{or .partition 0}}{{else}}0{{end}}{{else}}RollingUpdate 0{{end}}{{"\n"}}{{end}}'
}

# The digests a kind node's image of that name answers to, one line: its image
# id and each repo digest, `sha256:<hex>` each. Fails when the node holds no
# image of that name.
image_roll_node_digests() { # <node> <image>
  local out entry digests=()
  out="$(docker exec "$1" crictl inspecti -o go-template \
    --template '{{.status.id}}{{range .status.repoDigests}} {{.}}{{end}}' "$2" 2>/dev/null)" || return 1
  for entry in $out; do digests+=("${entry##*@}"); done
  [ "${#digests[@]}" -gt 0 ] || return 1
  printf '%s\n' "${digests[*]}"
}

# Restarts every workload of NS with a pod on a replaced image and waits for
# each rollout. A StatefulSet's pods below its partition are not read (the
# rollout leaves them); a workload updated OnDelete is named and left.
roll_stale_images() {
  local rows name owner kind pod node image id wl type part ordinal key digest
  local -A deployment=() strategy=() partition=() held=() stale=()
  rows="$(image_roll_replicasets)" || die "image roll: cannot list the ReplicaSets of $NS"
  while read -r name owner; do
    [ -z "$name" ] || deployment[$name]="$owner"
  done <<< "$rows"
  for kind in statefulset daemonset; do
    rows="$(image_roll_strategies "${kind}s")" || die "image roll: cannot list the ${kind}s of $NS"
    while read -r name type part; do
      [ -n "$name" ] || continue
      strategy[$kind/$name]="$type"
      partition[$kind/$name]="$part"
    done <<< "$rows"
  done
  rows="$(image_roll_containers)" || die "image roll: cannot list the pods of $NS"
  while read -r kind owner pod node image id; do
    [ -n "$id" ] || continue
    case "$kind" in
      ReplicaSet) [ -n "${deployment[$owner]:-}" ] || continue; wl="deployment/${deployment[$owner]}" ;;
      StatefulSet) wl="statefulset/$owner" ;;
      DaemonSet) wl="daemonset/$owner" ;;
      *) continue ;;
    esac
    if [ "$kind" = StatefulSet ]; then
      ordinal="${pod##*-}"
      [[ "$ordinal" =~ ^[0-9]+$ ]] && [ "$ordinal" -ge "${partition[$wl]:-0}" ] || continue
    fi
    key="$node $image"
    if [ -z "${held[$key]+set}" ]; then
      held[$key]="$(image_roll_node_digests "$node" "$image")" \
        || { log "image roll: $node holds no $image; $pod left on its image"; held[$key]=""; }
    fi
    [ -n "${held[$key]}" ] || continue
    digest="${id##*@}"
    case " ${held[$key]} " in *" $digest "*) continue ;; esac
    stale[$wl]+=" $pod ($image)"
  done <<< "$rows"

  local workloads=() rolled=()
  [ "${#stale[@]}" -eq 0 ] || mapfile -t workloads < <(printf '%s\n' "${!stale[@]}" | sort)
  for wl in "${workloads[@]}"; do
    if [ "${strategy[$wl]:-}" = OnDelete ]; then
      log "image roll: $wl is OnDelete, left on a replaced image:${stale[$wl]}"
      continue
    fi
    log "image roll: restarting $wl, on a replaced image:${stale[$wl]}"
    kubectl -n "$NS" rollout restart "$wl" >/dev/null || die "image roll: rollout restart $wl failed"
    rolled+=("$wl")
  done
  if [ "${#rolled[@]}" -eq 0 ]; then
    log "image roll: every pod runs the image its tag names on its node, nothing rolled"
    return 0
  fi
  for wl in "${rolled[@]}"; do
    kubectl -n "$NS" rollout status "$wl" --timeout "$ROLLOUT_TIMEOUT" >/dev/null \
      || die "image roll: $wl did not finish its rollout in $ROLLOUT_TIMEOUT (kubectl -n $NS rollout status $wl)"
  done
  log "image roll: rolled ${rolled[*]}"
}
