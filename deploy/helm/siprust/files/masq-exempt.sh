#!/bin/sh
# The masq-exempt DaemonSet's loop, on every node's network namespace
# (hostNetwork). Reads PROXY_VIP, EXTERNAL_VIP, EXTRA_VIPS (space-separated,
# may be empty), EXTERNAL_SUBNET, EXTERNAL_INTERFACE and PERIOD from its env;
# the chart never rewrites this text.
#
# kindnetd's KIND-MASQ-AGENT chain (nat POSTROUTING) exempts only the pod CIDR.
# The SIP plane keeps real addresses: worker -> internal VIP would be SNAT'd to
# a node IP with its source port remapped, and the VIP-sourced replies would no
# longer match the peer's conntrack tuple; the external plane is never
# masqueraded either. One RETURN rule per identity and direction at position 1,
# re-asserted every PERIOD seconds so a kindnetd reconcile wipes them for one
# period at most.
#
# One filter rule rides the same loop: a packet for the internal VIP arriving
# on the external interface is dropped. The proxy trusts whatever reaches its
# internal socket (ADR-0048), and an edge node holds both VIPs.
# One RETURN rule in KIND-MASQ-AGENT: $1 is -s or -d, $2 the address.
exempt() {
  iptables -t nat -C KIND-MASQ-AGENT "$1" "$2" -j RETURN 2>/dev/null \
    || { iptables -t nat -I KIND-MASQ-AGENT 1 "$1" "$2" -j RETURN && echo "masq-exempt: inserted $1 $2 RETURN"; }
}
echo "masq-exempt: asserting KIND-MASQ-AGENT RETURN rules for $PROXY_VIP, $EXTERNAL_VIP $EXTRA_VIPS, $EXTERNAL_SUBNET and the $EXTERNAL_INTERFACE -> $PROXY_VIP DROP (period ${PERIOD}s)"
while true; do
  iptables -C INPUT -i "$EXTERNAL_INTERFACE" -d "$PROXY_VIP/32" -j DROP 2>/dev/null \
    || { iptables -I INPUT 1 -i "$EXTERNAL_INTERFACE" -d "$PROXY_VIP/32" -j DROP && echo "masq-exempt: inserted INPUT -i $EXTERNAL_INTERFACE -d $PROXY_VIP/32 DROP"; }
  if iptables -t nat -nL KIND-MASQ-AGENT >/dev/null 2>&1; then
    exempt -s "$PROXY_VIP/32"
    exempt -d "$PROXY_VIP/32"
    exempt -s "$EXTERNAL_VIP/32"
    exempt -d "$EXTERNAL_SUBNET"
    for vip in $EXTRA_VIPS; do
      exempt -s "$vip/32"
      exempt -d "$vip/32"
    done
  fi
  sleep "$PERIOD"
done
