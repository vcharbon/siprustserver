#!/bin/sh
# The proxy pod's init container, on the node's network namespace
# (hostNetwork). Reads PROXY_VIP, EXTERNAL_VIP and UNRES_QLEN_BYTES from its
# env; the chart never rewrites this text.
#
# Both VIPs go on lo before the proxy starts, so its bind(VIP:port) on each
# face succeeds on the VRRP backup too. Strict ARP: with the VIPs on lo of both
# nodes, only the interface owning an address answers ARP for it.
#
# A next hop the kernel cannot resolve holds every datagram sent to it on the
# neighbour's unresolved queue, charged to the sending socket, for the whole
# ARP cycle (ADR-0033). The default bound equals a default socket send buffer;
# UNRES_QLEN_BYTES bounds it per neighbour on every interface. net.core.wmem_max
# is the host's and read-only here (the host preflight raises it).
set -e
ip addr add "$PROXY_VIP/32" dev lo 2>/dev/null || ip addr show dev lo | grep -q "$PROXY_VIP/32"
ip addr add "$EXTERNAL_VIP/32" dev lo 2>/dev/null || ip addr show dev lo | grep -q "$EXTERNAL_VIP/32"
echo 1 > /proc/sys/net/ipv4/conf/all/arp_ignore
echo 2 > /proc/sys/net/ipv4/conf/all/arp_announce
echo 1 > /proc/sys/net/ipv4/conf/lo/arp_ignore
echo 2 > /proc/sys/net/ipv4/conf/lo/arp_announce
for f in /proc/sys/net/ipv4/neigh/*/unres_qlen_bytes; do
  echo "$UNRES_QLEN_BYTES" > "$f" || echo "vip-loopback: WARN could not set $f"
done
echo "vip-loopback: $PROXY_VIP/32 + $EXTERNAL_VIP/32 + strict-ARP + neigh unres_qlen_bytes=$UNRES_QLEN_BYTES ready"
