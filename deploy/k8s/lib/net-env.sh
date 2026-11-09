# Shared network + port wiring of the kind lab — the SINGLE source of truth,
# sourced by deploy/helm/lib/cluster.sh and restated by the charts' kind values
# (deploy/helm/check asserts they agree; PROXY_VIP is defaulted here once).
# Everything here is overridable from the environment.
#
#   SIP_SUBNET   internal kind docker bridge, the lower half of A.B.0.0/16
#                (default 172.20.0.0/17)
#   SIP_GATEWAY  bridge gateway — the BOTTOM of the subnet (.0.1), the host,
#                which runs the lab's top-of-rack router (deploy/k8s/tor/).
#                kind/docker hand node IPs out from the bottom upward.
#   PROXY_VIP    the internal VIP (.255.250), in the /16 but OUTSIDE the
#                bridge's /17: no node holds it on the bridge, every sender
#                reaches it through its default route to the host, whose
#                router installs the /32 the proxy nodes announce over BGP
#                (ADR-0056). Deriving both ends from one SIP_SUBNET keeps the
#                split reproduced.
#   SIP_PORT     UDP port the front proxy listens on (default 5060).
#
# Safe to source more than once; respects pre-set overrides.

# Internal cluster subnet: an A.B.0.0/17, so the gateway (.0.1) is on it and
# the VIP (.255.250) is not.
SIP_SUBNET="${SIP_SUBNET:-172.20.0.0/17}"
case "$SIP_SUBNET" in
  *.*.0.0/17) : ;;
  *) printf '\033[1;33m>> net-env: SIP_SUBNET=%s is not an A.B.0.0/17 — the .0.1 gateway on it / .255.250 VIP off it split may not hold\033[0m\n' "$SIP_SUBNET" >&2 ;;
esac

# First two octets, then reproduce the split: gateway at the bottom (.0.1),
# VIP at the top of the /16 (.255.250), off the bridge. Either end is
# independently overridable when you need a bespoke address.
_sip_prefix="${SIP_SUBNET%.0.0/*}"
SIP_GATEWAY="${SIP_GATEWAY:-${_sip_prefix}.0.1}"
PROXY_VIP="${PROXY_VIP:-${_sip_prefix}.255.250}"

# UDP port the front proxy listens on (the b-leg workers and the generator
# streams all dial the VIP on this port). The internal worker/UAS SIP port
# stays 5060 regardless — this knob is the externally-facing port only.
SIP_PORT="${SIP_PORT:-5060}"

# ---------------------------------------------------------------------------
# External SIP plane ("sipext") — the no-NAT dual-plane layout.
#
# A dedicated docker bridge with masquerade DISABLED; only the two edge nodes
# attach (dual-homed), plus the load generators (loadgen, the SIPp UACs and
# UASes) as plain docker containers. Callers reach the proxy's EXTERNAL face
# (SIPEXT_VIP) through the host's router, which routes the /32 to the edge
# nodes whose proxies are Ready — no SNAT/DNAT/conntrack rewrite anywhere on
# the SIP path (the masquerade-drop fix-class).
#
# Interface names are never hardcoded in a template: SIPEXT_IF is restated as
# the kind values' network.externalInterface (the chart check asserts they
# agree); eth1 = the sipext `docker network connect` done right after node
# creation holds on any kind bring-up.
#
#   SIPEXT_SUBNET   external plane, the lower half of A.B.C.0/24 (default
#                   192.168.60.0/25)
#   SIPEXT_GATEWAY  bridge gateway = the host's own address (.1): the
#                   router's external face and the proxies' external next hop
#   SIPEXT_VIP      the external VIP (.250), in the /24 but OUTSIDE the
#                   bridge's /25, routed like PROXY_VIP
#   SIPEXT_IF       optional explicit interface override (else resolved)
#   SIPEXT_NET      docker network name
#   Pinned container/edge addresses: edges .11/.12, sipp-uas .21 — pinned so
#   ladders/scenarios are reproducible and a docker restart can never reshuffle
#   them.
SIPEXT_SUBNET="${SIPEXT_SUBNET:-192.168.60.0/25}"
case "$SIPEXT_SUBNET" in
  *.0/25) : ;;
  *) printf '\033[1;33m>> net-env: SIPEXT_SUBNET=%s is not an A.B.C.0/25 — the .1 gateway on it / .250 VIP off it split may not hold\033[0m\n' "$SIPEXT_SUBNET" >&2 ;;
esac
_sipext_prefix="${SIPEXT_SUBNET%.0/*}"
SIPEXT_GATEWAY="${SIPEXT_GATEWAY:-${_sipext_prefix}.1}"
SIPEXT_VIP="${SIPEXT_VIP:-${_sipext_prefix}.250}"
SIPEXT_IF="${SIPEXT_IF:-eth1}"
SIPEXT_NET="${SIPEXT_NET:-sipext}"
SIPEXT_EDGE1_IP="${SIPEXT_EDGE1_IP:-${_sipext_prefix}.11}"
SIPEXT_EDGE2_IP="${SIPEXT_EDGE2_IP:-${_sipext_prefix}.12}"
SIPEXT_UAS_IP="${SIPEXT_UAS_IP:-${_sipext_prefix}.21}"

export SIP_SUBNET SIP_GATEWAY PROXY_VIP SIP_PORT
export SIPEXT_SUBNET SIPEXT_GATEWAY SIPEXT_VIP SIPEXT_IF SIPEXT_NET
export SIPEXT_EDGE1_IP SIPEXT_EDGE2_IP SIPEXT_UAS_IP

# Generator stream IP plan: every stream the HA tooling runs as a docker
# container on the sipext bridge gets ONE deterministic IP at
# SIPEXT_UAC_BASE_IP + its slot (the runner's HAKIT_SIPP_IP_BASE / _SLOTS),
# parked ABOVE the pinned single-container range so parallel streams can never
# collide with a pinned IP or with each other. A SIPp stream's stat exporter
# runs in the SAME network namespace, so :9035 rides the stream's own IP; a
# loadgen stream serves :9300 there. Full address plan of the /25 and the VIP
# above it:
#   .1    gateway (the host)             .11/.12 edge nodes (SIPEXT_EDGE*_IP)
#   .21+  sipp-uas containers
#   .64+  generator streams (slots 0..35 up to .99, scraped by the host
#         VictoriaMetrics)
#   .250  SIPEXT_VIP (off the bridge, routed)
SIPEXT_UAC_BASE_IP="${SIPEXT_UAC_BASE_IP:-${_sipext_prefix}.64}"
export SIPEXT_UAC_BASE_IP

# The lab's top-of-rack router (deploy/k8s/tor/): FRR in a host-network
# container, eBGP with the edge nodes' MetalLB speakers on both bridges, BFD,
# multipath over the announcing nodes. TOR_ASN is the router's AS,
# PROXY_NODE_ASN the edge nodes' (the kind values' proxy.announce.metallb
# restates both; the chart check asserts they agree). TOR_NAME names the
# container.
# BFD_INTERVAL_MS and BFD_DETECT_MULTIPLIER are the router's BFD timers,
# restated as the kind values' proxy.announce.metallb.bfd.
TOR_ASN="${TOR_ASN:-64512}"
PROXY_NODE_ASN="${PROXY_NODE_ASN:-64513}"
TOR_NAME="${TOR_NAME:-sip-tor}"
TOR_IMAGE="${TOR_IMAGE:-quay.io/frrouting/frr:10.6.2}"
BFD_INTERVAL_MS="${BFD_INTERVAL_MS:-300}"
BFD_DETECT_MULTIPLIER="${BFD_DETECT_MULTIPLIER:-3}"
export TOR_ASN PROXY_NODE_ASN TOR_NAME TOR_IMAGE BFD_INTERVAL_MS BFD_DETECT_MULTIPLIER

# The bridges' names on the host, fixed so the one-time host setup
# (deploy/k8s/lab-host-setup.sh) can name them in its sysctl and firewall
# rules before they exist.
SIP_BRIDGE="${SIP_BRIDGE:-br-sip-kind}"
SIPEXT_BRIDGE="${SIPEXT_BRIDGE:-br-sipext}"
export SIP_BRIDGE SIPEXT_BRIDGE

# kube-proxy's mode on the kind cluster: iptables or nftables. IPVS binds
# every LoadBalancer address to every node and refuses its unlisted ports,
# so a worker could not reach the internal VIP: refused (ADR-0056).
KUBE_PROXY_MODE="${KUBE_PROXY_MODE:-iptables}"
export KUBE_PROXY_MODE
