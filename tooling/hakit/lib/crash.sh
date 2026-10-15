# shellcheck shell=bash
# A crash of the target pods' processes (sourced after incident.sh): every
# container's init process is sent SIGKILL from its node, and each is proven
# dead before inject returns. The kubelet then restarts the containers in
# place, or the workload replaces the pod. A crash leaves no two incarnations
# alive at once: the probe measures the overlap between the elder's last log
# line and the restarted container's first, and fails on any.
#
# State in the event directory: `crashed` ("pod uid node container cid pid
# restarts" a line) and `killed_at_ms`.

# Record every container of the target pods: its id, host pid and restarts.
hk_crash_prepare() {
  local pod uid node cname cid rc pid
  : > "$HAKIT_EVENT_DIR/crashed"
  while read -r pod uid node; do
    while read -r cname cid rc; do
      [ -n "$cname" ] || continue
      cid="${cid#*://}"
      pid="$(docker exec "$node" crictl inspect --output go-template --template '{{.info.pid}}' "$cid" 2>/dev/null || true)"
      [[ "$pid" =~ ^[1-9][0-9]*$ ]] || hk_die "no running process for $pod/$cname"
      echo "$pod $uid $node $cname $cid $pid $rc" >> "$HAKIT_EVENT_DIR/crashed"
    done < <(kubectl -n "$NS" get pod "$pod" -o jsonpath='{range .status.containerStatuses[*]}{.name}{" "}{.containerID}{" "}{.restartCount}{"\n"}{end}')
  done < <(hk_targets)
  [ -s "$HAKIT_EVENT_DIR/crashed" ] || hk_die "no container to crash"
}

# SIGKILL every recorded process, then wait (at most HK_CRASH_WAIT_S, 10 s)
# until each is gone from its node. Facts: the pods, their containers, the
# signal instant and the instant every process was proven dead.
hk_crash_kill() {
  local wait_s="${HK_CRASH_WAIT_S:-10}" pod node cname pid at deadline dead
  at="$(hk_now_ms)"
  while read -r _ _ node _ _ pid _; do
    docker exec "$node" kill -9 "$pid" >&2 || true
  done < "$HAKIT_EVENT_DIR/crashed"
  echo "$at" > "$HAKIT_EVENT_DIR/killed_at_ms"
  deadline=$(( $(date +%s) + wait_s ))
  while read -r pod _ node cname _ pid _; do
    while docker exec "$node" test -d "/proc/$pid"; do
      [ "$(date +%s)" -lt "$deadline" ] || hk_die "$pod/$cname (pid $pid on $node) is still alive ${wait_s}s after SIGKILL"
      sleep 0.1
    done
  done < "$HAKIT_EVENT_DIR/crashed"
  dead="$(hk_now_ms)"
  jq -cn --argjson pods "$(hk_target_pods_json)" --argjson at "$at" --argjson dead "$dead" \
    --argjson before "$(cat "$HAKIT_EVENT_DIR/live_before")" \
    --argjson c "$(awk '{ print $1 "/" $4 }' "$HAKIT_EVENT_DIR/crashed" | jq -Rnc '[inputs]')" \
    '{pods: $pods, containers: $c, live_before: $before, killed_at_ms: $at, dead_at_ms: $dead}'
}

# The instant (epoch ms) of the first or last line of a container's log, by
# the runtime's own timestamps; empty when there is none.
hk_log_edge() { # pod container first|last [--previous]
  local line
  if [ "$3" = first ]; then
    line="$(kubectl -n "$NS" logs "$1" -c "$2" ${4:+"$4"} --timestamps 2>/dev/null | head -n 1 || true)"
  else
    line="$(kubectl -n "$NS" logs "$1" -c "$2" ${4:+"$4"} --timestamps --tail=1 2>/dev/null || true)"
  fi
  [ -n "$line" ] || return 0
  date -d "${line%% *}" +%s%3N
}

# Probe after a crash: not yet while a crashed pod (same uid) has a container
# that has not restarted or the pod is not Ready, or, for a replaced pod, while
# fewer pods are live than before. Done: per container its restart and Ready
# instants and the incarnation overlap, which must be 0.
hk_probe_crashed() {
  local pod uid node cname rc json now_rc ready started last first overlap facts="[]"
  while read -r pod uid node cname _ _ rc; do
    json="$(kubectl -n "$NS" get pod "$pod" -o json 2>/dev/null || true)"
    if [ -z "$json" ] || [ "$(jq -r '.metadata.uid' <<<"$json")" != "$uid" ]; then
      [ "$(hk_live_pods | grep -c . || true)" -ge "$(cat "$HAKIT_EVENT_DIR/live_before")" ] || exit "$HK_NOT_YET"
      facts="$(jq -c --arg p "$pod" --arg c "$cname" '. + [{pod: $p, container: $c, replaced: true}]' <<<"$facts")"
      continue
    fi
    now_rc="$(jq -r --arg c "$cname" '.status.containerStatuses[] | select(.name == $c) | .restartCount' <<<"$json")"
    ready="$(jq -r '.status.conditions[]? | select(.type == "Ready" and .status == "True") | .lastTransitionTime' <<<"$json")"
    [ "${now_rc:-0}" -gt "$rc" ] && [ -n "$ready" ] || exit "$HK_NOT_YET"
    started="$(jq -r --arg c "$cname" '.status.containerStatuses[] | select(.name == $c) | .state.running.startedAt // empty' <<<"$json")"
    last="$(hk_log_edge "$pod" "$cname" last --previous)"
    first="$(hk_log_edge "$pod" "$cname" first)"
    overlap=null
    if [ -n "$last" ] && [ -n "$first" ]; then
      overlap=$(( last > first ? last - first : 0 ))
      [ "$overlap" -eq 0 ] || hk_die "$pod/$cname: the crashed incarnation logged ${overlap} ms after its successor started"
    fi
    facts="$(jq -c --arg p "$pod" --arg c "$cname" --argjson s "$(hk_ms "$started")" --argjson r "$(hk_ms "$ready")" \
      --argjson l "${last:-null}" --argjson f "${first:-null}" --argjson o "$overlap" \
      '. + [{pod: $p, container: $c, replaced: false, restarted_at_ms: $s, ready_at_ms: $r,
             elder_last_log_ms: $l, successor_first_log_ms: $f, overlap_ms: $o}]' <<<"$facts")"
  done < "$HAKIT_EVENT_DIR/crashed"
  jq -cn --argjson c "$facts" '{containers: $c}'
}
