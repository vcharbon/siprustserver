# Helm charts

Two charts deploy a cluster:

| Chart | Deploys | Namespace |
|---|---|---|
| `siprust/` | the front proxy pair (keepalived VRRP, dual-faced), the B2BUA worker pool and its ingress NetworkPolicy, the call limiter, the CDR broker and consumer, and on kind the `masq-exempt` DaemonSet | `sip-test` on the lab |
| `siprust-observability/` | the in-cluster collectors: vmagent, kube-state-metrics, node-exporter, fluent-bit, writing to a VictoriaMetrics / VictoriaLogs pair outside the cluster | `observability` |

The stores and Grafana stay outside the cluster (`deploy/observability/stack`,
docker compose), so their history survives a cluster teardown. Every bring-up
installs both releases: a cluster never runs unobserved.

## Values

Each chart's `values.yaml` documents every key and `values.schema.json` refuses
an unknown key or a wrongly typed value. The defaults leave every runner knob
at the runner's own default and schedule on any node, the workers spread
softly across nodes. `values-kind.yaml` is the kind lab: the docker bridges
`deploy/k8s/lib/net-env.sh` lays out (the check asserts the two agree), the
`tier` node placement of `deploy/k8s/cluster.yaml` with one worker per app
node, the lab's runner env (socket buffers, keepalive, limiter, drain,
heap profiling, the loadgen relay header, the stress limiter), replication on,
traces exported with the trace header trusted, and `masq-exempt`.

The SUT chart requires its network (`network.proxyVip`, `externalVip`, both
interfaces, `internalCidrs`, and `externalSubnet` for `masq-exempt`) and,
with the workers' NetworkPolicy on, `worker.networkPolicy.proxyNodeCidrs`
while the proxy is enabled; the collectors chart requires `hostStack.address` and
`clusterName`. A top-level `enabled` in each chart serves a parent's
`condition:`.

Object names are fixed (`b2bua-worker`, `sip-front-proxy`, `call-limiter`,
`rabbitmq`, `cdr-consumer`, `masq-exempt`; the collectors by their component
name, their ClusterRoles and bindings prefixed by the release namespace) and
every pod carries `app: <name>`, which the HA tooling's incidents select by:
one SUT release per namespace.

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

The proxy's cookie key lives in the Secret `proxy.hmac.existingSecret` names,
which the chart requires: `helm template` and a GitOps controller render
without a cluster, so a key drawn at render would change at every render. Only
a release installed against its cluster may set `proxy.hmac.drawInCluster`
(the kind lab does): the chart then keeps the Secret `sip-front-proxy-hmac`,
its key drawn on the first install and read back by every upgrade through
`lookup`, `proxy.hmac.kid` applying to that first draw only.

## Deployment requirements

- **The internal SIP plane is isolated** (ADR-0048). The proxy trusts what
  reaches its internal socket, so only the workers and the proxies reach the
  internal VIP. The proxies are hostNetwork: no NetworkPolicy selects them, so
  a host firewall on the proxy nodes or a CNI host policy enforces it (the
  kind lab: `masq-exempt`'s drop rule and the docker bridges' isolation).
- **The workers' ingress** is the chart's NetworkPolicy
  (`templates/worker-network-policy.yaml`, `worker.networkPolicy`, on by
  default): SIP from the internal VIP and `proxyNodeCidrs` (the proxies'
  OPTIONS probes and dead notices leave from their node's own address),
  replication from the worker pods, the probe port from
  `monitoringNamespace`; the kubelet's probes come from the pod's own node,
  which no NetworkPolicy blocks. It holds only where the CNI implements
  NetworkPolicy (kindnet does); a CNI that does not ignores it. A CNI that
  gives node addresses an identity of their own must let CIDR rules match them
  (Cilium: `policy-cidr-match-mode: nodes`). The replication port has no peer
  authentication: this policy is what confines it to the workers.
- **The operator routes** (cordon, release-all, the live calls; ADR-0050 §10)
  listen on the worker's loopback (`B2BUA_ADMIN`, 127.0.0.1:9093), never on a
  Service: `kubectl port-forward pod/<worker> 19093:9093`, or `kubectl exec`.

## Render-time relations

`siprust/templates/checks.yaml` holds the relations that span a pod spec and a
runner's env, or two components, and fails the render naming every value
involved: the worker's and the proxy's `terminationGracePeriodSeconds` against
their drain bounds, replication and its connection cap against the worker
count, the CDR queue's `x-max-length` on both sides, the worker's RSS ceilings and the proxy's
branch memo against their memory limits, the workers' consult deadline against
the proxy's dead verdict, the limiter lease against the workers' refresh, the
one INVITE bound of workers and proxies, and the cookie key above. Each runner
checks its own relations at startup on its final configuration. A value set
through `extraEnv` is outside the render relations: keeping it consistent is
the operator's responsibility. [docs/TUNING.md](../../docs/TUNING.md) says how
to change each variable safely.

## The kind lab

`kind-lab.sh` wraps the bring-up:

```bash
deploy/helm/kind-lab.sh up       # cluster, sipext bridge, host preflight, images kind-loaded
deploy/helm/kind-lab.sh images   # the images of `up` rebuilt from the tree and kind-loaded, cluster kept
deploy/helm/kind-lab.sh image <tag> [docker build args]   # one more image, built and kind-loaded
deploy/helm/kind-lab.sh deploy   # both releases, Chaos Mesh, SIPp UAS (hakit uas-basic.xml), image roll, VIP + isolation smokes, series check
deploy/helm/kind-lab.sh template # the SUT release as deploy would install it, rendered
deploy/helm/kind-lab.sh chaos-mesh   # Chaos Mesh alone, pinned, chaos-mesh-values-kind.yaml
deploy/helm/kind-lab.sh down
```

A product chart that depends on `siprust` is the SUT release instead with
`SUT_CHART` (its directory), `SUT_RELEASE` (its release name) and
`SUT_SUBCHART` (the name of its `siprust` dependency): `deploy` and `template`
build its dependencies, pass the siprust kind values nested under that name,
then the product chart's own `values-kind.yaml` when it has one, then the helm
args (`lib/sut.sh`). A namespace
holds one SUT release: `deploy` refuses one another release owns. The VIP and
isolation smokes run when the release deploys the front proxy.

`up` and `down` are `lib/cluster.sh`, the smokes `lib/smokes.sh`; `deploy`
needs the host stack up (`deploy/observability/install.sh --bootstrap`) and is
the two helm invocations:

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
and consumer included. The series check after it is `hakit obs-check` on the
`proxy` scope, or `sut` when the release runs no proxy. Load and incidents are
the HA tooling's runs (`tooling/hakit`, deploy/README.md).

A reloaded image keeps its tag, so the pod templates do not change and helm
rolls nothing. After the SUT release, `deploy` compares each live pod's
`imageID` with the digests its image's tag names on the pod's node (`crictl
inspecti` in the kind node) and restarts every Deployment, StatefulSet and
DaemonSet with a pod on a replaced image, then waits for each rollout
(`lib/image-roll.sh`). It logs the workloads it rolled, or that nothing rolled.
An unchanged image reloads at the same digest, so a redeploy with nothing
rebuilt restarts nothing. A StatefulSet's pods below its partition are left to
the partition; a workload updated `OnDelete` is named and left; a node that
answers no image for a live pod on it fails the deploy.

## Checks

`just helm-check` (a few seconds, needs `helm` and PyYAML): `helm lint
--strict` of both charts, the render contracts (`check/test_*.py`), the
downstream chart, the cookie key's `lookup` against a fake API server, the kind
values against `net-env.sh`, the lab's SUT helpers (`lib/sut.sh`) and the golden
renders of the kind values (`check/golden/`). A change to a render is reviewed
in the golden diff: `just helm-golden` rewrites them.
