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
an unknown key or a wrongly typed value. The defaults leave every runner knob
at the runner's own default and schedule on any node, the workers spread
softly across nodes. `values-kind.yaml` is the kind lab: the docker bridges
`deploy/k8s/lib/net-env.sh` lays out (the check asserts the two agree), the
`tier` node placement of `deploy/k8s/cluster.yaml` with one worker per app
node, the lab's runner env (socket buffers, keepalive, limiter, drain,
heap profiling, the loadgen relay header, the stress limiter), replication on,
traces exported with the trace header trusted, the limiter's `NET_ADMIN` for
network chaos, and `masq-exempt`. `check/test_parity.py` holds the kind render
to what `deploy/k8s/manifests` renders through `run.sh`.

The SUT chart requires its network (`network.proxyVip`, `externalVip`, both
interfaces, `internalCidrs`, and `externalSubnet` for `masq-exempt`) while the
proxy is enabled; the collectors chart requires `hostStack.address` and
`clusterName`. A top-level `enabled` in each chart serves a parent's
`condition:`.

Object names are fixed (`b2bua-worker`, `sip-front-proxy`, `call-limiter`,
`rabbitmq`, `cdr-consumer`, `masq-exempt`; the collectors by their component
name, their ClusterRoles and bindings prefixed by the release namespace) and
every pod carries `app: <name>`, which the chaos and endurance tooling select
by: one SUT release per namespace.

## Extending the SUT chart from a product chart

A product chart depends on `siprust` and sets, per component (`worker`,
`proxy`, `callLimiter`, `cdr.consumer`):

- `image` (repository, tag, pullPolicy over the default `image`) and `command`
  / `args`: its own runner;
- `env`: plain env by name over what the chart derives (a name there wins),
  string values rendered through `tpl` (`{{ .Release.Namespace }}` works); a
  number goes in with `--set-string`. A null in user-supplied values (`-f`,
  `--set`) removes an env; a parent chart's own `values.yaml` cannot remove a
  subchart default (Helm keeps it), it overrides it;
- `extraEnv`: EnvVar entries appended (`valueFrom` and the like);
- `extraContainers`, `extraVolumes`, `extraVolumeMounts` (and
  `extraInitContainers` on the worker and the proxy): sidecars;
- on the worker and the proxy, `extraPorts`, `podSecurityContext`, `sysctls`,
  `priorityClassName`;
- `resources`, `nodeSelector`, `tolerations`, `podAnnotations` (over the
  chart's `prometheus.io/*`, key by key), `podLabels` (never `app`); the worker
  also `affinity` and `topologySpreadConstraints`;
- `enabled` on the worker, the proxy, the call limiter, the CDR chain
  (`cdr.enabled`) and the consumer (`cdr.consumer.enabled`).

A component on the external plane, such as a media relay, is a sidecar of the
proxy pod: it shares the edge node's network (hostNetwork) and the VRRP pair's
fate. `proxy.keepalived.extraVirtualIps` floats its own addresses with the two
VIPs (each on lo of both nodes, out of the kind masquerade with `masq-exempt`
on) and `proxy.keepalived.extraTrackScripts` makes its health weigh on the
pair's priority beside the proxy's.

The proxy's intake knobs are values: `proxy.recvShards`
(`PROXY_RECV_SHARDS`), `proxy.selfGate.cpsRate` / `cpsSize`
(`PROXY_SELF_GATE_CPS_RATE` / `_SIZE`); null keeps the runner default.
`check/test_downstream.py` builds such a product chart and checks each point.

## The proxy's cookie key

Without `proxy.hmac.existingSecret` the chart keeps the Secret
`sip-front-proxy-hmac`: its key is drawn on the first install and read back by
every upgrade through `lookup`, so `proxy.hmac.kid` applies to that first draw
only. A deployer that renders without a cluster (`helm template`, a GitOps
controller) gets an empty `lookup` and a new key at every render: it provides
the Secret itself and names it in `existingSecret`.

## The kind lab

`kind-lab.sh` wraps the bring-up:

```bash
deploy/helm/kind-lab.sh up       # cluster, sipext bridge, host preflight, images kind-loaded (run.sh up)
deploy/helm/kind-lab.sh image <tag> [docker build args]   # one more image, built and kind-loaded
deploy/helm/kind-lab.sh deploy   # both releases, Chaos Mesh, SIPp UAS (hakit uas-basic.xml), VIP + isolation smokes, series check
deploy/helm/kind-lab.sh template # the SUT release as deploy would install it, rendered
deploy/helm/kind-lab.sh chaos-mesh   # Chaos Mesh alone, pinned, chaos-mesh-values-kind.yaml
deploy/helm/kind-lab.sh down
```

A product chart that depends on `siprust` is the SUT release instead with
`SUT_CHART` (its directory), `SUT_RELEASE` (its release name) and
`SUT_SUBCHART` (the name of its `siprust` dependency): `deploy` and `template`
build its dependencies, pass the siprust kind values nested under that name,
then the product chart's own `values-kind.yaml`, then the helm args. A namespace
holds one SUT release: `deploy` refuses one another release owns. The VIP and
isolation smokes run when the release deploys the front proxy.

`up` and the SIPp generators are `deploy/k8s/run.sh`'s library functions;
`deploy` is the two helm invocations:

```bash
helm upgrade --install siprust-observability deploy/helm/siprust-observability \
  --kube-context kind-sip-e2e -n observability --create-namespace \
  -f deploy/helm/siprust-observability/values-kind.yaml --set clusterName=kind-sip-e2e --wait
helm upgrade --install siprust deploy/helm/siprust \
  --kube-context kind-sip-e2e -n sip-test --create-namespace \
  -f deploy/helm/siprust/values-kind.yaml --wait [--set worker.replicas=3 ...]
```

Every deploy passes the full value set: the values files and every `--set` the
run needs. A `--set` of an earlier deploy that the next one omits is gone
(`-f` without `--reuse-values` starts from the chart defaults); the runner
passes the whole set from its run profile.

`--wait` makes every workload's readiness part of the deploy, the CDR broker
and consumer included. SIPp load runs as before: `deploy/k8s/run.sh caps 50 60`.

## Checks

`just helm-check` (a few seconds, needs `helm`, `envsubst` and PyYAML): `helm
lint --strict` of both charts, the render contracts (`check/test_*.py`), the
downstream chart, the cookie key's `lookup` against a fake API server, the kind
values against `net-env.sh`, the parity of the kind render with the old
manifests, and the golden renders of the kind values (`check/golden/`). A
change to a render is reviewed in the golden diff: `just helm-golden` rewrites
them.
