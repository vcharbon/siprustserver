#!/bin/sh
# The proxy pod's init container, on the node's network namespace
# (hostNetwork). Reads PROXY_VIP, EXTERNAL_VIP and UNRES_QLEN_BYTES from its
# env; the chart never rewrites this text.
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
set -e
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
echo "vip-loopback: $PROXY_VIP $EXTERNAL_VIP on lo + strict ARP + no ICMP port unreachable from them + neigh unres_qlen_bytes=$UNRES_QLEN_BYTES ready"
