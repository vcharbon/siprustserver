# shellcheck shell=bash
# The holder of a virtual address among host-network pods (sourced after
# incident.sh): the holder carries the address on an interface other than
# `lo` (a VRRP backup parks it on `lo`, or not at all).

# "pod node interface" of each live pod of the selector holding `address`,
# read through `container` (which needs `ip`).
hk_vip_holders() { # address container [selector]
  local addr="$1" c="$2" pod node ifc
  while read -r pod _ node; do
    ifc="$(kubectl -n "$NS" exec "$pod" -c "$c" -- ip -4 -o addr show 2>/dev/null \
      | awk -v a="$addr" '$2 != "lo" && index($4, a "/") == 1 { print $2; exit }')"
    [ -z "$ifc" ] || echo "$pod $node $ifc"
  done < <(hk_live_pods "${3:-$HAKIT_TARGET_SELECTOR}")
}
