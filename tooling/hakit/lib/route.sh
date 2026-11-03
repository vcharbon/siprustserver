# shellcheck shell=bash
# The routes of the front proxy's VIPs in the host's table (sourced after
# incident.sh): the lab's router installs, per VIP, one next hop per node
# that announces it, so a node withdrawn is a next hop gone. The internal VIP
# is reached over a node's internal address, the external VIP over its
# address on the callers' docker network. Every read checks itself: a failed
# read is no answer.
#
# Params read: HAKIT_PARAM_INTERNAL_VIP, HAKIT_PARAM_EXTERNAL_VIP,
# HAKIT_PARAM_NETWORK (the callers' docker network); HK_ROUTE_POLL_MS (100)
# is the polling period of a wait.

# The next hops of `vip` in the host's table, sorted, one a line; nothing
# when the VIP has no route of its own.
hk_next_hops() { # vip
  local json
  json="$(ip -j route show "$1/32")" || return 1
  jq -r '.[] | (.gateway // empty), (.nexthops[]?.gateway // empty)' <<<"${json:-[]}" | sort
}

# The node's address on the plane of `vip`: its InternalIP for the internal
# VIP, its address on the callers' network for the external one.
hk_plane_addr() { # node vip
  local ip
  if [ "$2" = "$HAKIT_PARAM_INTERNAL_VIP" ]; then
    pf_node_ip "$1"
  else
    ip="$(docker inspect -f "{{with index .NetworkSettings.Networks \"$HAKIT_PARAM_NETWORK\"}}{{.IPAddress}}{{end}}" "$1")" || return 1
    [ -n "$ip" ] || { echo "route: $1 has no address on $HAKIT_PARAM_NETWORK" >&2; return 1; }
    echo "$ip"
  fi
}

# "vip addr" a line: each node's address on each VIP's plane.
hk_route_pairs() { # node…
  local node vip addr
  for node in "$@"; do
    for vip in "$HAKIT_PARAM_INTERNAL_VIP" "$HAKIT_PARAM_EXTERNAL_VIP"; do
      addr="$(hk_plane_addr "$node" "$vip")" || return 1
      echo "$vip $addr"
    done
  done
}

# Whether `addr` is a next hop of `vip` now.
hk_routed_via() { # vip addr
  local hops
  hops="$(hk_next_hops "$1")" || return 2
  grep -qxF "$2" <<<"$hops"
}

# Wait until every "vip addr" pair of `pairs` is gone (`want` absent) or
# present (`want` present), at most `timeout_s`. Prints one JSON object
# {"<vip> <addr>": ms after `since_ms`} with the instant each pair turned;
# fails naming the pairs that never did.
hk_route_wait() { # want since_ms timeout_s pairs
  local want="$1" since="$2" deadline pairs="$4" vip addr rc now out="{}" left
  deadline=$(( $(hk_now_ms) + $3 * 1000 ))
  while :; do
    left=""
    while read -r vip addr; do
      [ -n "$vip" ] || continue
      jq -e --arg k "$vip $addr" 'has($k)' <<<"$out" >/dev/null && continue
      hk_routed_via "$vip" "$addr" && rc=0 || rc=$?
      [ "$rc" -le 1 ] || return 1
      now="$(hk_now_ms)"
      if { [ "$want" = absent ] && [ "$rc" -eq 1 ]; } || { [ "$want" = present ] && [ "$rc" -eq 0 ]; }; then
        out="$(jq -c --arg k "$vip $addr" --argjson ms $(( now - since )) '. + {($k): $ms}' <<<"$out")"
      else
        left="$left$vip via $addr; "
      fi
    done <<<"$pairs"
    [ -n "$left" ] || { echo "$out"; return 0; }
    [ "$(hk_now_ms)" -lt "$deadline" ] || { echo "route: still not $want after ${3}s: $left" >&2; return 1; }
    sleep "$(awk -v ms="${HK_ROUTE_POLL_MS:-100}" 'BEGIN { printf "%.3f", ms / 1000 }')"
  done
}

# Exit 75 (not yet) unless every VIP is routed over exactly the nodes of the
# live pods of `selector` (the target selector by default). Prints
# {"<vip>": [next hops]}.
hk_probe_routed_over_live() { # [selector]
  local live live_nodes=() node vip want have addr out="{}"
  live="$(hk_live_pods "${1:-$HAKIT_TARGET_SELECTOR}")" || exit "$HK_NOT_YET"
  while read -r _ _ node; do
    if [ -n "$node" ]; then live_nodes+=("$node"); fi
  done <<<"$live"
  [ "${#live_nodes[@]}" -gt 0 ] || exit "$HK_NOT_YET"
  for vip in "$HAKIT_PARAM_INTERNAL_VIP" "$HAKIT_PARAM_EXTERNAL_VIP"; do
    want=""
    for node in "${live_nodes[@]}"; do
      addr="$(hk_plane_addr "$node" "$vip")" || exit "$HK_NOT_YET"
      want="$want$addr"$'\n'
    done
    want="$(sort <<<"$want" | awk NF)"
    have="$(hk_next_hops "$vip")" || exit "$HK_NOT_YET"
    [ "$have" = "$want" ] || exit "$HK_NOT_YET"
    out="$(jq -c --arg v "$vip" --arg h "$have" '. + {($v): ($h | split("\n"))}' <<<"$out")"
  done
  echo "$out"
}
