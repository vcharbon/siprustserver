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
`main.rs`). Deferred per ADR-0009/0010 and **not** wired here: the HTTP
call-control decision adapter, the real sliding-window limiter, HA/Redis call
replication, the real proxy self-gate, the AIMD per-worker bucket, proxy VIP/HA.

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

## Sharing with sipjsserver (the "best way")

This runner is **independent** (its own bash + manifests) but **shares the two
pieces that must stay identical** with sipjsserver, via relative symlinks
(assumes both repos are sibling checkouts under the same parent):

| Shared artifact | Symlink | Why shared |
|---|---|---|
| Cluster topology | `deploy/k8s/cluster.yaml` → `sipjsserver/tests/k8s/cluster.yaml` | Same node tiers + the load-node port mapping; **same cluster name `sip-e2e`** |
| SIPp scenarios | `deploy/k8s/scenarios` → `sipjsserver/tests/k8s/charts/sipp/scenarios` | Identical call flows ⇒ comparable results across SUTs |
| SIPp image | built in `run.sh` from `sipjsserver/.../charts/sipp` | One `sipp:dev` driver for both |

Everything SUT-specific (image build, worker/proxy manifests, env, metrics ports)
lives here in `siprustserver` and never touches the Node repo.

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
