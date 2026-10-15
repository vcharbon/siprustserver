# shellcheck shell=bash
# The restart timings of a killed B2BUA worker, read from the logs of the
# pods around it (sourced after incident.sh): when each surviving worker
# parked the killed one's replication peer, and when each front proxy dropped
# it from its registry and took the replacement in. Log lines are this
# repository's (`b2bua::repl::supervisor`, `sip_proxy::registry::projection`);
# a line not found leaves its instant null.

# The first log instant (epoch ms) of `pod`'s container since `since_ms`
# whose line matches the extended regexp; empty when none.
hk_log_instant() { # pod container since_ms regexp
  local since line
  since="$(date -u -d "@$(( $3 / 1000 ))" +%Y-%m-%dT%H:%M:%SZ)"
  line="$(kubectl -n "$NS" logs "$1" ${2:+-c "$2"} --since-time="$since" 2>/dev/null | grep -m1 -E "$4" || true)"
  [ -n "$line" ] || return 0
  date -d "${line%% *}" +%s%3N
}

# Facts of the worker `pod` killed at `killed_ms`: per survivor of the target
# selector its park instant, per proxy its left and joined instants.
hk_worker_timings() { # pod killed_ms
  local pod="$1" at="$2" p at_park left joined parked="[]" proxies="[]"
  local proxy_sel="${HAKIT_PARAM_PROXY_SELECTOR:-}" proxy_c="${HAKIT_PARAM_PROXY_CONTAINER:-}"
  while read -r p _ _; do
    [ "$p" != "$pod" ] || continue
    at_park="$(hk_log_instant "$p" "" "$at" "replication peer parked .* peer=${pod}( |\$)")"
    parked="$(jq -c --arg p "$p" --argjson t "${at_park:-null}" '. + [{pod: $p, parked_at_ms: $t}]' <<<"$parked")"
  done < <(hk_live_pods)
  if [ -n "$proxy_sel" ]; then
    while read -r p _ _; do
      left="$(hk_log_instant "$p" "$proxy_c" "$at" "worker left the registry .* worker=${pod}( |\$)")"
      joined="$(hk_log_instant "$p" "$proxy_c" "$at" "worker (joined the registry|re-created at a new host) .* worker=${pod}( |\$)")"
      proxies="$(jq -c --arg p "$p" --argjson l "${left:-null}" --argjson j "${joined:-null}" \
        '. + [{pod: $p, left_at_ms: $l, joined_at_ms: $j}]' <<<"$proxies")"
    done < <(hk_live_pods "$proxy_sel")
  fi
  jq -cn --argjson s "$parked" --argjson x "$proxies" '{survivors: $s, proxies: $x}'
}
