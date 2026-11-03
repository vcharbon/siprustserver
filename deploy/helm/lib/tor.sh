# shellcheck shell=bash
# The kind lab's top-of-rack router (deploy/k8s/tor/): FRR in a host-network
# container that outlives the checkout (restart unless-stopped, its
# configuration copied in), peering with the edge nodes' MetalLB speakers on
# both bridges, and the reads of the host's table it installs. Sourced after
# lib/cluster.sh (log, die, the network plan).

TOR_DIR="$K8S_DIR/tor"

# frr.conf.tmpl with every @NAME@ replaced by its net-env value, on stdout.
tor_config() {
  local name text
  text="$(cat "$TOR_DIR/frr.conf.tmpl")"
  for name in TOR_NAME TOR_ASN PROXY_NODE_ASN PROXY_VIP SIPEXT_VIP SIP_GATEWAY SIP_SUBNET \
    SIPEXT_SUBNET BFD_INTERVAL_MS BFD_DETECT_MULTIPLIER; do
    text="${text//@$name@/${!name}}"
  done
  case "$text" in *@[A-Z]*@*) die "tor: an @NAME@ of frr.conf.tmpl is left unreplaced" ;; esac
  printf '%s\n' "$text"
}

# The router container, (re)created when absent or when its configuration
# differs from the rendered one; left alone otherwise.
tor_up() {
  local want have dir
  want="$(tor_config)"
  have="$(docker exec "$TOR_NAME" cat /etc/frr/frr.conf 2>/dev/null || true)"
  if [ "$have" = "$want" ] && [ "$(docker inspect -f '{{.State.Running}}' "$TOR_NAME" 2>/dev/null)" = true ]; then
    log "tor: $TOR_NAME up, configuration unchanged"
    return 0
  fi
  docker image inspect "$TOR_IMAGE" >/dev/null 2>&1 || docker pull "$TOR_IMAGE" >/dev/null
  docker rm -f "$TOR_NAME" >/dev/null 2>&1 || true
  dir="$(mktemp -d "${TMPDIR:-/tmp}/sip-tor.XXXXXX")"
  printf '%s\n' "$want" > "$dir/frr.conf"
  cp "$TOR_DIR/daemons" "$TOR_DIR/vtysh.conf" "$dir/"
  docker create --name "$TOR_NAME" --label "sipext-run=$CLUSTER" \
    --restart unless-stopped --network host --privileged \
    "$TOR_IMAGE" >/dev/null
  docker cp "$dir/." "$TOR_NAME:/etc/frr/" >/dev/null
  rm -f "$dir/frr.conf" "$dir/daemons" "$dir/vtysh.conf"
  rmdir "$dir"
  docker start "$TOR_NAME" >/dev/null
  log "tor: $TOR_NAME up (AS $TOR_ASN, edge nodes AS $PROXY_NODE_ASN, BFD ${BFD_INTERVAL_MS} ms x $BFD_DETECT_MULTIPLIER)"
}

tor_down() { docker rm -f "$TOR_NAME" >/dev/null 2>&1 || true; }

# The next hops of `vip` in the host's table, one address per line, sorted;
# nothing when no node announces it (its blackhole route has no gateway).
tor_next_hops() { # vip
  ip -j route show "$1/32" 2>/dev/null \
    | jq -r '.[] | (.gateway // empty), (.nexthops[]?.gateway // empty)' | sort
}

# Wait up to `timeout_s` for `vip` to be routed over exactly `want` (the
# sorted, space-separated next hops); fails naming what the table holds.
tor_wait_next_hops() { # vip want timeout_s
  local vip="$1" want="$2" deadline have
  deadline=$(( $(date +%s) + $3 ))
  while :; do
    have="$(tor_next_hops "$vip" | tr '\n' ' ' | sed 's/ $//')"
    [ "$have" = "$want" ] && return 0
    [ "$(date +%s)" -lt "$deadline" ] || {
      log "tor: $vip is routed over '${have:-nothing}', want '$want'"
      return 1
    }
    sleep 1
  done
}
