# Shared network + port wiring for the kind-based SIP runner — the SINGLE source
# of truth, sourced by run.sh, endurance.sh and chaos.sh so the three never
# drift (PROXY_VIP/PROXY_TARGET used to be defaulted independently in all three).
# Everything here is overridable from the environment; the defaults reproduce
# the historical layout exactly.
#
#   SIP_SUBNET   internal kind docker bridge /16   (default 172.20.0.0/16)
#   SIP_GATEWAY  bridge gateway — the BOTTOM of the subnet (.0.1). kind/docker
#                hand node IPs out from the bottom of the range upward.
#   PROXY_VIP    keepalived VRRP VIP — the TOP of the subnet (.255.250). Parked
#                high so it can NEVER collide with a dynamically-allocated node
#                IP. This top/bottom split is the invariant the layout depends
#                on; deriving both ends from one SIP_SUBNET keeps it reproduced.
#   PROXY_TARGET what the SIPp UAC streams dial — the VIP by default.
#   SIP_PORT     UDP port the front-proxy LB listens on (default 5060).
#
# Source me AFTER `cd`-ing into deploy/k8s (all three runners do `cd "$(dirname
# "$0")"` first). Safe to source more than once; respects pre-set overrides.

# Internal cluster subnet. Must be a /16 for the top/bottom split below to land
# on-subnet (a /16 gives a .0.x bottom and a .255.x top).
SIP_SUBNET="${SIP_SUBNET:-172.20.0.0/16}"
case "$SIP_SUBNET" in
  *.*.0.0/16) : ;;
  *) printf '\033[1;33m>> net-env: SIP_SUBNET=%s is not an A.B.0.0/16 — the .0.1 gateway / .255.250 VIP split may be off-subnet\033[0m\n' "$SIP_SUBNET" >&2 ;;
esac

# First two octets (the /16 prefix), then reproduce the split: gateway at the
# bottom (.0.1), VIP parked at the top (.255.250). Either end is independently
# overridable when you need a bespoke address.
_sip_prefix="${SIP_SUBNET%.0.0/*}"
SIP_GATEWAY="${SIP_GATEWAY:-${_sip_prefix}.0.1}"
PROXY_VIP="${PROXY_VIP:-${_sip_prefix}.255.250}"
PROXY_TARGET="${PROXY_TARGET:-$PROXY_VIP}"

# UDP port the front-proxy load balancer listens on (the b-leg workers and the
# SIPp UAC streams all dial the VIP on this port). The internal worker/UAS SIP
# port stays 5060 regardless — this knob is the externally-facing LB port only.
SIP_PORT="${SIP_PORT:-5060}"

export SIP_SUBNET SIP_GATEWAY PROXY_VIP PROXY_TARGET SIP_PORT
