#!/bin/sh
# The proxy pod's egress-routes container, on the node's network namespace
# (hostNetwork), NET_ADMIN alone. Reads EXTERNAL_INTERFACE, EXTERNAL_NEXT_HOPS
# (space-separated) and PERIOD from its env; the chart never rewrites this text.
#
# Keeps the external next hops in table 5060, the only table traffic from the
# external VIP uses (vip-loopback.sh, ADR-0056 §11): the kernel drops a gateway
# route when its interface goes, and they return here when it does. A turn writes
# only on a difference, so the route and the entries cached from it are not
# churned. An interface down or gone is the external face's verdict to read, not
# a failure here; with the interface up and an address on it, a third write
# failing in a row exits, so the pod reads not Ready and the node withdraws by the
# slow path.
trap 'exit 0' TERM INT

# The default routes of the table, one `next-hop@interface` per line.
installed() {
  prev=""
  hop=""
  for word in $(ip -o route show table 5060 default | grep -v '^unreachable'); do
    case "$prev" in
      via) hop="$word" ;;
      dev) echo "$hop@$word" ;;
    esac
    prev="$word"
  done
}

routes=""
for hop in $EXTERNAL_NEXT_HOPS; do
  routes="$routes nexthop via $hop dev $EXTERNAL_INTERFACE"
done
wanted="$(for hop in $EXTERNAL_NEXT_HOPS; do echo "$hop@$EXTERNAL_INTERFACE"; done | sort)"
echo "vip-egress-routes: keeping table 5060 on$routes (period ${PERIOD}s)"

failures=0
err=""
while true; do
  have="$(installed | sort)"
  if [ "$have" = "$wanted" ] || [ -z "$(ip -o -4 addr show dev "$EXTERNAL_INTERFACE" up 2>/dev/null)" ]; then
    failures=0
  elif err="$(ip route replace table 5060 default $routes 2>&1)"; then
    failures=0
    echo "vip-egress-routes: table 5060 installed on$routes"
  else
    failures=$((failures + 1))
    echo "vip-egress-routes: write $failures failed: $err"
    [ "$failures" -lt 3 ] || exit 1
  fi
  sleep "$PERIOD" &
  wait $!
done
