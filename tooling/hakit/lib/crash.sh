# shellcheck shell=bash
# A crash of the target pods' processes (sourced after incident.sh): every
# container's task gets SIGKILL from its node by container id (no pid that
# could be reused), and each is proven dead by its runtime state before
# inject returns. The kubelet then restarts the containers in place, or the
# workload replaces the pod. A crash leaves no two incarnations alive at once:
# the probe compares the crashed container's last log line with its
# successor's first and fails on any overlap.
#
# State in the event directory: `crashed` ("pod uid node container cid
# restarts" a line) and `killed_at_ms`.

# The runtime state of a container on its node: `running`, `exited`, or `gone`
# (the runtime no longer knows it). Fails when the state cannot be read.
hk_container_state() { # node cid
  local out rc
  out="$(docker exec "$1" crictl inspect --output go-template --template '{{.status.state}}' "$2" 2>&1)" && rc=0 || rc=$?
  if [ "$rc" -eq 0 ]; then
    case "$out" in
      CONTAINER_RUNNING) echo running ;;
      CONTAINER_EXITED) echo exited ;;
      *) echo "crash: container $2 on $1 is in state '$out'" >&2; return 1 ;;
    esac
  elif grep -qi "notfound\|not found" <<<"$out"; then
    echo gone
  else
    echo "crash: the state of container $2 on $1 cannot be read: $out" >&2
    return 1
  fi
}

# Record every container of the target pods: its id and restart count. Every
# one must be running.
hk_crash_prepare() {
  local pod uid node cname cid rc rows state targets
  : > "$HAKIT_EVENT_DIR/crashed"
  targets="$(hk_targets)"
  while read -r pod uid node; do
    [ -n "$pod" ] || continue
    rows="$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{range .status.containerStatuses[*]}{.name}{" "}{.containerID}{" "}{.restartCount}{"\n"}{end}')"
    while read -r cname cid rc; do
      [ -n "$cname" ] || continue
      cid="${cid#*://}"
      state="$(hk_container_state "$node" "$cid")"
      [ "$state" = running ] || hk_die "$pod/$cname is $state, not running"
      echo "$pod $uid $node $cname $cid $rc" >> "$HAKIT_EVENT_DIR/crashed"
    done <<<"$rows"
  done <<<"$targets"
  [ -s "$HAKIT_EVENT_DIR/crashed" ] || hk_die "no container to crash"
}

# SIGKILL every recorded container's task, then wait (at most
# HK_CRASH_WAIT_S, 10 s) until the runtime reports each exited or gone; a
# state that cannot be read is not a death. Facts: the pods, their
# containers, the signal instant and the instant every one was proven dead.
hk_crash_kill() {
  local wait_s="${HK_CRASH_WAIT_S:-10}" pod node cname cid at deadline dead state crashed
  crashed="$(cat "$HAKIT_EVENT_DIR/crashed")"
  at="$(hk_now_ms)"
  while read -r _ _ node _ cid _; do
    if ! docker exec "$node" ctr -n k8s.io task kill -s SIGKILL "$cid" >&2; then
      state="$(hk_container_state "$node" "$cid")"
      [ "$state" != running ] || hk_die "SIGKILL of container $cid on $node failed"
    fi
  done <<<"$crashed"
  echo "$at" > "$HAKIT_EVENT_DIR/killed_at_ms"
  deadline=$(( $(date +%s) + wait_s ))
  while read -r pod _ node cname cid _; do
    while :; do
      state="$(hk_container_state "$node" "$cid")" || state=unreadable
      case "$state" in exited | gone) break ;; esac
      [ "$(date +%s)" -lt "$deadline" ] || hk_die "$pod/$cname is $state ${wait_s}s after SIGKILL"
      sleep 0.1
    done
  done <<<"$crashed"
  dead="$(hk_now_ms)"
  jq -cn --argjson pods "$(hk_target_pods_json)" --argjson at "$at" --argjson dead "$dead" \
    --argjson before "$(cat "$HAKIT_EVENT_DIR/live_before")" \
    --argjson c "$(awk '{ print $1 "/" $4 }' <<<"$crashed" | jq -Rnc '[inputs]')" \
    '{pods: $pods, containers: $c, live_before: $before, killed_at_ms: $at, dead_at_ms: $dead}'
}

# The instant (epoch ms) of the first or last line of a container's log, by
# the runtime's own timestamps; empty when the log has no line.
hk_log_edge() { # pod container first|last [--previous]
  local out line
  if [ "$3" = first ]; then
    out="$(kubectl -n "$NS" logs "$1" -c "$2" ${4:+"$4"} --timestamps --limit-bytes=8192)" || return 1
    line="${out%%$'\n'*}"
  else
    out="$(kubectl -n "$NS" logs "$1" -c "$2" ${4:+"$4"} --timestamps --tail=1)" || return 1
    line="$out"
  fi
  [ -n "$line" ] || return 0
  date -d "${line%% *}" +%s%3N
}

# Probe after a crash: not yet while a crashed pod (same uid) has a container
# that has not restarted, is not Ready, or cannot be read; for a replaced pod,
# while fewer pods are live than before. Done: per container its restart and
# Ready instants and the incarnation overlap, which must be 0.
hk_probe_crashed() {
  local pod uid cname rc json now_rc ready started last first overlap n facts="[]" crashed
  crashed="$(cat "$HAKIT_EVENT_DIR/crashed")"
  while read -r pod uid _ cname _ rc; do
    json="$(kubectl -n "$NS" get pod "$pod" --ignore-not-found -o json)" || exit "$HK_NOT_YET"
    if [ -z "$json" ] || [ "$(jq -r '.metadata.uid' <<<"$json")" != "$uid" ]; then
      n="$(hk_live_pods | hk_count)" || exit "$HK_NOT_YET"
      [ "$n" -ge "$(cat "$HAKIT_EVENT_DIR/live_before")" ] || exit "$HK_NOT_YET"
      facts="$(jq -c --arg p "$pod" --arg c "$cname" '. + [{pod: $p, container: $c, replaced: true}]' <<<"$facts")"
      continue
    fi
    now_rc="$(jq -r --arg c "$cname" '.status.containerStatuses[] | select(.name == $c) | .restartCount' <<<"$json")"
    ready="$(jq -r '.status.conditions[]? | select(.type == "Ready" and .status == "True") | .lastTransitionTime' <<<"$json")"
    [ "${now_rc:-0}" -gt "$rc" ] && [ -n "$ready" ] || exit "$HK_NOT_YET"
    started="$(jq -r --arg c "$cname" '.status.containerStatuses[] | select(.name == $c) | .state.running.startedAt // empty' <<<"$json")"
    last="$(hk_log_edge "$pod" "$cname" last --previous)" || exit "$HK_NOT_YET"
    first="$(hk_log_edge "$pod" "$cname" first)" || exit "$HK_NOT_YET"
    overlap=null
    if [ -n "$last" ] && [ -n "$first" ]; then
      overlap=$(( last > first ? last - first : 0 ))
      [ "$overlap" -eq 0 ] || hk_die "$pod/$cname: the crashed incarnation logged ${overlap} ms after its successor started"
    fi
    facts="$(jq -c --arg p "$pod" --arg c "$cname" --argjson s "$(hk_ms "$started")" --argjson r "$(hk_ms "$ready")" \
      --argjson l "${last:-null}" --argjson f "${first:-null}" --argjson o "$overlap" \
      '. + [{pod: $p, container: $c, replaced: false, restarted_at_ms: $s, ready_at_ms: $r,
             elder_last_log_ms: $l, successor_first_log_ms: $f, overlap_ms: $o}]' <<<"$facts")"
  done <<<"$crashed"
  jq -cn --argjson c "$facts" '{containers: $c}'
}
