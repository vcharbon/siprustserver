#!/bin/sh
# The lab router's start. FRR leaves its routes in the host's table when it
# stops, and a zebra that finds a VIP's route of an earlier run adopts it as a
# static route at distance 1, which wins over BGP: every route of a VIP that
# frr.conf names a blackhole for is removed before FRR starts.
awk '$1 == "ip" && $2 == "route" && $4 == "blackhole" { print $3 }' /etc/frr/frr.conf | while read -r vip; do
  while ip route del "$vip" 2>/dev/null; do :; done
done
exec /usr/lib/frr/docker-start
