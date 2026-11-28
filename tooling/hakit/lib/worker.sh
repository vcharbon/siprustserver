# shellcheck shell=bash
# The restart timings of a killed B2BUA worker, read from the logs of the
# pods around it (sourced after incident.sh, in a probe): when each surviving
# worker read the killed one's replication peer not ready and when it parked
# it (a pod restarted in place stays a member and is never parked), and when
# each front proxy dropped it from its routing (it left the registry, or stays
# a member and reads not ready) and took it back (its probe reads it alive, or
# accepting new calls, again). Log lines are this repository's
# (`b2bua::repl::supervisor`, `sip_proxy::registry::projection`); a line not
# in the log leaves its instant null, a log that cannot be read is not yet.
# A proxy that dropped the worker and has not taken it back is not yet: its
# health flip can log after the pod reads Ready, and one that never logs
# fails the probe at its `probe_s`.

# The first log instant (epoch ms) of `pod`'s container at or after `since_ms`
# whose line matches the extended regexp; empty when no line does. A line's
# instant is its leading RFC 3339 UTC timestamp, compared to the millisecond.
hk_log_instant() { # pod container since_ms regexp
  local since from logs line
  since="$(date -u -d "@$(( $3 / 1000 ))" +%Y-%m-%dT%H:%M:%SZ)"
  from="${since%Z}.$(printf %03d $(( $3 % 1000 )))"
  logs="$(kubectl -n "$NS" logs "$1" ${2:+-c "$2"} --since-time="$since")" || exit "$HK_NOT_YET"
  line="$(awk -v re="$4" -v from="$from" 'substr($1, 1, 23) >= from && $0 ~ re { print; exit }' <<<"$logs")"
  [ -n "$line" ] || return 0
  date -d "${line%% *}" +%s%3N
}

# Facts of the worker `pod` killed at `killed_ms`: per survivor of the target
# selector its not-ready and park instants, per proxy its left and joined
# instants; not yet while a proxy has left and not joined.
hk_worker_timings() { # pod killed_ms
  local pod="$1" at="$2" p at_unready at_park left joined parked="[]" proxies="[]" pods
  local proxy_sel="${HAKIT_PARAM_PROXY_SELECTOR:-}" proxy_c="${HAKIT_PARAM_PROXY_CONTAINER:-}"
  pods="$(hk_live_pods)" || exit "$HK_NOT_YET"
  while read -r p _ _; do
    [ -n "$p" ] && [ "$p" != "$pod" ] || continue
    at_unready="$(hk_log_instant "$p" "" "$at" "replication peer kept on presence alone [(]not ready[)] .* peer=${pod}( |\$)")"
    at_park="$(hk_log_instant "$p" "" "$at" "replication peer parked .* peer=${pod}( |\$)")"
    parked="$(jq -c --arg p "$p" --argjson u "${at_unready:-null}" --argjson t "${at_park:-null}" \
      '. + [{pod: $p, not_ready_at_ms: $u, parked_at_ms: $t}]' <<<"$parked")"
  done <<<"$pods"
  if [ -n "$proxy_sel" ]; then
    pods="$(hk_live_pods "$proxy_sel")" || exit "$HK_NOT_YET"
    while read -r p _ _; do
      [ -n "$p" ] || continue
      left="$(hk_log_instant "$p" "$proxy_c" "$at" "worker (left the registry|reads not ready, another reads ready: out of the routing) .* worker=${pod}( |\$)")"
      joined="$(hk_log_instant "$p" "$proxy_c" "$at" "worker (health flip .* worker=\"${pod}\" from=[A-Za-z]+ to=Alive|acceptance flip .* worker=\"${pod}\" from=Refuse to=Accept)")"
      [ -z "$left" ] || [ -n "$joined" ] || exit "$HK_NOT_YET"
      proxies="$(jq -c --arg p "$p" --argjson l "${left:-null}" --argjson j "${joined:-null}" \
        '. + [{pod: $p, left_at_ms: $l, joined_at_ms: $j}]' <<<"$proxies")"
    done <<<"$pods"
  fi
  jq -cn --argjson s "$parked" --argjson x "$proxies" '{survivors: $s, proxies: $x}'
}
