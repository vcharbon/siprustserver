# Helm charts

Two charts deploy a cluster:

| Chart | Deploys | Namespace |
|---|---|---|
| `siprust/` | the front proxy pair (keepalived VRRP, dual-faced), the B2BUA worker pool, the call limiter, the CDR broker and consumer, and on kind the `masq-exempt` DaemonSet | `sip-test` on the lab |
| `siprust-observability/` | the in-cluster collectors: vmagent, kube-state-metrics, node-exporter, fluent-bit, writing to a VictoriaMetrics / VictoriaLogs pair outside the cluster | `observability` |

The stores and Grafana stay outside the cluster (`deploy/observability/stack`,
docker compose), so their history survives a cluster teardown. Every bring-up
installs both releases: a cluster never runs unobserved.

The raw manifests of `deploy/k8s/manifests` and `deploy/observability/kind-addons`
render the same objects through `envsubst` and `sed`; a namespace holds one or
the other, never both (helm refuses objects it does not own).

## Values

Each chart's `values.yaml` documents every key and `values.schema.json` refuses
an unknown key or a wrongly typed value. `values-kind.yaml` is the kind lab:
the docker bridges `deploy/k8s/lib/net-env.sh` lays out (the check asserts the
two agree) and the lab settings (replication on, traces exported and the trace
header trusted, the limiter's `NET_ADMIN` for network chaos, `masq-exempt`).

The SUT chart requires its network (`network.proxyVip`, `externalVip`, both
interfaces, `internalCidrs`, and `externalSubnet` for `masq-exempt`) while the
proxy is enabled; the collectors chart requires `hostStack.address` and
`clusterName`.

Object names are fixed (`b2bua-worker`, `sip-front-proxy`, `call-limiter`,
`rabbitmq`, `cdr-consumer`, `masq-exempt`; the collectors by their component
name) and every pod carries `app: <name>`, which the chaos and endurance
tooling select by: one SUT release per namespace.

## Extending the SUT chart from a product chart

A product chart depends on `siprust` and sets, per component (`worker`,
`proxy`, `callLimiter`, `cdr.consumer`):

- `image` (repository, tag, pullPolicy over the default `image`) and `command`
  / `args`: its own runner;
- `env`: plain env by name over what the chart derives (a name there wins),
  string values; a null in user-supplied values (`-f`, `--set`) removes a
  default, a parent chart's own `values.yaml` overrides it instead (Helm keeps a
  subchart default its parent sets to null);
- `extraEnv`: EnvVar entries appended (`valueFrom` and the like);
- `extraContainers`, `extraVolumes`, `extraVolumeMounts` (and
  `extraInitContainers` on the worker and the proxy): sidecars such as a media
  relay beside the worker;
- `resources`, `nodeSelector`, `tolerations`, `podAnnotations`, `podLabels`
  (the worker also `affinity` and `topologySpreadConstraints`);
- `enabled` on the worker, the proxy, the call limiter, the CDR chain (`cdr.enabled`) and
  the consumer (`cdr.consumer.enabled`).

The proxy's intake knobs are values: `proxy.recvShards`
(`PROXY_RECV_SHARDS`), `proxy.selfGate.cpsRate` / `cpsSize`
(`PROXY_SELF_GATE_CPS_RATE` / `_SIZE`); null keeps the runner default.
`check/test_downstream.py` builds such a product chart and checks each point.

## The kind lab

`kind-lab.sh` wraps the bring-up:

```bash
deploy/helm/kind-lab.sh up       # cluster, sipext bridge, host preflight, images kind-loaded (run.sh up)
deploy/helm/kind-lab.sh deploy   # both releases, SIPp UAS, VIP + isolation smokes, series check
deploy/helm/kind-lab.sh down
```

`up` and the SIPp generators are `deploy/k8s/run.sh`'s library functions;
`deploy` is the two helm invocations:

```bash
helm upgrade --install siprust-observability deploy/helm/siprust-observability \
  --kube-context kind-sip-e2e -n observability --create-namespace \
  -f deploy/helm/siprust-observability/values-kind.yaml --wait
helm upgrade --install siprust deploy/helm/siprust \
  --kube-context kind-sip-e2e -n sip-test --create-namespace \
  -f deploy/helm/siprust/values-kind.yaml --wait [--set worker.replicas=3 ...]
```

A re-deploy is the same `helm upgrade` with the same values files plus the
`--set` of the run: the release holds every value, so no knob of an earlier
deploy is lost. The proxy's cookie signing key (Secret `sip-front-proxy-hmac`)
is drawn on the first install and read back by every upgrade.

SIPp load runs as before: `deploy/k8s/run.sh caps 50 60`.

## Checks

`just helm-check` (a few seconds, needs `helm` and PyYAML): `helm lint
--strict` of both charts, the render contracts (`check/test_*.py`), the
downstream chart, the kind values against `net-env.sh`, and the golden renders
of the kind values (`check/golden/`). A change to a render is reviewed in the
golden diff: `just helm-golden` rewrites them.
