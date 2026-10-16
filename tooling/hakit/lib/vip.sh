# shellcheck shell=bash
# The holder of a virtual address among host-network pods (sourced after
# incident.sh): the holder carries the address on an interface other than
# `lo` (a VRRP backup parks it on `lo`, or not at all).

# "pod node interface" of each live pod of the selector holding `address`,
# read through `container` (which needs `ip`). Fails when a pod's addresses
# cannot be read.
hk_vip_holders() { # address container [selector]
  local addr="$1" c="$2" pod node addrs ifc pods
  pods="$(hk_live_pods "${3:-$HAKIT_TARGET_SELECTOR}")" || return 1
  while read -r pod _ node; do
    [ -n "$pod" ] || continue
    addrs="$(kubectl -n "$NS" exec "$pod" -c "$c" -- ip -4 -o addr show)" || return 1
    ifc="$(awk -v a="$addr" '$2 != "lo" && index($4, a "/") == 1 { print $2; exit }' <<<"$addrs")"
    [ -z "$ifc" ] || echo "$pod $node $ifc"
  done <<<"$pods"
}
