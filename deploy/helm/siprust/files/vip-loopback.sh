#!/bin/sh
# The proxy pod's init container, on the node's network namespace
# (hostNetwork). Reads PROXY_VIP, EXTERNAL_VIP, EXTERNAL_INTERFACE,
# UNRES_QLEN_BYTES, the NEIGH_* timers and ROUTE_GUARD from its env; the chart
# never rewrites this text.
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
# Traffic from the external VIP leaves through the external interface and
# nowhere else (ADR-0056 §11). A rule sends it to a table of its own, whose
# unreachable default stays while no next hop is installed there, so a miss never
# falls through to the main table; the egress-routes container keeps the next
# hops in it. The local table comes before any rule, so what stops local delivery
# is the chain, which holds no address list and no exemption for lo: a datagram
# from the external socket to the internal VIP would otherwise reach a socket
# trusted by its face. None of this reads the interface, so a pod starting during
# a cut comes up, and reads unfit.
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
ip route replace table 5060 unreachable default metric 4294967295
ip rule show priority 5060 | grep -Fq "from $EXTERNAL_VIP lookup 5060" \
  || ip rule add priority 5060 from "$EXTERNAL_VIP" lookup 5060
stale="$(ip rule show priority 5060 | grep -Fv "from $EXTERNAL_VIP lookup 5060" | head -n 1)"
while [ -n "$stale" ]; do
  ip rule del priority 5060 ${stale#*:}
  stale="$(ip rule show priority 5060 | grep -Fv "from $EXTERNAL_VIP lookup 5060" | head -n 1)"
done
jump=""
iptables -C OUTPUT -j SIP-EXT-EGRESS 2>/dev/null || jump="-I OUTPUT 1 -j SIP-EXT-EGRESS"
iptables-restore --noflush <<RULES
*filter
:SIP-EXT-EGRESS - [0:0]
-A SIP-EXT-EGRESS -s $EXTERNAL_VIP/32 ! -o $EXTERNAL_INTERFACE -j DROP
$jump
COMMIT
RULES
echo "vip-loopback: $PROXY_VIP $EXTERNAL_VIP on lo + strict ARP + no ICMP port unreachable from them + neigh unres_qlen_bytes=$UNRES_QLEN_BYTES base_reachable_time_ms=$NEIGH_BASE_REACHABLE_TIME_MS delay_first_probe_time=$NEIGH_DELAY_FIRST_PROBE_S ucast_solicit=$NEIGH_UCAST_SOLICIT mcast_solicit=$NEIGH_MCAST_SOLICIT retrans_time_ms=$NEIGH_RETRANS_TIME_MS + multipath off unresolved next hops + $EXTERNAL_VIP only through $EXTERNAL_INTERFACE + route guard $ROUTE_GUARD ready"
