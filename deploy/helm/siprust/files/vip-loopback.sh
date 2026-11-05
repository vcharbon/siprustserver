#!/bin/sh
# The proxy pod's init container, on the node's network namespace
# (hostNetwork). Reads PROXY_VIP, EXTERNAL_VIP, UNRES_QLEN_BYTES, the
# NEIGH_* timers and ROUTE_GUARD from its env; the chart never rewrites this
# text.
#
# Both VIPs go on lo before the proxy starts: the node owns them for good and
# the routers reach them through the node's announcement (ADR-0056). Strict
# ARP keeps lo's addresses off every neighbour table: the VIPs are routed,
# never ARP-answered.
#
# A packet for a VIP that reaches the node while no proxy socket is bound
# (the window before the node's route is withdrawn) would draw an ICMP port
# unreachable, which a SIP UAC treats as a transport error (RFC 3261 §18.4)
# instead of retransmitting into the peer's path: those are dropped.
#
# A next hop the kernel cannot resolve holds every datagram sent to it on the
# neighbour's unresolved queue, charged to the sending socket, for the whole
# ARP cycle (ADR-0033). The default bound equals a default socket send buffer;
# UNRES_QLEN_BYTES bounds it per neighbour on every interface. net.core.wmem_max
# is the host's and read-only here (the host preflight raises it).
#
# A next hop that dies with its carrier up reads FAILED only at the end of the
# neighbour cycle: the NEIGH_* timers bound that cycle on every interface, and
# fib_multipath_use_neigh makes multipath egress leave a next hop not resolved
# (ADR-0056 §10). A host namespace's default entry reaches an interface created
# later.
#
# With ROUTE_GUARD=off, a table an earlier route guard left on the node is
# removed, so the node's BFD and BGP pass; with it on, the pod's own guard
# owns the table (its init step left it withdrawing).
set -e
if [ "$ROUTE_GUARD" = off ]; then
  nft delete table inet route_guard 2>/dev/null || true
fi
for vip in $PROXY_VIP $EXTERNAL_VIP; do
  ip addr add "$vip/32" dev lo 2>/dev/null || ip addr show dev lo | grep -q "$vip/32"
  iptables -C OUTPUT -s "$vip/32" -p icmp --icmp-type port-unreachable -j DROP 2>/dev/null \
    || iptables -I OUTPUT 1 -s "$vip/32" -p icmp --icmp-type port-unreachable -j DROP
done
echo 1 > /proc/sys/net/ipv4/conf/all/arp_ignore
echo 2 > /proc/sys/net/ipv4/conf/all/arp_announce
echo 1 > /proc/sys/net/ipv4/conf/lo/arp_ignore
echo 2 > /proc/sys/net/ipv4/conf/lo/arp_announce
for f in /proc/sys/net/ipv4/neigh/*/unres_qlen_bytes; do
  echo "$UNRES_QLEN_BYTES" > "$f" || echo "vip-loopback: WARN could not set $f"
done
neigh_set() {
  for f in /proc/sys/net/ipv4/neigh/*/"$1"; do
    echo "$2" > "$f" || echo "vip-loopback: WARN could not set $f"
  done
}
neigh_set base_reachable_time_ms "$NEIGH_BASE_REACHABLE_TIME_MS"
neigh_set delay_first_probe_time "$NEIGH_DELAY_FIRST_PROBE_S"
neigh_set ucast_solicit "$NEIGH_UCAST_SOLICIT"
neigh_set mcast_solicit "$NEIGH_MCAST_SOLICIT"
neigh_set retrans_time_ms "$NEIGH_RETRANS_TIME_MS"
echo 1 > /proc/sys/net/ipv4/fib_multipath_use_neigh
echo "vip-loopback: $PROXY_VIP $EXTERNAL_VIP on lo + strict ARP + no ICMP port unreachable from them + neigh unres_qlen_bytes=$UNRES_QLEN_BYTES base_reachable_time_ms=$NEIGH_BASE_REACHABLE_TIME_MS delay_first_probe_time=$NEIGH_DELAY_FIRST_PROBE_S ucast_solicit=$NEIGH_UCAST_SOLICIT mcast_solicit=$NEIGH_MCAST_SOLICIT retrans_time_ms=$NEIGH_RETRANS_TIME_MS + multipath off unresolved next hops + route guard $ROUTE_GUARD ready"
