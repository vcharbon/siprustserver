# Front-proxy HA (VRRP VIP) + finish the limiter exercise

## Context

The endurance run surfaced two things:

1. **`kill_proxy` is a real SPOF.** The proxy stamps its **own pod IP** into
   Record-Route ([30-proxy.yaml:48](deploy/k8s/manifests/30-proxy.yaml#L48),
   `PROXY_ADVERTISE=$(POD_IP):5060`). When the single proxy pod is killed it comes
   back with a new IP, so every dialog established before the kill is pinned to the
   dead IP — their in-dialog BYE/keepalives black-hole and **retransmit until the
   call is reaped** (the ~10/s tail you saw, dominated by long-hold calls). The
   suite hid this behind a lenient 60% `kill_proxy` bar. The architecture goal is
   **full resilience**, so we implement proper proxy HA and delete the caveat.

2. **The limiter exercise never ran.** The `sipp-uac-limiter` pod failed at
   startup: `Unsupported keyword '{"id":"endurance-limiter","limit":20}'` — the
   vendored SIPp parses the JSON array's `[...]` as a keyword reference (my earlier
   "3.7.7 handles literal brackets" assumption was wrong). Every limiter event read
   `0` and failed. The fix is the `-key xapi` workaround the codebase already
   documents.

The limiter b2bua/worker/dashboard work from the previous plan is already
implemented and compiles/tests green; this plan finishes the limiter scenario and
adds proxy HA, then runs a clean 2 hours.

**Key enabler (no proxy code change):** the Rust proxy already splits sockets —
forwarding uses `PROXY_LISTEN`, while OPTIONS health probes use a separate
ephemeral socket bound to `0.0.0.0:0`
([sip-proxy-runner/src/main.rs:160-167](crates/sip-proxy-runner/src/main.rs#L160)).
So with `PROXY_LISTEN=VIP` the master sources forwarded SIP from the VIP, but
*both* proxies keep health-probing workers from their own node IPs — the backup
stays warm and its `/readyz` passes. This is exactly the active/passive keepalived
VRRP VIP design sipjsserver uses
(`/home/vince/sipjsserver/deploy/helm/sip-front-proxy/`), and it ports as
**manifests + a keepalived sidecar only**.

## 1. Front-proxy HA — keepalived VRRP VIP (active/passive)

Port the sipjsserver Helm chart to raw manifests in
[30-proxy.yaml](deploy/k8s/manifests/30-proxy.yaml). VIP = `172.20.255.250`
(static, high in the kind `172.20.0.0/16` node subnet — same value the TS project
used; overridable via `PROXY_VIP`). Two `tier=edge` nodes already exist in
[cluster.yaml](deploy/k8s/cluster.yaml).

Changes to the proxy Deployment:
- `replicas: 2`; `strategy: RollingUpdate {maxSurge:0, maxUnavailable:1}`; hard
  `podAntiAffinity` on `kubernetes.io/hostname` (one proxy per edge node).
- `hostNetwork: true`, `dnsPolicy: ClusterFirstWithHostNet`,
  `shareProcessNamespace: true` (keepalived needs node ARP).
- `PROXY_LISTEN=${PROXY_VIP}:5060`, `PROXY_ADVERTISE=${PROXY_VIP}:5060` (replaces
  the pod-IP advertise — the **stable** address that survives failover + restart).
- **initContainer `vip-loopback`** (privileged, `NET_ADMIN`): `ip addr add
  ${PROXY_VIP}/32 dev lo` + strict-ARP sysctls (`arp_ignore=1`, `arp_announce=2`
  on all+lo) so the proxy's `bind(VIP:5060)` succeeds on both pods and only the
  master answers ARP. Ported verbatim from the TS init container.
- **sidecar `keepalived`** (`osixia/keepalived:2.0.20`, caps `NET_ADMIN`,
  `NET_BROADCAST`, `NET_RAW`; `preStop: pkill keepalived; sleep 1`) reading a new
  ConfigMap `sip-front-proxy-keepalived` (`state BACKUP`, `nopreempt`,
  `virtual_router_id 51`, `advert_int 0.5`, `virtual_ipaddress ${PROXY_VIP}`,
  multicast VRRP — the TS kind cluster uses multicast on the same Docker bridge;
  `notify_*` re-add VIP on lo). Add this ConfigMap to 30-proxy.yaml (or a sibling
  `28-proxy-keepalived.yaml`).
- Keep the metrics readiness/liveness probes (kubelet probes node-IP:9090 under
  hostNetwork — unaffected). Keep the Service for metrics; SIP no longer flows
  through it.

The HMAC stickiness cookie + worker P2P replication (ADR-0011) already make
in-dialog routing and worker failover correct from either proxy; active/passive
(only the master is live) keeps the per-pod `cancel_lru` coherent, so CANCEL/ACK
correlation is unaffected.

## 2. Limiter scenario — `-key xapi` fix

- [uac-endurance-limiter-cap20.xml](deploy/k8s/sipp/scenarios/uac-endurance-limiter-cap20.xml):
  change the header to `X-Api-Call: [xapi]` (single-pass `-key` substitution is
  not re-parsed for brackets — the workaround documented in
  [uac-endurance-limiter.xml](deploy/k8s/sipp/scenarios/uac-endurance-limiter.xml)).
- [40-sipp-uac-job.yaml](deploy/k8s/manifests/40-sipp-uac-job.yaml): append three
  **static** args to every stream —
  `-key`, `xapi`, `'{"action":"route","call_limiter":[{"id":"endurance-limiter","limit":${LIMITER_CAP}}]}'`.
  An unreferenced `-key` is a no-op for the long/short/abuse/peak scenarios (they
  don't contain `[xapi]`), so only the limiter stream sends the header. The JSON is
  a single-quoted YAML scalar (brackets/quotes are safe; only `${LIMITER_CAP}` is
  envsubst-expanded, default 20).

## 3. SIPp targets the VIP

[40-sipp-uac-job.yaml](deploy/k8s/manifests/40-sipp-uac-job.yaml): change the first
sipp arg from the Service DNS to `${PROXY_TARGET}:5060`. Export
`PROXY_TARGET="${PROXY_VIP:-172.20.255.250}"` near the top of
[run.sh](deploy/k8s/run.sh), [endurance.sh](deploy/k8s/endurance.sh), and
[chaos.sh](deploy/k8s/chaos.sh) so every `envsubst`-rendered launch site (baseline
streams, peak, orphan, abuse, failover, caps/sweep) inherits it.

## 4. Tighten `kill_proxy`

- [chaos.sh](deploy/k8s/chaos.sh) `kill_proxy`: kill **only the VIP master**, not
  both replicas. Detect it by exec-ing each proxy pod's keepalived container and
  checking `eth0` for the VIP (`ip addr show eth0 | grep ${PROXY_VIP}`); delete
  that pod (fallback: first pod). After the kill, wait for the Deployment to
  restore 2 Ready replicas.
- [endurance.sh](deploy/k8s/endurance.sh): delete the lenient `PROXY_THRESHOLD`
  branch ([:313-315](deploy/k8s/endurance.sh#L313)) so `kill_proxy` is scored at
  the normal `PASS_THRESHOLD` (90%) — failover must keep new + in-dialog calls
  flowing.

## 5. Image loading

`osixia/keepalived:2.0.20` is not present locally. Add `docker pull` +
`kind load docker-image osixia/keepalived:2.0.20 --name "$CLUSTER"` to
[run.sh](deploy/k8s/run.sh) `up` (next to the SUT/sipp loads), and load it into the
existing cluster once during implementation.

## 6. ADRs — fold proxy HA into ADR-0012 (D7); delete the out-of-scope caveat

- [ADR-0012](docs/adr/0012-ha-addressing-and-membership-reconcile.md): add
  **Decision D7 — Front-proxy HA via active/passive VRRP VIP**: 2 anti-affined
  edge replicas, keepalived VIP, stable VIP advertise (survives failover +
  restart), the already-split forwarding/probe sockets (backup stays warm), the
  cancel_lru-coherence rationale for active/passive. Edit D6's "**single proxy and
  no VIP** (HA-behind-a-VIP is explicitly out of scope for this thin runner)"
  sentence to reference D7 (the gap is now absorbed by the peer).
- [ADR-0009](docs/adr/0009-front-proxy-rust-shape.md): remove the SPOF framing /
  add a one-line "HA: see ADR-0012 D7" note; the proxy is no longer a SPOF.
- Delete the out-of-scope wording in
  [30-proxy.yaml](deploy/k8s/manifests/30-proxy.yaml#L1) header and the
  [endurance.sh](deploy/k8s/endurance.sh#L313) / [chaos.sh](deploy/k8s/chaos.sh)
  comments; update `(single-replica)` phrasings.

## Files touched (summary)

- `deploy/k8s/manifests/30-proxy.yaml` — 2 replicas, hostNetwork, anti-affinity,
  VIP init container + keepalived sidecar + ConfigMap, VIP listen/advertise
- `deploy/k8s/manifests/40-sipp-uac-job.yaml` — `-key xapi` args, `${PROXY_TARGET}`
- `deploy/k8s/sipp/scenarios/uac-endurance-limiter-cap20.xml` — `X-Api-Call: [xapi]`
- `deploy/k8s/{run.sh,endurance.sh,chaos.sh}` — `PROXY_TARGET`/`PROXY_VIP` export,
  keepalived image load, `kill_proxy` master-only + threshold removal
- `docs/adr/0012-...md` (new D7), `docs/adr/0009-...md` (drop SPOF framing)

No Rust changes. No new crate deps.

## Verification

1. **HA smoke first (don't burn 2h blind):** rebuild/reload the SUT image + load
   keepalived, `REPL_ENABLE=1 ./run.sh deploy`, confirm: both proxies Ready, the
   VIP is on exactly one edge node (`kubectl exec … ip addr | grep 172.20.255.250`),
   a SIPp call to the VIP completes, then `./chaos.sh proxykill` (master) and
   confirm calls keep flowing (VIP moves, <2s gap) and the limiter stream pins at
   ~20 (`sipp_current_calls{role="limiter"}`, and `limiter_rejected_total` rate
   non-zero — proves the `-key` fix). Iterate here until green.
2. **Clean 2-hour run:** `./endurance.sh run` (full `DURATION=7200`,
   `CHAOS_INTERVAL=900`, `SHORT_CPS=100`). Success criteria:
   - `kill_proxy` events PASS at the normal 90% bar (no lenient threshold), and the
     retransmit/dead-call tail after a proxy kill is gone (VIP stable);
   - `limiter_kill` / `limiter_netcut` reconverge to ~20 within `LIMITER_GRACE`;
   - `events.jsonl` shows all event types `result:"pass"`.
3. Grafana: proxy dashboard shows 2 pods, traffic continues across `kill_proxy`
   windows; call-limiter dashboard shows the chain stressed on every call.
