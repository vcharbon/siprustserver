# shellcheck shell=bash
# The call-path smokes of a lab running the front proxy (kind-lab.sh deploy,
# through lib/sut.sh): the lab router routes both VIPs over both edge nodes,
# the external VIP answers SIP from the callers' plane, only the proxy bridges
# the two planes, and the workers' NetworkPolicy keeps other pods out. Sourced
# after lib/cluster.sh (log, die, kubectl on the lab's context, the network
# plan, lib/tor.sh).

# The addresses of the tier=edge nodes on the internal bridge, sorted, one
# per line.
edge_internal_addrs() {
  kubectl get nodes -l tier=edge \
    -o jsonpath='{range .items[*]}{.status.addresses[?(@.type=="InternalIP")].address}{"\n"}{end}' | sort
}

# Both VIPs routed over both edge nodes in the host's table (the lab
# router's FIB), within ROUTE_SMOKE_TIMEOUT (120 s): each node announces once
# its proxy is Ready (ADR-0056).
route_smoke() {
  local timeout="${ROUTE_SMOKE_TIMEOUT:-120}" internal external
  internal="$(edge_internal_addrs | tr '\n' ' ' | sed 's/ $//')"
  external="$(printf '%s\n' "$SIPEXT_EDGE1_IP" "$SIPEXT_EDGE2_IP" | sort | tr '\n' ' ' | sed 's/ $//')"
  log "route-smoke: $PROXY_VIP over '$internal', $SIPEXT_VIP over '$external' (<=${timeout}s)"
  tor_wait_next_hops "$PROXY_VIP" "$internal" "$timeout" \
    || die "route-smoke: the internal VIP is not routed over both edge nodes (ip route show $PROXY_VIP; docker exec $TOR_NAME vtysh -c 'show bgp summary')"
  tor_wait_next_hops "$SIPEXT_VIP" "$external" "$timeout" \
    || die "route-smoke: the external VIP is not routed over both edge nodes (ip route show $SIPEXT_VIP; docker exec $TOR_NAME vtysh -c 'show bgp summary')"
  log "route-smoke: both VIPs routed over both edge nodes"
}

# An OPTIONS to SIPEXT_VIP:SIP_PORT from a one-shot container on sipext, the
# callers' vantage: the request crosses the lab router's route to an edge
# node, the reply comes back on the bridge. Retried every ~3 s up to
# VIP_SMOKE_TIMEOUT (90 s).
vip_smoke() {
  local timeout="${VIP_SMOKE_TIMEOUT:-90}" attempts probe
  attempts=$(( timeout / 3 ))
  log "vip-smoke: OPTIONS round-trip via ${SIPEXT_VIP}:${SIP_PORT} from sipext (<=${attempts} attempts)"
  probe="$(cat <<'EOS'
vip="$1"; port="$2"; attempts="$3"
myip="$(hostname -i 2>/dev/null | awk '{print $1}')"
i=0
while [ "$i" -lt "$attempts" ]; do
  printf 'OPTIONS sip:vip-smoke@%s:%s SIP/2.0\r\nVia: SIP/2.0/UDP %s:5060;branch=z9hG4bK-vipsmoke-%s;rport\r\nMax-Forwards: 10\r\nFrom: <sip:vip-smoke@vip-smoke.invalid>;tag=vipsmoke%s\r\nTo: <sip:vip-smoke@%s>\r\nCall-ID: vip-smoke-%s@vip-smoke\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n' \
    "$vip" "$port" "$myip" "$i" "$i" "$vip" "$i" > /tmp/req
  nc -u -w 2 "$vip" "$port" < /tmp/req > /tmp/resp 2>/dev/null || true
  if grep -q '^SIP/2.0' /tmp/resp; then
    echo "VIP-SMOKE-OK attempt=$i status=$(head -n1 /tmp/resp | tr -d '\r')"
    exit 0
  fi
  i=$((i+1))
  sleep 1
done
echo "VIP-SMOKE-FAIL: no SIP response from $vip:$port in $attempts attempts"
exit 1
EOS
)"
  docker rm -f vip-smoke >/dev/null 2>&1 || true
  docker run --rm --name vip-smoke --label "sipext-run=$CLUSTER" \
    --network "$SIPEXT_NET" "$NETTOOLS_IMAGE" \
    /bin/sh -c "$probe" vip-smoke "$SIPEXT_VIP" "$SIP_PORT" "$attempts"
}

# The plane isolation, each property probed from where a violation would
# start: (1) sipext reaches neither a worker pod nor the internal VIP, (2) the
# pod network does not reach the external VIP, (3) the external test zone is
# NXDOMAIN in the cluster. A negative probe is tried 3 times, so one lost
# packet cannot hide a real reply.
isolation_smoke() {
  local neg wip
  neg="$(cat <<'EOS'
target="$1"; port="$2"; label="$3"
i=0
while [ "$i" -lt 3 ]; do
  printf 'OPTIONS sip:iso@%s:%s SIP/2.0\r\nVia: SIP/2.0/UDP 0.0.0.0:5060;branch=z9hG4bK-iso-%s;rport\r\nMax-Forwards: 10\r\nFrom: <sip:iso@iso.invalid>;tag=iso%s\r\nTo: <sip:iso@%s>\r\nCall-ID: iso-%s@iso\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n' \
    "$target" "$port" "$i" "$i" "$target" "$i" > /tmp/req
  nc -u -w 2 "$target" "$port" < /tmp/req > /tmp/resp 2>/dev/null || true
  if grep -q '^SIP/2.0' /tmp/resp; then
    echo "ISOLATION-FAIL: $label ($target:$port) ANSWERED from this vantage"
    exit 1
  fi
  i=$((i+1))
done
echo "ISOLATION-OK: $label ($target:$port) unreachable as required"
exit 0
EOS
)"
  wip="$(kubectl -n "$NS" get pods -l app=b2bua-worker -o jsonpath='{.items[0].status.podIP}' 2>/dev/null)"
  [ -n "$wip" ] || die "isolation-smoke: no b2bua-worker pod IP (deploy first)"
  log "isolation-smoke 1/3: sipext must NOT reach worker $wip / internal VIP $PROXY_VIP"
  docker run --rm --label "sipext-run=$CLUSTER" --network "$SIPEXT_NET" "$NETTOOLS_IMAGE" \
    /bin/sh -c "$neg" iso "$wip" 5060 "worker-pod" \
    || die "isolation-smoke: worker pod reachable from sipext"
  docker run --rm --label "sipext-run=$CLUSTER" --network "$SIPEXT_NET" "$NETTOOLS_IMAGE" \
    /bin/sh -c "$neg" iso "$PROXY_VIP" "$SIP_PORT" "internal-VIP" \
    || die "isolation-smoke: internal VIP reachable from sipext: the planes are bridged outside the proxy"
  log "isolation-smoke 2/3: the pod network must NOT reach external VIP $SIPEXT_VIP"
  kubectl -n "$NS" delete pod iso-smoke --ignore-not-found --now >/dev/null 2>&1 || true
  kubectl -n "$NS" run iso-smoke --rm -i --restart=Never \
    --image="$NETTOOLS_IMAGE" --image-pull-policy=IfNotPresent \
    --pod-running-timeout=2m \
    --overrides='{"spec":{"nodeSelector":{"tier":"app"}}}' \
    --command -- /bin/sh -c "$neg" iso "$SIPEXT_VIP" "$SIP_PORT" "external-VIP" \
    || die "isolation-smoke: external VIP reachable from the pod network"
  log "isolation-smoke 3/3: the external zone must be NXDOMAIN in the cluster"
  kubectl -n "$NS" run iso-dns --rm -i --restart=Never \
    --image="$NETTOOLS_IMAGE" --image-pull-policy=IfNotPresent \
    --pod-running-timeout=2m \
    --overrides='{"spec":{"nodeSelector":{"tier":"app"}}}' \
    --command -- /bin/sh -c 'if nslookup uas.sipext.test >/dev/null 2>&1; then echo "ISOLATION-FAIL: uas.sipext.test resolves in-cluster"; exit 1; fi; echo "ISOLATION-OK: external zone NXDOMAIN in-cluster"' \
    || die "isolation-smoke: external test zone resolvable from the pod network"
  log "isolation-smoke: all 3 properties hold"
}

# The workers' NetworkPolicy, probed from a pod of the release's namespace
# that is no worker: a worker answers neither SIP on 5060/UDP nor a
# replication connect on its replication port. A probe is tried 3 times, so
# one lost packet cannot hide a real answer. Skipped when the release renders
# no policy (worker.networkPolicy.enabled=false).
policy_smoke() {
  local probe wip port
  if ! kubectl -n "$NS" get networkpolicy b2bua-worker >/dev/null 2>&1; then
    log "policy-smoke: no b2bua-worker NetworkPolicy in $NS: skipped"
    return 0
  fi
  wip="$(kubectl -n "$NS" get pods -l app=b2bua-worker -o jsonpath='{.items[0].status.podIP}' 2>/dev/null)"
  [ -n "$wip" ] || die "policy-smoke: no b2bua-worker pod IP (deploy first)"
  port="$(kubectl -n "$NS" get pods -l app=b2bua-worker -o jsonpath='{.items[0].spec.containers[0].ports[?(@.name=="repl")].containerPort}' 2>/dev/null)"
  port="${port:-9092}"
  probe="$(cat <<'EOS'
wip="$1"; repl="$2"
i=0
while [ "$i" -lt 3 ]; do
  printf 'OPTIONS sip:np@%s:5060 SIP/2.0\r\nVia: SIP/2.0/UDP 0.0.0.0:5060;branch=z9hG4bK-np-%s;rport\r\nMax-Forwards: 10\r\nFrom: <sip:np@np.invalid>;tag=np%s\r\nTo: <sip:np@%s>\r\nCall-ID: np-%s@np\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n' \
    "$wip" "$i" "$i" "$wip" "$i" > /tmp/req
  nc -u -w 2 "$wip" 5060 < /tmp/req > /tmp/resp 2>/dev/null || true
  if grep -q '^SIP/2.0' /tmp/resp; then
    echo "POLICY-FAIL: worker $wip:5060/UDP answered a pod that is no proxy"; exit 1
  fi
  if nc -w 2 "$wip" "$repl" < /dev/null > /dev/null 2>&1; then
    echo "POLICY-FAIL: worker $wip:$repl/TCP took a connect from a pod that is no worker"; exit 1
  fi
  i=$((i+1))
done
echo "POLICY-OK: worker $wip refuses 5060/UDP and $repl/TCP to a pod that is no proxy and no worker"
EOS
)"
  log "policy-smoke: a pod in $NS must NOT reach worker $wip on 5060/UDP nor $port/TCP"
  kubectl -n "$NS" delete pod np-smoke --ignore-not-found --now >/dev/null 2>&1 || true
  kubectl -n "$NS" run np-smoke --rm -i --restart=Never \
    --image="$NETTOOLS_IMAGE" --image-pull-policy=IfNotPresent \
    --pod-running-timeout=2m \
    --overrides='{"spec":{"nodeSelector":{"tier":"app"}}}' \
    --command -- /bin/sh -c "$probe" np "$wip" "$port" \
    || die "policy-smoke: the workers' NetworkPolicy lets a plain pod in (does the CNI enforce NetworkPolicy?)"
}
