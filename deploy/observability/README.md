# Observability — VictoriaMetrics + Grafana (Rust SUT)

Self-contained metrics/logs/traces stack for the Rust SIP SUT running in kind:
host-side storage + UI (this directory), in-cluster scraper/forwarder (the
`siprust-observability` helm release, `deploy/helm`).

```
            host (docker compose)                     kind cluster (sip-e2e)
            ────────────────────                      ──────────────────────
 Grafana :3333 ◀── PromQL ── VictoriaMetrics :8428 ◀── remote_write ── vmagent (Deployment)
               ◀── LogsQL ── VictoriaLogs   :9428 ◀── HTTP push   ── fluent-bit (DaemonSet)
               ◀── Jaeger ── VictoriaTraces :10428 ◀── OTLP/HTTP  ── b2bua-worker + sip-front-proxy

                                              vmagent k8s SD scrapes (helm release):
                                                • pods annotated prometheus.io/scrape=true
                                                  (sip-front-proxy :9090, b2bua-worker :9091,
                                                   kube-state-metrics, node-exporter)
                                                • kubelet /metrics + cAdvisor (every node)
```

## Bring-up

The host stack outlives every cluster: it attaches to the `kind` docker network,
so it comes up after the first `deploy/helm/kind-lab.sh up` and stays up across
cluster recreates (its data dir is gitignored and survives `down`).
`kind-lab.sh deploy` installs the in-cluster collectors and refuses to run while
the host stack is down.

```bash
./install.sh --bootstrap   # data dirs, sipext network, compose up --build, health wait
./install.sh --apply       # idempotent: reload VictoriaMetrics' scrape config
./install.sh --status      # probe endpoints + dump scrape targets
./install.sh --down        # tear the host stack down (data kept)
```

- Grafana:        http://localhost:3333  (anonymous admin)
- VictoriaMetrics: http://localhost:8428/vmui
- VictoriaLogs:    http://localhost:9428/select/vmui
- VictoriaTraces:  http://localhost:10428  (OTLP ingest `/insert/opentelemetry/v1/traces`)

## Dashboards (auto-provisioned, mounted in place)

The dashboards live on the HA tooling search path (ADR-0049 §7.5): the base set
in `tooling/hakit/content/dashboards/`, a product's in the `dashboards/`
directory of each of its `HAKIT_PATH` entries. Grafana's file provider reads
`/hakit/dashboards` recursively; each directory is bind-mounted read-only at
`/hakit/dashboards/<i>`, `i` its position on the path. The base is in the
compose file; with `HAKIT_PATH` set, `install.sh` runs
`hakit dashboards --compose-override` (it validates the path first) and adds
the override it writes. Nothing is copied: an edit reloads within ~10s.

Every dashboard carries the generic block, and `hakit validate` refuses one
without it: the `Incident window` annotation (regions from
`sip_chaos_window == 1`), the `Incident phase` annotation (points from
`sip_chaos_event`), the tag `hakit` and the link listing every dashboard so
tagged with the time range kept.

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

The SUT chart sets these on the proxy (:9090) and the worker (:9091).

## Notes / caveats

- One stack per host. The host container names (`grafana`, `victoriametrics`, …)
  and ports (3333/8428/9428/10428) are shared with sipjsserver's observability
  stack — only one can run at a time. `install.sh --down` the other first.
- Use `127.0.0.1`, not `localhost`, when curling the host endpoints
  (`localhost` → IPv6 `::1` gets connection-reset against these containers).
- Per-call traces (ADR-0026) are SAMPLED, so the VictoriaTraces datasource is
  near-empty by design: the default draw is 1e-4. To see a specific call, send
  `X-Trace-Sample: 1.0` on its INVITE (the kind values set `SIP_TRACE_HEADER=1`,
  which is what makes the lab honour that header) or return `"trace": true` from the
  decision engine. See [docs/observability.md](../../docs/observability.md).
