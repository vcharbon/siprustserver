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

# ---------------------------------------------------------------------------
# External SIP plane ("sipext") — the no-NAT dual-plane layout.
#
# A dedicated docker bridge with masquerade DISABLED; only the two edge nodes
# attach (dual-homed), plus the load generators (loadgen / sipp-uas / dnsmasq)
# as plain docker containers. Callers reach the proxy's EXTERNAL face
# (SIPEXT_VIP) same-L2 — no SNAT/DNAT/conntrack rewrite anywhere on the SIP
# path (the fix-class for newkahneed-041). The WSL host owns the bridge
# gateway (.1) so it dials the VIP directly.
#
# Replication contract: interface names are ENV, never hardcoded in manifests.
# SIP_IF/SIPEXT_IF are envsubst-stamped into the keepalived conf; the kind
# defaults (eth0 = the kind bridge at node creation, eth1 = the sipext
# `docker network connect` done immediately after) hold on any kind bring-up.
# Any other Linux box reproduces the run by exporting the two names for
# whatever interfaces hold addresses in SIP_SUBNET/SIPEXT_SUBNET (dummy/bridge
# for single-box; two real boxes need genuinely shared L2 for VRRP).
#
#   SIPEXT_SUBNET   external plane /24 (default 192.168.60.0/24)
#   SIPEXT_GATEWAY  bridge gateway = the WSL host's own address (.1)
#   SIPEXT_VIP      keepalived VRRP VIP on the external plane (.250, parked
#                   high like PROXY_VIP so pinned container IPs never collide)
#   SIPEXT_IF       optional explicit interface override (else resolved)
#   SIPEXT_NET      docker network name
#   Pinned container/edge addresses: edges .11/.12, loadgen .20, sipp-uas .21,
#   dnsmasq .53 — pinned so ladders/scenarios are reproducible and a docker
#   restart can never reshuffle them.
SIPEXT_SUBNET="${SIPEXT_SUBNET:-192.168.60.0/24}"
case "$SIPEXT_SUBNET" in
  *.0/24) : ;;
  *) printf '\033[1;33m>> net-env: SIPEXT_SUBNET=%s is not an A.B.C.0/24 — the .1 gateway / .250 VIP split may be off-subnet\033[0m\n' "$SIPEXT_SUBNET" >&2 ;;
esac
_sipext_prefix="${SIPEXT_SUBNET%.0/*}"
SIPEXT_GATEWAY="${SIPEXT_GATEWAY:-${_sipext_prefix}.1}"
SIPEXT_VIP="${SIPEXT_VIP:-${_sipext_prefix}.250}"
SIP_IF="${SIP_IF:-eth0}"
SIPEXT_IF="${SIPEXT_IF:-eth1}"
SIPEXT_NET="${SIPEXT_NET:-sipext}"
SIPEXT_EDGE1_IP="${SIPEXT_EDGE1_IP:-${_sipext_prefix}.11}"
SIPEXT_EDGE2_IP="${SIPEXT_EDGE2_IP:-${_sipext_prefix}.12}"
SIPEXT_LOADGEN_IP="${SIPEXT_LOADGEN_IP:-${_sipext_prefix}.20}"
SIPEXT_UAS_IP="${SIPEXT_UAS_IP:-${_sipext_prefix}.21}"
SIPEXT_DNSMASQ_IP="${SIPEXT_DNSMASQ_IP:-${_sipext_prefix}.53}"

# Egress face-picker for the dual-face proxy: destinations inside these CIDRs
# leave on the INTERNAL face (source = PROXY_VIP); everything else leaves on
# the EXTERNAL face (source = SIPEXT_VIP). Pod CIDR + the internal kind subnet.
POD_CIDR="${POD_CIDR:-10.244.0.0/16}"
PROXY_FACE_INT_CIDRS="${PROXY_FACE_INT_CIDRS:-${POD_CIDR},${SIP_SUBNET}}"

# What external callers dial — the EXTERNAL VIP now (PROXY_TARGET stays the
# internal VIP: it is what in-cluster components, e.g. workers' outbound
# proxy, keep dialing).
SIPEXT_TARGET="${SIPEXT_TARGET:-$SIPEXT_VIP}"

export SIP_SUBNET SIP_GATEWAY PROXY_VIP PROXY_TARGET SIP_PORT
export SIPEXT_SUBNET SIPEXT_GATEWAY SIPEXT_VIP SIP_IF SIPEXT_IF SIPEXT_NET
export SIPEXT_EDGE1_IP SIPEXT_EDGE2_IP SIPEXT_LOADGEN_IP SIPEXT_UAS_IP SIPEXT_DNSMASQ_IP
export POD_CIDR PROXY_FACE_INT_CIDRS SIPEXT_TARGET

# --- NEW KNOB (sipext generator migration): UAC stream IP plan ---------------
# Every SIPp UAC stream launched as a docker container on the sipext bridge
# (lib/sipext-gen.sh) gets ONE deterministic IP at SIPEXT_UAC_BASE_IP + slot,
# parked ABOVE the pinned single-container range so parallel streams can never
# collide with a pinned IP or with each other. The stream's stat exporter runs
# in the SAME network namespace (--network container:<uac>), so :9035 rides the
# stream's own IP — no second address needed. Full address plan for the /24:
#   .1    gateway (the WSL host)         .11/.12 edge nodes (SIPEXT_EDGE*_IP)
#   .20   loadgen (SIPEXT_LOADGEN_IP)    .21+    sipp-uas containers
#   .53   dnsmasq                        .100+   UAC streams (slot plan below)
#   .250  SIPEXT_VIP (parked high)
# Slot conventions (see lib/sipext-gen.sh; observability scrapes .100-.135):
#   0      run.sh caps()/sweep()          1/2    chaos.sh failover/bringback
#   3/4/5  chaos.sh peak/abuse/orphan     10..29 endurance long shards
#   30..35 endurance reinvite/short_em/short_ne/limiter/spare
SIPEXT_UAC_BASE_IP="${SIPEXT_UAC_BASE_IP:-${_sipext_prefix}.100}"
export SIPEXT_UAC_BASE_IP
