# Containerized Rust SIP SUT + thin k8s load runner

Production-shaped, containerizable binaries for the Rust front proxy and B2BUA,
plus a minimal kind/k8s runner that deploys the full stack and drives SIPp load —
the Rust-side counterpart to sipjsserver's k8s endurance harness.

```
sipp UAC ──INVITE──▶ sip-front-proxy ──INVITE──▶ b2bua-worker pool ──INVITE──▶ sipp UAS
 (tier=load)          (tier=edge, LB +            (tier=app, real CDR          (tier=load)
                       HMAC Record-Route           buffer, /metrics)
                       stickiness, OPTIONS
                       health probe, /metrics)
```

## Binaries (containerizable today)

| Binary | Crate | Wiring |
|---|---|---|
| `sip-proxy-runner` | `crates/sip-proxy-runner` | Real UDP transport · `LoadBalancerStrategy` (HRW + HMAC-signed Record-Route stickiness) · `StaticWorkerRegistry` · OPTIONS `HealthProbe` feeding the band observer · `AlwaysAdmitGate` · Prometheus `/metrics` |
| `b2bua-runner` | `crates/b2bua-runner` | Real UDP transport · system clock · `BufferedCdrWriter` over a discarding sink (no endurance leak) · `InMemoryCallStore` · `NoopLimiter` · `ScriptedDecisionEngine` routing all calls to the UAS · Prometheus `/metrics` |

Both are configured entirely by env vars (see the module docs at the top of each
`main.rs`). **HA call replication is now wirable** in `b2bua-runner` (opt-in via
`B2BUA_REPL=1`: real TCP repl transport + `K8sMembership` EndpointSlice
discovery + SIGTERM drain + `/ready` probe — ADR-0011 / the chaos suite below).
Still deferred per ADR-0009/0010 and **not** wired here: the HTTP call-control
decision adapter, the real sliding-window limiter, the real proxy self-gate, the
AIMD per-worker bucket, proxy VIP/HA, and the proxy's own k8s registry (it still
takes IP literals via `PROXY_WORKERS`).

### Image

One image carries both binaries; the k8s `command` selects which to run.

```bash
docker build -f deploy/docker/Dockerfile -t siprustserver:dev .   # context = repo root
```

## Thin k8s runner — `deploy/k8s/run.sh`

```bash
cd deploy/k8s
./run.sh all 30 50 100 200 400   # up + deploy + sweep caps, 30s sampling each
# or step by step:
./run.sh up                      # (re)create cluster, build+load images
./run.sh deploy                  # apply uas + workers + proxy, wait ready
./run.sh caps 200 30             # 200 cps for 30s, sample CPU%/RSS
./run.sh sweep 30 50 100 200 400
./run.sh down                    # delete the cluster
```

Each cap prints per-pod CPU% and RSS(MB) (sampled from `/proc/1` via
`kubectl exec`, so no metrics-server needed). App metrics:
`kubectl -n sip-test port-forward deploy/sip-front-proxy 9090 & curl localhost:9090/metrics`.

### Configuration (env vars)

Network and port wiring is a single source of truth in `deploy/k8s/lib/net-env.sh`
(sourced by `run.sh`, `endurance.sh`, `chaos.sh`), so the three runners can never
drift. Everything is overridable from the environment; defaults reproduce the
historical layout.

| Env var | Default | Meaning |
|---|---|---|
| `SIP_SUBNET` | `172.20.0.0/16` | Internal kind docker bridge **/16**. The gateway and VIP are *derived* from it — change this one knob to move the whole cluster off a colliding subnet. |
| `SIP_GATEWAY` | `<prefix>.0.1` | Bridge gateway — the **bottom** of the subnet (kind hands node IPs out from the bottom up). Derived; override only for a bespoke address. |
| `PROXY_VIP` | `<prefix>.255.250` | keepalived VRRP VIP — parked at the **top** of the subnet so it can never collide with a node IP. This top/bottom split is the invariant the layout depends on. Derived; overridable. |
| `PROXY_TARGET` | `$PROXY_VIP` | What the SIPp UAC streams dial. |
| `SIP_PORT` | `5060` | UDP port the front-proxy **LB** listens on (workers and UAC streams dial the VIP on this port). The internal worker/UAS SIP port stays 5060 regardless. |
| `CLUSTER` / `NS` | `sip-e2e` / `sip-test` | kind cluster name / namespace. |
| `SUT_IMAGE` | `siprustserver:dev` | Rust SUT image tag. |
| `WORKER_REPLICAS` | `2` | b2bua worker pool size. |
| `OBS_ENABLE` | `1` | Bring up the observability stack (Grafana :3333, VictoriaMetrics :8428). `0` to skip. |
| `REPL_ENABLE` / `REPL_PORT` | `0` / `9092` | HA call replication (chaos suite sets `1`). |

Host preflight knobs (see system requirements below):

| Env var | Default | Meaning |
|---|---|---|
| `PREFLIGHT_STRICT` | `0` | `1` makes a failed cgroup/sysctl check **abort** `up` instead of warning. |
| `PREFLIGHT_FIX_SYSCTLS` | `0` | `1` auto-raises any low sysctl (needs root / passwordless sudo). |
| `REQUIRED_SYSCTLS` | `fs.inotify.max_user_instances=512 fs.inotify.max_user_watches=524288 fs.file-max=2097152` | Space-separated `key=min` pairs checked at `up`. |

```bash
# Example: run the whole stack on a different subnet + LB port (e.g. 172.20 collides
# with a VMware vmnet), enforcing host prerequisites:
SIP_SUBNET=10.80.0.0/16 SIP_PORT=5080 PREFLIGHT_STRICT=1 ./run.sh up
SIP_SUBNET=10.80.0.0/16 SIP_PORT=5080 ./run.sh deploy
# VIP derives to 10.80.255.250, gateway to 10.80.0.1.
```

## System requirements (kind host)

The runner is developed on WSL2 but runs on any Linux box with rootful Docker
(e.g. a VMware VM). `run.sh up` runs a host preflight (cgroups + sysctls,
advisory by default — see the knobs above) before creating the cluster.

| Requirement | Minimum / note |
|---|---|
| Tools on `PATH` | `docker` (rootful), `docker compose`, `kind`, `kubectl`, `envsubst` (gettext) — checked by `run.sh` preflight. |
| **cgroups** | **v2 strongly preferred** (default on Ubuntu 22.04+, Debian 12, Fedora 35+, RHEL/Rocky 9). On cgroup **v1** the host must boot with `cgroup_enable=memory swapaccount=1`, otherwise `cap-kind-memory.sh`'s `--memory-swap` node ceiling is **silently ignored**. Verify: `docker info \| grep -i cgroup` → `Cgroup Version: 2`. |
| **sysctls** | A 6-node kind cluster (`cluster.yaml`) exhausts default inotify/fd limits. Set `fs.inotify.max_user_instances≥512`, `fs.inotify.max_user_watches≥524288`, `fs.file-max≥2097152` (persist in `/etc/sysctl.d/`), plus the kind-standard `net.ipv4.ip_forward=1` and `net.bridge.bridge-nf-call-iptables=1`. |
| Kernel modules | `sch_netem` (needed by `chaos.sh` `tc netem`), `br_netfilter`, `overlay`. |
| RAM | ~16 GiB. The cluster ceiling alone is 9.5 GiB (`cap-kind-memory.sh`, tunable via `TOTAL_CAP_MB` and the per-tier `*_CAP_MB`). |
| Network | `$SIP_SUBNET` (default `172.20.0.0/16`) **must be free** on the host — no vmnet / route / other docker network may overlap it. On VMware, check `ip route \| grep 172.20` and `docker network ls`; if it collides, set a free `SIP_SUBNET`. |

## HA replication chaos suite — `deploy/k8s/chaos.sh`

Goal-3 (S11) acceptance: the **real-clock, real-TCP, real-k8s** test of
peer-to-peer call replication (ADR-0011). It stands up the stack with
replication **on** (`REPL_ENABLE=1`, ≥2 workers), drives long-hold dialogs, then
kills the worker holding a dialog mid-call and asserts the dialog **survives**
(the in-dialog BYE lands on the backup worker that holds the replica).

```bash
cd deploy/k8s
./chaos.sh failover     # up + deploy(repl) + hold-failover under pod kill + assert
./chaos.sh up           # just cluster + images (repl)
./chaos.sh kill         # inject one worker kill against a running stack
./chaos.sh down
# knobs: CALLS=30 CPS=3 KILL_TARGET=b2bua-worker-0 PASS_THRESHOLD=90 KEEP=1
```

It is a **shell script, not a `cargo test`** — real kind clusters + image builds
are slow and WSL2-flaky, so it must never gate `cargo test --workspace`. The
delta-translation logic it exercises *is* unit-tested fast (`cargo test -p
topology --features kube`); chaos.sh is the end-to-end signal you run on demand.

When `REPL_ENABLE=1`, each worker discovers peers via the **K8sMembership**
EndpointSlice informer (RBAC in `manifests/15-worker-rbac.yaml`), serves its
changelog on the repl TCP port, reports `NotReady` via `/ready` until
re-hydrated, and `Draining` on SIGTERM. See `deploy/k8s/manifests/20-worker.yaml`
and the b2bua-runner module docs for the env grammar
(`B2BUA_REPL*`/`B2BUA_PEERS`).

## Sharing with sipjsserver (vendored, divergeable)

This runner is **independent** (its own bash + manifests). As of S11 the two
pieces it used to **symlink** from a sibling `sipjsserver` checkout are now
**vendored copies** in-tree, so the runner stands alone and the artifacts may
diverge (the chaos/endurance scenarios especially):

| Artifact | Location (was a symlink) | Note |
|---|---|---|
| Cluster topology | `deploy/k8s/cluster.yaml` | Copied from `sipjsserver/tests/k8s/cluster.yaml`; **same cluster name `sip-e2e`** (WSL one-cluster switch) |
| SIPp scenarios | `deploy/k8s/sipp/scenarios/` | Copied from the sipjs sipp chart; free to diverge |
| SIPp image | built in `run.sh` from `deploy/k8s/sipp/Dockerfile` | One `sipp:dev` driver, built from the vendored context |

To still compare Node vs Rust head-to-head, keep the *scenarios* byte-identical
where you want comparable results; everything else is SUT-specific and lives
here.

### WSL one-cluster constraint

This host runs **one** kind cluster at a time, and both SUTs deliberately use the
cluster name **`sip-e2e`**. `run.sh up` (and `all`) **first** run
`kind delete cluster --name sip-e2e` — destroying any existing `sip-e2e` cluster,
including sipjsserver's. That is the intended "stop the other, run this" switch;
it mirrors how the sipjs runner tears down first at BRINGUP. Run `./run.sh down`
when finished so the host is free for the other SUT.

> The two runners are interchangeable by construction: same cluster name, same
> SIPp driver, same scenarios — only the SUT image and its manifests differ. To
> compare Node vs Rust, run one runner's `all`, capture results, `down`, then run
> the other.
