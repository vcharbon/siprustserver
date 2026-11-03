# Containerized Rust SIP SUT, its kind lab and its HA tooling

Production-shaped, containerizable binaries for the Rust front proxy and B2BUA,
the Helm charts that deploy them, a kind lab that brings a cluster up from the
charts, and the HA tooling runner (`tooling/hakit`, ADR-0049) that loads the lab,
fires incidents and judges the calls.

```
sipp UAC ──INVITE──▶ sip-front-proxy ──INVITE──▶ b2bua-worker pool ──INVITE──▶ sipp UAS
 (docker on the        (tier=edge, dual-faced:     (tier=app, real CDR          (docker on the
  no-NAT sipext         ext VIP caller-facing /     buffer, /metrics)            sipext bridge,
  bridge; dials the     int VIP worker-facing;                                   pinned IPs)
  external VIP)         LB + HMAC Record-Route
                        stickiness, OPTIONS
                        health probe, /metrics)
```

Every docker-placed load generator (the SIPp UAC/UAS, the Rust loadgen) is a
plain container on the `sipext` bridge with a pinned IP and explicit
`--cpus/--memory` caps; the address plan is `deploy/k8s/lib/net-env.sh`. A
loadgen stream may instead run as a pod (`placement: pod` in its profile).

## Binaries (containerizable today)

| Binary | Crate | Wiring |
|---|---|---|
| `sip-proxy-runner` | `crates/sip-proxy-runner` | Real UDP transport · `LoadBalancerStrategy` (HRW + HMAC-signed Record-Route stickiness) · EndpointSlice worker discovery (`ComposedWorkerRegistry`; a fixed `PROXY_WORKERS` list when set) · OPTIONS `HealthProbe` feeding the band observer · `EluCpsGate` self-gate · Prometheus `/metrics` |
| `route-guard` | `crates/route-guard` | Beside each proxy, on the node's network with `NET_ADMIN` alone: reads the proxy's `/fitz` and keeps an nftables set that lets the node's BFD and BGP to its routers out while the proxy is fit (ADR-0056 §10) · Prometheus `/metrics` on `ROUTE_GUARD_METRICS` |
| `b2bua-runner` | `crates/b2bua-runner` | Real UDP transport · system clock · `BufferedCdrWriter` over a discarding sink (no endurance leak) · `InMemoryCallStore` · `NoopLimiter` · `ScriptedDecisionEngine` routing all calls to the UAS · Prometheus `/metrics` |

Both are configured entirely by env vars (see the module docs at the top of each
`main.rs`). HA call replication is opt-in via `B2BUA_REPL=1`: real TCP repl
transport + `K8sMembership` EndpointSlice discovery + SIGTERM drain + `/ready`
probe (ADR-0011); a worker with it on refuses to boot without `B2BUA_CLUSTER_KEY`
(ADR-0055 §12), which the chart reads from the Secret
`worker.clusterKey.existingSecret` names (the kind lab creates one). The proxy
refuses a boot it cannot honour (ADR-0046): it needs
`PROXY_HMAC_KEY`, and an advertise address when it listens on an unspecified one.

### Image

One image carries both binaries; the k8s `command` selects which to run.

```bash
docker build -f deploy/docker/Dockerfile -t siprustserver:dev .   # context = repo root
```

## Helm charts and the kind lab — `deploy/helm`

`deploy/helm/siprust` deploys the SUT and its backends, and
`deploy/helm/siprust-observability` the in-cluster collectors;
`deploy/helm/kind-lab.sh` brings the kind lab up from them. See
[deploy/helm/README.md](helm/README.md).

## Bring-up and a first run

From a clean clone, with the tools of the system requirements below:

```bash
deploy/helm/kind-lab.sh up                         # cluster, sipext bridge, images (~10-30 min cold)
deploy/observability/install.sh --bootstrap        # host VictoriaMetrics/Logs/Traces + Grafana :3333
deploy/helm/kind-lab.sh deploy                     # collectors, Chaos Mesh, SIPp callees, the SUT, smokes
cd tooling/hakit
uv run --locked hakit validate                     # the content of the search path
uv run --locked hakit --dry-run run kill-worker-smoke   # the profile replayed against a fake executor
uv run --locked hakit run kill-worker-smoke        # 10 min: basic calls, a worker crash every 150 s
```

`hakit run` writes the run directory under `HAKIT_RESULTS` (default
`~/.local/state/hakit/results`): `events.jsonl`, the records, `verdict.json` and
`summary.md`. The base profiles are `tooling/hakit/content/profiles/`; a product
adds its own incidents, budgets, profiles and dashboards on `HAKIT_PATH`
(ADR-0049 §3). `deploy/helm/kind-lab.sh down` deletes the cluster and the sipext
plane; the host stack and its history stay.

### Configuration (env vars)

Network and port wiring is a single source of truth in
`deploy/k8s/lib/net-env.sh`, which the charts' kind values restate (the helm
check asserts they agree). Everything is overridable from the environment;
a change of plan is a change of `values-kind.yaml` too.

| Env var | Default | Meaning |
|---|---|---|
| `SIP_SUBNET` | `172.20.0.0/17` | Internal kind docker bridge, the lower half of its /16. The gateway and VIP are *derived* from it. |
| `SIP_GATEWAY` | `<prefix>.0.1` | Bridge gateway — the **bottom** of the subnet (kind hands node IPs out from the bottom up); the host, the lab's router. |
| `PROXY_VIP` | `<prefix>.255.250` | The internal VIP, **off** the bridge: the lab router routes it to the edge nodes that announce it (ADR-0056). |
| `SIPEXT_SUBNET`, `SIPEXT_VIP` | `192.168.60.0/25`, `192.168.60.250` | The external bridge and the external VIP, off it, routed the same way. |
| `KUBE_PROXY_MODE` | `iptables` | kube-proxy's mode at `up`: `iptables` or `nftables` (IPVS refused). |
| `TOR_ASN`, `PROXY_NODE_ASN`, `BFD_INTERVAL_MS`, `BFD_DETECT_MULTIPLIER` | `64512`, `64513`, `300`, `3` | The lab router's AS, the edge nodes' AS and the BFD timers (restated by the kind values). |
| `SIP_BRIDGE`, `SIPEXT_BRIDGE` | `br-sip-kind`, `br-sipext` | The bridges' host interface names, which the one-time host setup names. |
| `SIP_PORT` | `5060` | UDP port the front-proxy **LB** listens on. |
| `CLUSTER` / `NS` | `sip-e2e` / `sip-test` | kind cluster name / namespace. |
| `SUT_IMAGE` | `siprustserver:dev` | Rust SUT image tag `up` builds. |

Host preflight knobs (see system requirements below):

| Env var | Default | Meaning |
|---|---|---|
| `PREFLIGHT_STRICT` | `0` | `1` makes a failed cgroup/sysctl check **abort** `up` instead of warning. |
| `PREFLIGHT_FIX_SYSCTLS` | `0` | `1` auto-raises any low sysctl (needs root / passwordless sudo). |
| `REQUIRED_SYSCTLS` | `fs.inotify.max_user_instances=512 fs.inotify.max_user_watches=524288 fs.file-max=2097152 net.core.wmem_max=4194304 net.core.rmem_max=4194304` | Space-separated `key=min` pairs checked at `up`. |

## System requirements (kind host)

The lab runs on any Linux box with rootful Docker (a VM included). `kind-lab.sh up`
runs a host preflight (cgroups + sysctls, advisory by default — see the knobs
above, and the lab routing, required) before creating the cluster. The host is
the lab's top-of-rack router: run `sudo deploy/k8s/lab-host-setup.sh` once
(layer-4 multipath hashing, no ICMP redirects and loose reverse-path filtering
on the lab bridges, and with ufw active the BGP, BFD and same-bridge forwarding
rules); the bring-up needs no sudo afterwards.

| Requirement | Minimum / note |
|---|---|
| Tools on `PATH` | `docker` (rootful), `docker compose`, `kind`, `kubectl`, `helm`, `uv` — `kind-lab.sh` checks the first four and `helm`; `uv` runs the HA tooling. |
| **cgroups** | **v2 strongly preferred** (default on Ubuntu 22.04+, Debian 12, Fedora 35+, RHEL/Rocky 9). On cgroup **v1** the host must boot with `cgroup_enable=memory swapaccount=1`, otherwise `cap-kind-memory.sh`'s `--memory-swap` node ceiling is **silently ignored**. Verify: `docker info \| grep -i cgroup` → `Cgroup Version: 2`. |
| **sysctls** | A 6-node kind cluster (`cluster.yaml`) exhausts default inotify/fd limits. Set `fs.inotify.max_user_instances≥512`, `fs.inotify.max_user_watches≥524288`, `fs.file-max≥2097152` (persist in `/etc/sysctl.d/`), plus the kind-standard `net.ipv4.ip_forward=1` and `net.bridge.bridge-nf-call-iptables=1`. `net.core.wmem_max≥4194304` lets the SIP sockets take the 4 MiB `SO_SNDBUF` the charts request (ADR-0033), and `net.core.rmem_max≥4194304` the 4 MiB `SO_RCVBUF`; both are the host's, a kind node cannot raise them. |
| Kernel modules | `sch_netem` (the latency and loss incidents), `br_netfilter`, `overlay`. |
| RAM | ~16 GiB. The cluster ceiling alone is 9.5 GiB (`cap-kind-memory.sh`, tunable via `TOTAL_CAP_MB` and the per-tier `*_CAP_MB`). |
| CPU | On a shared host, `weight-kind-cpu.sh` (run by `up`, re-runnable on a live cluster) raises each node container's CPU weight by tier (app to the maximum `cpu.weight` 10000, infra and edge to a raised one, control plane left at the default; `*_CPU_SHARES` in docker CPU shares, the weight each node got printed), so under host contention the app node's pods get the largest share instead of an equal share with every other node. |
| Network | The /16 of `$SIP_SUBNET` (default `172.20.0.0/16`, the bridge its lower /17) and the /24 of `$SIPEXT_SUBNET` **must be free** on the host — no vmnet / route / other docker network may overlap them; if one collides, set a free subnet (and the kind values with it). |
| Lab router | The FRR image `quay.io/frrouting/frr:10.6.2` (`TOR_IMAGE`), run by `up` as a host-network container; MetalLB `0.16.1` (`METALLB_VERSION`) installed by `deploy`. |

## Production deployment requirements (the front proxy's announcement)

The front proxy pair is active-active behind BGP (ADR-0056). A cluster it runs on
provides:

- **MetalLB in BGP mode with the frr-k8s backend**, its speakers on the proxy nodes,
  **BFD** on every session and **no BGP graceful restart** (a restarted speaker or
  frr-k8s withdraws its node's routes). The chart can render MetalLB's objects
  (`proxy.announce.metallb`) or leave them to the operator.
- **kube-proxy in iptables or nftables mode.** IPVS binds every LoadBalancer address
  to every node and refuses unlisted ports, so a worker could not reach the internal
  VIP; a kube-proxy replacement is checked for the same property first.
- **Top-of-rack routers with multipath** over the announcing nodes, hashing on layer 4,
  preferably resilient hashing (adding a node then moves few flows).
- **The internal VIP never routable from the external plane**: separate routers or a
  VRF per plane; the external VIP likewise kept off the internal plane.
- **No worker pod on a node that runs or ran a proxy**: `proxy.nodeSelector` names the proxy
  nodes (required) and the workers' node affinity keeps them off those; the VIPs stay on a proxy
  node's `lo`.
- **A proxy node's own listeners answer on the VIPs.** The VIPs are local addresses of a proxy
  node, so every service bound to all of its addresses answers on them, from the callers' plane
  on the external VIP: the kubelet (10250) and kube-proxy's `healthCheckNodePort` of the
  announce-only Services among them. The operator firewalls them on the external plane (or binds
  them to the node's internal address).
- **The internal plane's isolation** of ADR-0048: nothing on the external plane reaches
  a worker or the proxies' internal face.
- Each proxy node's external interface (`network.externalInterface`) and its external BGP
  peers' addresses on it (`network.externalNextHops`): the proxy's readiness requires the
  interface up and one of those next hops resolved; their BFD and keepalive traffic keeps them
  resolved. A lab router also holds a blackhole route per VIP below BGP, so a VIP nobody
  announces is unreachable instead of following a default route.
- The drain grace, `minReadySeconds` and the readiness probe set from the measured
  withdrawal and announcement latency (`proxy.announce.propagationMs`,
  `docs/TUNING.md`).
- **The route guard** (ADR-0056 §10, `proxy.routeGuard`, on by default) runs in the proxy pod
  with `NET_ADMIN` on the node's network namespace, as root, every other capability dropped; its
  init step needs the same. It drops the node's BFD control (UDP 3784) and BGP (TCP 179) toward the
  routers it names while the proxy is unfit, so:
  - the routers run BFD on the proxy nodes' sessions with a detection time inside the guard's
    bound (multiplier × receive interval, 900 ms by default) and withdraw a node's routes when its
    BFD goes down;
  - the routers update their forwarding tables within the bound's margin once BFD reads a session
    down: every detection path is checked at render against `routeGuard.withdrawWithinMs` with
    the BFD detection included but not the forwarding update, and the slowest path (a proxy cut
    from every worker: isolation 2 s + two readings 0.2 s + BFD 0.9 s = 3.1 s at the defaults)
    leaves 0.3 s under the 3.4 s bound and 0.4 s under the 3.5 s INVITE retransmission it
    protects; the crash path (dead bound 1 s + BFD 0.9 s) leaves 1.5 s;
  - the proxy nodes peer with those routers for MetalLB alone: a CNI's BGP to the same routers,
    or routes the nodes learn over those sessions (the routes to the workers and the callers
    among them), would be cut with it;
  - nothing else on a proxy node uses an nftables table named `route_guard` in the `inet` family.
- **Reading the route guard.** `route_guard_withdrawn` is 1 on a node held out of the routers'
  ECMP, and the guard container is then not Ready, nor is the pod; `route_guard_withdrawals_total{cause}` counts each withdrawal (`unfit`: the proxy named
  why, `expired`: no fit answer within `deadMs`, `start`) and `route_guard_restores_total` each
  return. The guard logs `node withdrawn: BFD and BGP to the routers dropped` with the cause and
  the proxy's reason, and `node let out: BFD and BGP to the routers pass again`. On the node,
  `nft list table inet route_guard` shows the routers let out and the drop counters, and
  `curl -s 127.0.0.1:9090/fitz` the proxy's own reading. A node that ran a guard and runs no
  proxy keeps the table withdrawing until a proxy pod starts there or
  `nft delete table inet route_guard` removes it.

## HA acceptance

HA call replication is exercised end to end by the HA tooling's incidents on the
kind lab (`kill_worker`, `kill_worker_held`, `force_delete`, `partition`, …) under
a run profile whose impact budgets state what each incident may cost; the
deterministic in-process failover harness (`crates/failover-harness`) is the
gate. The delta-translation logic of the membership watch is unit-tested fast
(`cargo test -p topology --features kube`). Triage of a failure:
[docs/testing/ha-acceptance.md](../docs/testing/ha-acceptance.md).

### One cluster at a time

The host runs **one** kind cluster named `sip-e2e` at a time. `kind-lab.sh up`
refuses to replace an existing one (a cluster left by a failed bring-up stays for
analysis); `kind-lab.sh down`, or `FORCE_RECREATE=1 kind-lab.sh up`, frees it.
