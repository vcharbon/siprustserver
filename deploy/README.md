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

Every load generator (the SIPp UAC/UAS, the Rust loadgen) is a plain docker
container on the `sipext` bridge with a pinned IP and explicit `--cpus/--memory`
caps; the address plan is `deploy/k8s/lib/net-env.sh`.

## Binaries (containerizable today)

| Binary | Crate | Wiring |
|---|---|---|
| `sip-proxy-runner` | `crates/sip-proxy-runner` | Real UDP transport · `LoadBalancerStrategy` (HRW + HMAC-signed Record-Route stickiness) · EndpointSlice worker discovery (`ComposedWorkerRegistry`; a fixed `PROXY_WORKERS` list when set) · OPTIONS `HealthProbe` feeding the band observer · `EluCpsGate` self-gate · Prometheus `/metrics` |
| `b2bua-runner` | `crates/b2bua-runner` | Real UDP transport · system clock · `BufferedCdrWriter` over a discarding sink (no endurance leak) · `InMemoryCallStore` · `NoopLimiter` · `ScriptedDecisionEngine` routing all calls to the UAS · Prometheus `/metrics` |

Both are configured entirely by env vars (see the module docs at the top of each
`main.rs`). HA call replication is opt-in via `B2BUA_REPL=1`: real TCP repl
transport + `K8sMembership` EndpointSlice discovery + SIGTERM drain + `/ready`
probe (ADR-0011). The proxy refuses a boot it cannot honour (ADR-0046): it needs
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
| `SIP_SUBNET` | `172.20.0.0/16` | Internal kind docker bridge **/16**. The gateway and VIP are *derived* from it. |
| `SIP_GATEWAY` | `<prefix>.0.1` | Bridge gateway — the **bottom** of the subnet (kind hands node IPs out from the bottom up). |
| `PROXY_VIP` | `<prefix>.255.250` | keepalived VRRP VIP — parked at the **top** of the subnet so it can never collide with a node IP. |
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
above) before creating the cluster.

| Requirement | Minimum / note |
|---|---|
| Tools on `PATH` | `docker` (rootful), `docker compose`, `kind`, `kubectl`, `helm`, `uv` — `kind-lab.sh` checks the first four and `helm`; `uv` runs the HA tooling. |
| **cgroups** | **v2 strongly preferred** (default on Ubuntu 22.04+, Debian 12, Fedora 35+, RHEL/Rocky 9). On cgroup **v1** the host must boot with `cgroup_enable=memory swapaccount=1`, otherwise `cap-kind-memory.sh`'s `--memory-swap` node ceiling is **silently ignored**. Verify: `docker info \| grep -i cgroup` → `Cgroup Version: 2`. |
| **sysctls** | A 6-node kind cluster (`cluster.yaml`) exhausts default inotify/fd limits. Set `fs.inotify.max_user_instances≥512`, `fs.inotify.max_user_watches≥524288`, `fs.file-max≥2097152` (persist in `/etc/sysctl.d/`), plus the kind-standard `net.ipv4.ip_forward=1` and `net.bridge.bridge-nf-call-iptables=1`. `net.core.wmem_max≥4194304` lets the SIP sockets take the 4 MiB `SO_SNDBUF` the charts request (ADR-0033), and `net.core.rmem_max≥4194304` the 4 MiB `SO_RCVBUF`; both are the host's, a kind node cannot raise them. |
| Kernel modules | `sch_netem` (the latency and loss incidents), `br_netfilter`, `overlay`. |
| RAM | ~16 GiB. The cluster ceiling alone is 9.5 GiB (`cap-kind-memory.sh`, tunable via `TOTAL_CAP_MB` and the per-tier `*_CAP_MB`). |
| Network | `$SIP_SUBNET` (default `172.20.0.0/16`) **must be free** on the host — no vmnet / route / other docker network may overlap it; if it collides, set a free `SIP_SUBNET` (and the kind values with it). |

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
