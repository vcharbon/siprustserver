#!/bin/sh
# The proxy pod's init container, on the node's network namespace
# (hostNetwork). Reads PROXY_VIP, EXTERNAL_VIP, EXTRA_VIPS (space-separated,
# may be empty) and UNRES_QLEN_BYTES from its env; the chart never rewrites
# this text.
#
# Every floating address goes on lo before the proxy and its sidecars start,
# so a bind(VIP:port) succeeds on the VRRP backup too. Strict ARP: with the
# VIPs on lo of both nodes, only the interface owning an address answers ARP
# for it.
#
# A next hop the kernel cannot resolve holds every datagram sent to it on the
# neighbour's unresolved queue, charged to the sending socket, for the whole
# ARP cycle (ADR-0033). The default bound equals a default socket send buffer;
# UNRES_QLEN_BYTES bounds it per neighbour on every interface. net.core.wmem_max
# is the host's and read-only here (the host preflight raises it).
set -e
for vip in $PROXY_VIP $EXTERNAL_VIP $EXTRA_VIPS; do
  ip addr add "$vip/32" dev lo 2>/dev/null || ip addr show dev lo | grep -q "$vip/32"
done
echo 1 > /proc/sys/net/ipv4/conf/all/arp_ignore
echo 2 > /proc/sys/net/ipv4/conf/all/arp_announce
echo 1 > /proc/sys/net/ipv4/conf/lo/arp_ignore
echo 2 > /proc/sys/net/ipv4/conf/lo/arp_announce
for f in /proc/sys/net/ipv4/neigh/*/unres_qlen_bytes; do
  echo "$UNRES_QLEN_BYTES" > "$f" || echo "vip-loopback: WARN could not set $f"
done
echo "vip-loopback: $PROXY_VIP $EXTERNAL_VIP $EXTRA_VIPS on lo + strict-ARP + neigh unres_qlen_bytes=$UNRES_QLEN_BYTES ready"
