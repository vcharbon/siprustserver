# 0012 — HA addressing & membership reconcile: self-healing topology, DNS-by-pod-name, one watcher for both consumers

**Status:** accepted (2026-06-02)

**Source:** this codebase (no sipjsserver counterpart — the source mediated
membership through Redis/`PeerEnumerator`, which the Rust port dropped in
ADR-0011 X1). Triggered by a real 2h endurance+chaos incident
(`deploy/k8s/results/endurance-20260602-085331/`) and its handoff
(`/tmp/handoff-repl-reconnect-bug.md`).

## Context

ADR-0011 X7 promised "the k8s watcher is written **once** and consumed by both
the proxy and the b2bua replication engine — neither re-implements discovery."
At the time only the **repl** consumer used it (`topology::K8sMembership`, an
EndpointSlice informer → `MemberDelta` broadcast); the **proxy** was left on a
`StaticWorkerRegistry` whose worker IPs `deploy/k8s/run.sh` baked from live pod
IPs at deploy time, and `chaos.sh` re-ran `deploy` to refresh after every kill.
X7 was therefore only half-realized.

A chaos run then exposed a second, sharper defect. The repl
`ReplicationSupervisor` reconcile loop did `Err(_) => return` on **any**
`broadcast::error::RecvError` — both `Lagged` and `Closed`. During kill-churn the
membership delta channel lagged once; the loop exited permanently; the node went
**deaf to all further membership deltas**; its puller kept re-dialing a peer's
**dead pod IP** forever (its own connection-loss detection + backoff were
correct — nothing ever told it the address moved). The backup partition for that
peer went stale and the next kill lost failover (chaos `kill_worker` #5 resolved
87% < 90%).

Two distinct concerns surfaced, and this ADR pins both plus the addressing
question they raised:

1. **Liveness of the membership-consuming loop** (the actual bug).
2. **Addressing**: should the repl puller and the proxy reach workers by **Pod
   IP** (re-resolved on membership change) or by **stable per-pod DNS name**
   (re-resolved per connect)? And must the two flows be **consistent**?

The two flows differ in transport: repl is a **long-lived TCP** stream (a
connection break is a crisp re-resolve trigger); proxy→worker is **connectionless
UDP** SIP (liveness is inferred from an OPTIONS health probe, not a socket
error). That difference is load-bearing for the decisions below.

## Decision D1 — the membership-consuming loop must self-heal; `Lagged` is recoverable, only `Closed` is terminal

`Err(Lagged)` means the consumer fell behind a bursty producer and the channel
**dropped intermediate deltas** — it does **not** mean membership is gone. The
correct response is to **re-read `membership.snapshot()` and full-reconcile**
(spawn/redirect pullers for new or moved ordinals, park departed ones), then keep
looping. Only `Err(Closed)` (the source dropped) is terminal. `return`-on-any-
error was the bug.

The snapshot is authoritative and the underlying `K8sMembership` reflector is a
`default_backoff()` self-healing watch, so reconciling **from the snapshot** (not
from the lost deltas) is always sufficient to converge — this is the same
snapshot-driven `reconcile_to_desired` pattern the informer already uses
internally. `spawn_puller` is idempotent (absorbs + cancels any running puller,
reseeds from the retained watermark), so re-reconciling is safe.

## Decision D2 — a periodic snapshot reconcile is a belt-and-suspenders safety net

Reacting only to live deltas means **any** single missed delta (a lag we didn't
catch, a watch gap, a future bug) is silently uncorrected. The supervisor
therefore also runs a **periodic (default 5 s) snapshot-driven reconcile**: it
re-resolves the desired set and only acts on **drift** — spawn a peer that is
desired but not running or whose resolved address changed; park a peer that is
running but no longer desired. An unchanged set is a no-op (no needless puller
respawns, no dropped streams). This makes a missed delta **non-fatal** rather
than catastrophic, and is cheap (a handful of peers, every few seconds).

## Decision D3 — repl addressing: stable per-pod DNS name, resolved fresh per connect attempt (defense-in-depth, hybrid with the informer)

The puller resolves its peer's **stable per-pod DNS name**
(`{ordinal}.{headless-svc}.{ns}.svc.cluster.local`, derived from the ordinal =
StatefulSet pod name = membership identity) **fresh on every connect attempt**,
instead of caching a `SocketAddr` at construction. Its existing
`recv→None → Backoff → Connecting` loop then re-resolves the name → the restarted
peer's new IP automatically, **with no membership delta required**. This would
have prevented the incident on its own.

This is a **robustness multiplier, not a correctness substitute**, and is
explicitly **hybrid**:

- The **EndpointSlice informer stays** the source of the membership *set* +
  readiness (D1/D2). DNS answers "address of a name I already know"; it cannot
  enumerate peers, observe scale-down, or gate readiness of the set. DNS can
  never replace the informer.
- `publishNotReadyAddresses: false` ⇒ NXDOMAIN-while-not-ready **preserves** the
  "don't replicate to an un-rehydrated peer" gate the EndpointSlice ready flag
  gave; backoff-retry absorbs it. No deadlock: re-hydration is directional (the
  new node pulls from the surviving node, whose DNS resolves).

Resolution is **async** (`tokio::net::lookup_host`) so it never blocks the
runtime, and a **bare IP fast-path** (`parse::<IpAddr>()`) keeps the sim test
seam (literal `SocketAddr`s) and the legacy static-`B2BUA_PEERS` path working
unchanged. The per-connect resolver is the seam; the sim resolver returns its
fixed mapping, so tiers 1–2 (ADR-0011 X10) are untouched.

## Decision D4 — the proxy joins the same informer (ADR-0011 X7, finally realized); it still reaches workers by direct Pod IP

The proxy replaces its baked-IP `StaticWorkerRegistry` with a
`K8sWorkerRegistry` that wraps the **same** `topology::K8sMembership` informer
the repl path runs, mapping each ready `Peer{ordinal, host}` →
`WorkerEntry{id = ordinal, address = host:sip_port}`. The proxy then **still
sends OPTIONS and forwards SIP straight to the worker's Pod IP** — the informer's
only job is to keep that IP list correct automatically on reboot/scale. No DNS in
the proxy data path.

Why informer (not DNS) for the proxy, despite D3 choosing DNS for repl: UDP has
**no connection event** to trigger re-resolution, so DNS would need either a
blocking per-forward lookup (a hot-path footgun) or a background refresh task
plus a **cluster-wide** CoreDNS TTL reduction. The informer is **event-driven
(≈instant, no NXDOMAIN negative-cache lag), adds no DNS QPS, needs no cluster-
wide change**, and reuses an already-running watch. It is strictly better here.

Two health signals stay **separate and must not fight**:

- **Membership** (set + Pod IP + k8s-readiness) ← informer.
- **Health/load** (`Alive`/`NotReady`/`Draining`/`Dead` + load bands) ← the
  unchanged OPTIONS `HealthProbe`.

The informer's reconcile is therefore **health-preserving**: it only `Added` /
`Removed` / `AddressChanged`; it never resets an existing worker's probe-written
health. A new worker enters `Unknown` (not routable for new dialogs) until its
first OPTIONS flips it `Alive`; the LB fresh-pod guard (20 s) and drain grace
(5 s) are unchanged.

## Decision D5 — the consistency we enforce is *membership source + identity*, not *address representation*

We deliberately **do not** force identical address handling on both flows.
Consistency is mandatory where it buys correctness and operability:

- **Identity** = StatefulSet pod name = `B2BUA_ORDINAL` = EndpointSlice
  `targetRef.name` = proxy cookie `w_pri`/`w_bak` id = callRef primary. (Already
  true; this ADR does not relax it.)
- **Membership source** = the one EndpointSlice informer, consumed by both.

Address **representation** may legitimately differ per transport (DNS-per-connect
for TCP repl; informer-fed Pod IP for UDP proxy) because the *re-resolution
trigger* differs. Forcing them identical buys little and complicates the UDP
path.

### Failure modes that work in only one scheme (the consistency rationale)

- **Only DNS (D3) gives:** recovery with **no membership delta at all** —
  survives a deaf/lagged consumer or a watch gap. The incident's silver bullet;
  layered on top of D1/D2.
- **Only the informer + ready-flag gives:** event-driven readiness with **no
  negative-cache lag**; **no hard dependency on CoreDNS** in the recovery path
  (DNS is itself a common incident cause); works in the **paused-clock sim
  tests** (DNS isn't modeled there); and **set/scale enumeration** DNS
  structurally cannot provide.

This is why the design is **layered**: D1/D2 (informer + self-healing reconcile)
is the correctness floor; D3 (DNS-per-connect) is additive robustness for repl;
D4 puts the proxy on the same floor.

## Decision D6 — the proxy gates k8s readiness on ≥1 routable worker, and exposes an empty-pool gauge

D4's "informer-fed pool, `Unknown` until first OPTIONS" has a sharp edge the
old baked-IP registry did not: the pool **starts empty and fills asynchronously**
(`K8sMembership::spawn` returns before the first watch event), and **stays empty**
if the watch is RBAC-forbidden, the worker Service name is wrong, or no endpoint
is Ready. The old `StaticWorkerRegistry` started every worker `Alive`, so the
proxy could route the instant the process bound its socket. With a **single proxy
and no VIP** (HA-behind-a-VIP is explicitly out of scope for this thin runner),
that gap is not absorbed by a peer — every INVITE in the window is silently
black-holed, and a misconfigured watch black-holes **forever**.

So the proxy adopts the worker's contract: a `GET /readyz` that is `200` only
when the registry holds **≥1 `Alive`** worker, wired to the k8s `readinessProbe`
(was a bare `tcpSocket`). The Service then withholds traffic during a (re)start
until the pool is genuinely routable, and a forbidden/mis-named watch surfaces as
a **`kubectl rollout status` timeout** instead of a silent outage. A
`sip_proxy_worker_pool_empty` gauge (`1` iff zero `Alive` workers), published by a
runner-side sampler that also finally populates the `sip_worker_health` gauges,
makes the condition alertable in Prometheus. Liveness (`/healthz`, the process is
up) stays separate from readiness (`/readyz`, fit to serve) — mirroring the
worker.

## Consequences

- **Deploy simplification.** The `run.sh` block that `kubectl get pods … podIP`
  → bakes `PROXY_WORKERS` is **deleted**; the `chaos.sh` re-`deploy` step whose
  only purpose was refreshing stale worker IPs after a kill is **deleted**. The
  proxy gains an RBAC manifest (ServiceAccount/Role/RoleBinding for the
  EndpointSlice watch, mirroring `15-worker-rbac.yaml`). The pod-name-as-id
  invariant (the big run.sh warning) is unchanged and still required.
- **New dependency for the proxy:** an in-cluster kube client + EndpointSlice
  watch RBAC. Falls back (liveness over completeness, ADR-0011 X5) to the static
  `PROXY_WORKERS` list when no kube client is available (dev/local, and the sim
  test tiers), exactly as the b2bua runner falls back to `B2BUA_PEERS`.
- **CoreDNS enters the repl reconnect path** (D3) — bounded by backoff; negative
  caching can slow a just-ready peer's first reconnect by up to the SOA negative
  TTL, which is acceptable because D1/D2 already converge via the informer.
- **Regression test** added at the membership/supervisor layer: a deliberately-
  lagged broadcast channel (filled past its 256 capacity) followed by an
  address change must still redirect the puller to the new address — proving D1.

## Deferred (with justification)

- **DNS-with-low-TTL for the proxy** — rejected in favour of D4's informer; would
  only be revisited if giving the proxy watch RBAC proves undesirable in some
  target cluster.
- **Lowering cluster-wide CoreDNS TTL** — not needed; D4 is event-driven and D3's
  staleness is backoff-bounded.
- **Per-connect resolution pushed below the `ReplicationNetwork` seam** (a
  `connect_host` API) — D3 resolves *above* the seam (in the puller) to keep the
  transport seam `SocketAddr`-typed and the sim transport untouched.

## References

- ADR-0011 (HA replication peer-to-peer: X1 drop-Redis, X5 liveness-over-
  completeness, X7 one-watcher-both-consumers, X9 incarnation gen, X10 sim
  transport / test tiers — the decisions this ADR extends).
- ADR-0009 (proxy HRW + stickiness cookie + fresh-pod guard / drain grace),
  ADR-0002 (crate-per-layer / acyclicity: `topology` is a shared leaf consumed by
  both proxy and b2bua).
- `crates/b2bua/src/repl/{supervisor,puller}.rs`, `crates/topology/src/{lib,k8s}.rs`,
  `crates/sip-proxy/src/registry/`, `crates/{b2bua-runner,sip-proxy-runner}/src/main.rs`,
  `deploy/k8s/{run.sh,chaos.sh,manifests/}`.
- Incident handoff: `/tmp/handoff-repl-reconnect-bug.md`;
  run `deploy/k8s/results/endurance-20260602-085331/`.
