# shellcheck shell=bash
# Pod and node fault mechanisms for the chaos primitives, applied from the kind
# node that hosts the pod (kind node name == its docker container name), so they
# need nothing in the pod's image and no capability on the pod:
#
#   pf_freeze / pf_thaw        cgroup v2 freeze of the pod's first container: every
#                              process stops, sockets stay open, nothing answers
#   pf_reject_tcp / pf_unreject  TCP RST to new connections on a port, in the pod's
#                              netns; the kubelet's probes (from its node) still pass
#   pf_delay / pf_loss / pf_undelay  netem delay / 100 % loss on the pod's eth0
#   pf_node_stop / pf_node_start  the whole kind node, as a machine loss
#   pf_clear_pod               undo every pod fault above; idempotent
#   pf_pod_residue             name every pod fault still applied (empty = clean)
#
# Every fault is undone by its pair or by pf_clear_pod, whatever state a killed
# caller left behind. Callers provide NS; the functions return non-zero and print
# nothing on stdout when the pod cannot be resolved.

PF_REJECT_TAG="${PF_REJECT_TAG:-chaos-reject}"

# A sleep a trap interrupts at once (bash runs a trap only between commands, so
# a foreground sleep would hold the caller's cleanup for its whole length).
pf_sleep() { sleep "$1" & wait "$!"; }

# The live (not terminating) pod of a label selector.
pf_pod_of() { # $1 selector
  kubectl -n "$NS" get pod -l "$1" \
    -o jsonpath='{range .items[*]}{.metadata.deletionTimestamp}{" "}{.metadata.name}{"\n"}{end}' 2>/dev/null \
    | awk '$0 ~ /^ / && $1 != "" { print $1; exit }'
}
pf_pod_node() { kubectl -n "$NS" get pod "$1" -o jsonpath='{.spec.nodeName}' 2>/dev/null; }
pf_node_ip()  { kubectl get node "$1" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null; }

# "<node> <host pid>" of the pod's first container, the pid as the node sees it.
pf_pod_host() { # $1 pod
  local node cid pid
  node="$(pf_pod_node "$1")"; [ -n "$node" ] || return 1
  cid="$(kubectl -n "$NS" get pod "$1" -o jsonpath='{.status.containerStatuses[0].containerID}' 2>/dev/null)"
  cid="${cid#*://}"; [ -n "$cid" ] || return 1
  pid="$(docker exec "$node" crictl inspect --output go-template --template '{{.info.pid}}' "$cid" 2>/dev/null)"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  echo "$node $pid"
}

# "<node> <pid>" of the pod's sandbox: its netns outlives any restart of the
# pod's containers, so a network fault is applied and undone through it.
pf_pod_sandbox() { # $1 pod
  local node uid sb pid
  node="$(pf_pod_node "$1")"; [ -n "$node" ] || return 1
  uid="$(kubectl -n "$NS" get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null)"; [ -n "$uid" ] || return 1
  sb="$(docker exec "$node" crictl pods --label "io.kubernetes.pod.uid=$uid" --state ready -q 2>/dev/null | head -n 1)"
  [ -n "$sb" ] || return 1
  pid="$(docker exec "$node" crictl inspectp --output go-template --template '{{.info.pid}}' "$sb" 2>/dev/null)"
  [[ "$pid" =~ ^[1-9][0-9]*$ ]] || return 1
  echo "$node $pid"
}

# Run a command in the pod's network namespace, from its node.
pf_netns() { # $1 pod  $2.. command
  local host; host="$(pf_pod_sandbox "$1")" || return 1; shift
  docker exec "${host% *}" nsenter -t "${host#* }" -n -- "$@"
}

# The container's cgroup v2 directory on its node.
pf_cgroup() { # $1 pod  →  "<node> <dir>"
  local host node pid rel
  host="$(pf_pod_host "$1")" || return 1
  node="${host% *}"; pid="${host#* }"
  rel="$(docker exec "$node" cat "/proc/$pid/cgroup" 2>/dev/null | awk -F: '$1 == "0" { print $3; exit }')"
  [ -n "$rel" ] || return 1
  echo "$node /sys/fs/cgroup$rel"
}
pf_set_freeze() { # $1 pod  $2 0|1
  local cg; cg="$(pf_cgroup "$1")" || return 1
  docker exec "${cg% *}" sh -c "echo $2 > '${cg#* }/cgroup.freeze'"
}
pf_freeze() { pf_set_freeze "$1" 1; }
pf_thaw()   { pf_set_freeze "$1" 0; }

# The kubelet probes a pod from its node, through the pod's default gateway, so
# the node's own addresses stay accepted: the pod stays Ready and its Service
# keeps routing to it, and every other client gets the RST.
pf_reject_tcp() { # $1 pod  $2 port
  local ip gw src
  ip="$(pf_node_ip "$(pf_pod_node "$1")")"; [ -n "$ip" ] || return 1
  gw="$(pf_netns "$1" ip -4 route show default | awk '{ print $3; exit }')"
  pf_netns "$1" iptables -I INPUT -p tcp --dport "$2" \
    -m comment --comment "$PF_REJECT_TAG" -j REJECT --reject-with tcp-reset || return 1
  for src in "$ip" $gw; do
    pf_netns "$1" iptables -I INPUT -p tcp --dport "$2" -s "$src" \
      -m comment --comment "$PF_REJECT_TAG" -j ACCEPT || return 1
  done
}
pf_unreject() { # $1 pod: delete every rule this file added
  local rule
  while rule="$(pf_netns "$1" iptables -S INPUT 2>/dev/null | grep -m1 -- "$PF_REJECT_TAG")" && [ -n "$rule" ]; do
    # shellcheck disable=SC2086  # the rule's own words
    pf_netns "$1" iptables ${rule/#-A/-D} || return 1
  done
}

pf_delay()   { pf_netns "$1" tc qdisc replace dev eth0 root netem delay "${2}ms"; } # $1 pod  $2 ms
pf_loss()    { pf_netns "$1" tc qdisc replace dev eth0 root netem loss 100%; }     # $1 pod
# Remove any qdisc on eth0; fails unless the netns answers and holds no netem.
pf_undelay() {
  pf_netns "$1" tc qdisc del dev eth0 root 2>/dev/null
  local q; q="$(pf_netns "$1" tc qdisc show dev eth0)" || return 1
  ! grep -q netem <<< "$q"
}

# Undo every pod fault on the live pod of a selector: thaw, no REJECT rule, no
# qdisc on eth0 (which also removes a netem loss set from inside the pod).
pf_clear_pod() { # $1 selector
  local pod; pod="$(pf_pod_of "$1")"; [ -n "$pod" ] || return 0
  pf_thaw "$pod" 2>/dev/null
  pf_unreject "$pod" 2>/dev/null
  pf_undelay "$pod"
  return 0
}

# Every pod fault still applied to pod $1, one word each (netem, reject,
# frozen), or "unreadable" when its netns or cgroup cannot be read.
pf_pod_residue() { # $1 pod
  local q r cg f
  q="$(pf_netns "$1" tc qdisc show dev eth0 2>/dev/null)" || { echo unreadable; return; }
  grep -q netem <<< "$q" && echo netem
  r="$(pf_netns "$1" iptables -S INPUT 2>/dev/null)" || { echo unreadable; return; }
  grep -q -- "$PF_REJECT_TAG" <<< "$r" && echo reject
  cg="$(pf_cgroup "$1")" || { echo unreadable; return; }
  f="$(docker exec "${cg% *}" cat "${cg#* }/cgroup.freeze" 2>/dev/null)" || { echo unreadable; return; }
  [ "$f" = 0 ] || echo frozen
}

# Nodes by label, as docker container names.
pf_nodes_of() { kubectl get nodes -l "$1" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null; }
pf_node_stop()  { docker stop -t 0 "$1" >/dev/null; }
# Start a stopped node and wait until the API server sees it Ready again.
pf_node_start() { # $1 node  [$2 timeout s]
  docker start "$1" >/dev/null || return 1
  kubectl wait --for=condition=Ready "node/$1" --timeout="${2:-180}s" >/dev/null 2>&1
}
# Every kind node of the cluster that is not running (a node fault left behind).
pf_stopped_nodes() { # $1 cluster name
  docker ps -a --filter "label=io.x-k8s.kind.cluster=$1" --filter status=exited --format '{{.Names}}'
}
