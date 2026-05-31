# Observability — VictoriaMetrics + Grafana (Rust SUT)

Self-contained metrics/logs stack for the Rust SIP SUT running in kind. This is
the siprustserver-owned copy (it no longer depends on the sibling sipjsserver
checkout). Host-side storage + UI, in-cluster scraper/forwarder.

```
            host (docker compose)                     kind cluster (sip-e2e)
            ────────────────────                      ──────────────────────
 Grafana :3333 ◀── PromQL ── VictoriaMetrics :8428 ◀── remote_write ── vmagent (Deployment)
               ◀── LogsQL ── VictoriaLogs   :9428 ◀── HTTP push   ── fluent-bit (DaemonSet)

                                              vmagent k8s SD scrapes:
                                                • pods annotated prometheus.io/scrape=true
                                                  (sip-front-proxy :9090, b2bua-worker :9091,
                                                   kube-state-metrics, node-exporter)
                                                • kubelet /metrics + cAdvisor (every node)
```

## Automatic deployment

`deploy/k8s/run.sh up` (and `all`) calls `install.sh --bootstrap` after the
cluster is created, so scraping + dashboards are always wired against the fresh
cluster. Recreating the kind cluster wipes the in-cluster `observability`
namespace, so the addons are re-applied on every `up`. The host docker-compose
stack persists across runs (its data dir is gitignored but survives `down`).

Skip with `OBS_ENABLE=0 ./run.sh up`. Re-apply by hand with `./run.sh obs`.

## Manual control

```bash
./install.sh --bootstrap   # bring up host stack + apply kind-addons
./install.sh --apply       # idempotent: reload dashboards/scrape + reapply addons
./install.sh --status      # probe endpoints + dump scrape targets
./install.sh --down        # tear down host stack + delete observability ns
```

- Grafana:        http://localhost:3333  (anonymous admin)
- VictoriaMetrics: http://localhost:8428/vmui
- VictoriaLogs:    http://localhost:9428/select/vmui

## Dashboards (auto-provisioned, bind-mounted)

`stack/grafana/dashboards/*.json` are bind-mounted into Grafana and reload within
~10s — edit the JSON in this repo, no restart/upload needed.

| UID | Title | Built from |
|---|---|---|
| `sip-front-proxy` | SIP front-proxy — Load balancer (Rust) | `sip_messages_total`, `sip_routing_decision_total`, `sip_routing_duration_seconds`, `sip_proxy_*`, `sip_worker_health` |
| `b2bua-worker` | B2BUA — Worker (Rust) | `b2bua_active_calls`, `b2bua_call_*`, `b2bua_dispatch_*`, `b2bua_handler_timeouts_total`, `b2bua_cdr_*` |
| `k8s-state` | Kubernetes — state & per-pod resources | kube-state-metrics + cAdvisor (SUT-independent) |
| `kind-nodes` | Kind — Node infrastructure | node-exporter (SUT-independent) |

The app dashboards are adapted from sipjsserver's but track the **Rust**
exporters' metric names and label shapes — notably `sip_messages_total` carries
a single `label` dimension holding `direction:result` (the Rust proxy does not
split into separate `direction`/`result`/`method` labels), and `sip_worker_health`
is a per-state count gauge (no `worker_id`). The Rust routing-duration histogram
emits only the `+Inf` bucket, so the latency panel shows the windowed average
rather than quantiles.

## How scraping works

vmagent auto-discovers any pod carrying:

```yaml
metadata:
  annotations:
    prometheus.io/scrape: "true"
    prometheus.io/port:   "<port>"     # the /metrics container port
```

The Rust manifests already set these: `deploy/k8s/manifests/30-proxy.yaml`
(:9090) and `20-worker.yaml` (:9091).

## Notes / caveats

- One stack per host. The host container names (`grafana`, `victoriametrics`, …)
  and ports (3333/8428/9428/10428) are shared with sipjsserver's observability
  stack — only one can run at a time. `install.sh --down` the other first.
- Use `127.0.0.1`, not `localhost`, when curling the host endpoints
  (`localhost` → IPv6 `::1` gets connection-reset against these containers).
- VictoriaTraces/OTel tracing is provisioned but the Rust binaries don't emit
  spans yet (deferred per ADR-0009/0010) — that datasource will be empty.
