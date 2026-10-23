# shellcheck shell=bash
# Pod and node fault mechanisms of the base incidents, applied from the kind
# node that hosts the pod (kind node name == its docker container name), so
# they need nothing in the pod's image and no capability on the pod:
#
#   pf_freeze / pf_thaw          cgroup v2 freeze of the whole pod (its pod-level
#                                cgroup: every container stops, sockets stay open)
#   pf_reject_tcp / pf_unreject  TCP RST to new connections on a port, in the
#                                pod's netns; the kubelet's probes still pass
#   pf_delay / pf_loss / pf_undelay  netem on the pod's eth0 (egress)
#   pf_node_stop / pf_node_start / pf_node_wait_ready  the kind node, a machine loss
#   pf_pod_residue               every pod fault still applied, one word each
#
# Every function fails (non-zero, the reason on stderr) when it cannot do or
# read what it states; none takes an error for an answer. A network fault is
# applied and undone through the pod's sandbox netns, which outlives any
# restart of its containers. Callers provide NS and a context-pinned kubectl.

PF_REJECT_TAG="${PF_REJECT_TAG:-hakit-reject}"

pf_err() { echo "pod-faults: $*" >&2; return 1; }

pf_pod_node() { # pod
  local n
  n="$(kubectl -n "$NS" get pod "$1" -o jsonpath='{.spec.nodeName}')" || return 1
  [ -n "$n" ] || { pf_err "$1 has no node"; return 1; }
  echo "$n"
}
pf_node_ip() { # node
  local ip
  ip="$(kubectl get node "$1" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')" || return 1
  [ -n "$ip" ] || { pf_err "$1 has no InternalIP"; return 1; }
  echo "$ip"
}

# "<node> <pid>" of the pod's sandbox, the pid as the node sees it.
pf_pod_sandbox() { # pod
  local node uid sb pid
  node="$(pf_pod_node "$1")" || return 1
  uid="$(kubectl -n "$NS" get pod "$1" -o jsonpath='{.metadata.uid}')" || return 1
  sb="$(docker exec "$node" crictl pods --label "io.kubernetes.pod.uid=$uid" --state ready -q)" || return 1
  sb="${sb%%$'\n'*}"
  [ -n "$sb" ] || { pf_err "no ready sandbox for $1"; return 1; }
  pid="$(docker exec "$node" crictl inspectp --output go-template --template '{{.info.pid}}' "$sb")" || return 1
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || { pf_err "no sandbox pid for $1: '$pid'"; return 1; }
  echo "$node $pid"
}

# Run a command in the pod's network namespace, from its node.
pf_netns() { # pod command…
  local h
  h="$(pf_pod_sandbox "$1")" || return 1
  shift
  docker exec "${h% *}" nsenter -t "${h#* }" -n -- "$@"
}

# "<node> <dir>": the pod-level cgroup v2 directory on its node (the parent of
# the sandbox's own cgroup, holding every container of the pod).
pf_pod_cgroup() { # pod
  local h node rel
  h="$(pf_pod_sandbox "$1")" || return 1
  node="${h% *}"
  rel="$(docker exec "$node" cat "/proc/${h#* }/cgroup")" || return 1
  rel="$(awk -F: '$1 == "0" { print $3; exit }' <<<"$rel")"
  [ -n "$rel" ] || { pf_err "no cgroup v2 path for $1"; return 1; }
  echo "$node /sys/fs/cgroup${rel%/*}"
}

pf_set_freeze() { # pod 0|1
  local cg now
  cg="$(pf_pod_cgroup "$1")" || return 1
  docker exec "${cg% *}" sh -c "echo $2 > '${cg#* }/cgroup.freeze'" || return 1
  now="$(docker exec "${cg% *}" cat "${cg#* }/cgroup.freeze")" || return 1
  [ "$now" = "$2" ] || { pf_err "cgroup.freeze of $1 reads '$now' after writing $2"; return 1; }
}
pf_freeze() { pf_set_freeze "$1" 1; }
pf_thaw() { pf_set_freeze "$1" 0; }

# The kubelet probes a pod from its node through the pod's default gateway, so
# the node's own addresses stay accepted and the pod stays Ready.
pf_reject_tcp() { # pod port
  local ip routes gw src
  ip="$(pf_node_ip "$(pf_pod_node "$1")")" || return 1
  routes="$(pf_netns "$1" ip -4 route show default)" || return 1
  gw="$(awk '{ print $3; exit }' <<<"$routes")"
  pf_netns "$1" iptables -I INPUT -p tcp --dport "$2" \
    -m comment --comment "$PF_REJECT_TAG" -j REJECT --reject-with tcp-reset || return 1
  for src in "$ip" $gw; do
    pf_netns "$1" iptables -I INPUT -p tcp --dport "$2" -s "$src" \
      -m comment --comment "$PF_REJECT_TAG" -j ACCEPT || return 1
  done
}
# Delete every rule pf_reject_tcp added.
pf_unreject() { # pod
  local rules rule
  while :; do
    rules="$(pf_netns "$1" iptables -S INPUT)" || return 1
    rule="$(awk -v t="$PF_REJECT_TAG" 'index($0, t) { print; exit }' <<<"$rules")"
    [ -n "$rule" ] || return 0
    # shellcheck disable=SC2086  # the rule's own words
    pf_netns "$1" iptables ${rule/#-A/-D} || return 1
  done
}

pf_delay() { pf_netns "$1" tc qdisc replace dev eth0 root netem delay "${2}ms"; } # pod ms
pf_loss() { pf_netns "$1" tc qdisc replace dev eth0 root netem loss "${2}%"; }    # pod pct
# Remove the root qdisc when it carries a netem anywhere (a netem root or a
# prio tree holding one), then prove no netem is left.
pf_undelay() { # pod
  local q
  q="$(pf_netns "$1" tc qdisc show dev eth0)" || return 1
  if grep -q netem <<<"$q"; then
    pf_netns "$1" tc qdisc del dev eth0 root || return 1
    q="$(pf_netns "$1" tc qdisc show dev eth0)" || return 1
    if grep -q netem <<<"$q"; then pf_err "netem still on $1 after its removal"; return 1; fi
  fi
}

# Every pod fault still applied to the pod, one word each (netem, reject,
# frozen); fails when any of its state cannot be read.
pf_pod_residue() { # pod
  local q r cg f
  q="$(pf_netns "$1" tc qdisc show dev eth0)" || return 1
  r="$(pf_netns "$1" iptables -S INPUT)" || return 1
  cg="$(pf_pod_cgroup "$1")" || return 1
  f="$(docker exec "${cg% *}" cat "${cg#* }/cgroup.freeze")" || return 1
  if grep -q netem <<<"$q"; then echo netem; fi
  if grep -q -- "$PF_REJECT_TAG" <<<"$r"; then echo reject; fi
  if [ "$f" != 0 ]; then echo frozen; fi
}

pf_node_stop() { docker stop -t 0 "$1" >/dev/null; } # node
pf_node_start() { docker start "$1" >/dev/null; }   # node
# Wait until the API server sees the node Ready.
pf_node_wait_ready() { kubectl wait --for=condition=Ready "node/$1" --timeout="${2}s" >&2; } # node timeout_s
