#!/usr/bin/env bash
# The kind lab's one-time host setup, run once with sudo; the lab's bring-up
# needs no sudo afterwards. The host is the lab's top-of-rack router
# (deploy/k8s/tor/): it routes both VIPs to the edge nodes that announce them,
# out of the very bridge a sender's packet came in on.
#
#   sudo deploy/k8s/lab-host-setup.sh           write and apply the settings
#   deploy/k8s/lab-host-setup.sh --check        exit 0 when they are in place
#
# Writes $SYSCTL_FILE (/etc/sysctl.d/90-sip-lab.conf), applied at boot and,
# through systemd's udev rule, to each lab bridge as docker creates it:
#   - net.ipv4.fib_multipath_hash_policy=1: the router hashes flows on the
#     5-tuple, so the generator streams spread over the announcing nodes;
#   - send_redirects=0 (all, default and both bridges): a packet routed back
#     out its own bridge draws no ICMP redirect, which would teach the sender
#     a node's address and bypass the routing;
#   - rp_filter=2 on both bridges: a worker pod's packet, routed in from the
#     internal bridge with its pod address, passes the loose reverse-path
#     check (the host has no route to the pod network).
# With ufw active, adds rules: BGP (tcp 179) and BFD (udp 3784-3785) in from
# both bridges, and forwarding in and out of the same bridge. Forwarding
# between the two bridges stays docker's to refuse (the plane isolation).
#
# Env: the bridge names of deploy/k8s/lib/net-env.sh (SIP_BRIDGE,
# SIPEXT_BRIDGE); SYSCTL_FILE.
set -euo pipefail
HERE="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
# shellcheck source-path=SCRIPTDIR source=lib/net-env.sh
source "$HERE/lib/net-env.sh"
SYSCTL_FILE="${SYSCTL_FILE:-/etc/sysctl.d/90-sip-lab.conf}"

sysctl_text() {
  cat <<EOF
# The kind lab's routing on this host (deploy/k8s/lab-host-setup.sh).
net.ipv4.ip_forward = 1
net.ipv4.fib_multipath_hash_policy = 1
net.ipv4.conf.all.send_redirects = 0
net.ipv4.conf.default.send_redirects = 0
net.ipv4.conf.$SIP_BRIDGE.send_redirects = 0
net.ipv4.conf.$SIPEXT_BRIDGE.send_redirects = 0
net.ipv4.conf.$SIP_BRIDGE.rp_filter = 2
net.ipv4.conf.$SIPEXT_BRIDGE.rp_filter = 2
EOF
}

# The ufw rules, one `ufw` argument list per line.
ufw_rules() {
  local br
  for br in "$SIP_BRIDGE" "$SIPEXT_BRIDGE"; do
    echo "allow in on $br to any port 179 proto tcp comment sip-lab-bgp"
    echo "allow in on $br to any port 3784:3785 proto udp comment sip-lab-bfd"
    echo "route allow in on $br out on $br comment sip-lab-forward"
  done
}

ufw_active() { command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q '^Status: active'; }

# Whether the settings are in place: the file as written, the global keys live.
check() {
  [ -f "$SYSCTL_FILE" ] && [ "$(cat "$SYSCTL_FILE")" = "$(sysctl_text)" ] || return 1
  [ "$(sysctl -n net.ipv4.fib_multipath_hash_policy)" = 1 ] || return 1
  [ "$(sysctl -n net.ipv4.conf.all.send_redirects)" = 0 ] || return 1
}

apply() {
  [ "$(id -u)" -eq 0 ] || { echo "lab-host-setup: run it with sudo" >&2; exit 1; }
  sysctl_text > "$SYSCTL_FILE"
  echo "lab-host-setup: wrote $SYSCTL_FILE"
  # A bridge that does not exist yet gets its keys from udev when created.
  sysctl -e -p "$SYSCTL_FILE" >/dev/null
  echo "lab-host-setup: applied $SYSCTL_FILE"
  if ufw_active; then
    local rule
    while read -r rule; do
      # shellcheck disable=SC2086
      ufw $rule >/dev/null
    done < <(ufw_rules)
    echo "lab-host-setup: ufw rules for $SIP_BRIDGE and $SIPEXT_BRIDGE in place"
  else
    echo "lab-host-setup: ufw inactive, no firewall rule needed"
  fi
}

case "${1:-}" in
  --check) check ;;
  --print) sysctl_text; ufw_rules ;;
  "") apply ;;
  *) echo "usage: $0 [--check|--print]" >&2; exit 2 ;;
esac
