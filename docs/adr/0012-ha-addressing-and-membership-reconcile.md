# 0012: HA addressing and membership reconcile. Self-healing topology, DNS-by-pod-name, one watcher for both consumers

**Status:** accepted. Amended by ADR-0050 (D4: an address change resets health to `Unknown`
and re-arms no guard; there is no fresh-pod guard or drain grace) and ADR-0009 X7 (D7, the
memo-coherence argument). D7 superseded by ADR-0056.

Incident evidence, the rationale of each choice and the superseded D7 design are in the
[appendix](appendix/0012-ha-addressing-and-membership-reconcile.md).

## Context

ADR-0011 X7 has one k8s watcher consumed by both the proxy and the b2bua replication engine.
A membership loop that exits on a lagged broadcast leaves its node deaf to deltas, its puller
re-dialing a dead pod IP forever. This ADR pins the loop's liveness and how each flow reaches
workers. Repl is a long-lived TCP stream, where a connection break is a crisp re-resolve
trigger. Proxy-to-worker is connectionless UDP SIP, whose liveness comes from an OPTIONS
probe. [Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#context).

## Decision D1. The membership-consuming loop must self-heal; `Lagged` is recoverable, only `Closed` is terminal

`Err(Lagged)` means the consumer fell behind and the channel dropped intermediate deltas. It
does not mean membership is gone. The loop re-reads `membership.snapshot()`, full-reconciles
(spawn or redirect pullers for new or moved ordinals, park departed ones) and keeps looping.
Only `Err(Closed)` (the source dropped) is terminal.

The snapshot is authoritative and the `K8sMembership` reflector is a `default_backoff()`
self-healing watch, so reconciling from the snapshot always converges. `spawn_puller` is
idempotent (it absorbs and cancels any running puller, and reseeds from the retained
watermark), so re-reconciling is safe.

## Decision D2. A periodic snapshot reconcile is a belt-and-suspenders safety net

The supervisor also runs a periodic snapshot-driven reconcile (default 5 s). It acts only on
drift: it spawns a peer that is desired but not running or whose resolved address changed,
and parks a peer that is running but not desired. An unchanged set is a no-op. A missed delta
is then non-fatal.

## Decision D3. Repl addressing: stable per-pod DNS name, resolved fresh per connect attempt

The puller resolves its peer's stable per-pod DNS name
(`{ordinal}.{headless-svc}.{ns}.svc.cluster.local`) fresh on every connect attempt, async
(`tokio::net::lookup_host`), with a bare-IP fast path for the sim seam and a static
`B2BUA_PEERS`. A restarted peer's new IP is reached with no membership delta. DNS adds
robustness and replaces nothing: the EndpointSlice informer stays the source of the
membership set and its readiness (D1, D2). `publishNotReadyAddresses: false` keeps an
un-rehydrated peer unreachable, and backoff absorbs the NXDOMAIN.
[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#decision-d3-repl-addressing-stable-per-pod-dns-name-resolved-fresh-per-connect-attempt).

## Decision D4. The proxy joins the same informer (ADR-0011 X7); it still reaches workers by direct Pod IP

> Amended by ADR-0031 D1: a *terminating* member stays a replication peer and stops being a
> routing target, so membership deletion does not mean "gone" for both consumers at once.

The proxy's `ComposedWorkerRegistry` (membership ⊕ health projection) wraps the same
`topology::K8sMembership` informer, mapping each ready `Peer{ordinal, host}` to
`WorkerEntry{id = ordinal, address = host:sip_port}`. The proxy sends OPTIONS and forwards SIP
straight to the Pod IP, with no DNS in its data path. UDP has no connection event to trigger a
re-resolve.

Membership (set, Pod IP, k8s readiness) comes from the informer. Health and load come from the
OPTIONS `HealthProbe`. A worker whose ordinal and Pod IP are both unchanged keeps its
probe-written health. A new worker enters `Unknown` until its first OPTIONS reads `Alive`.
[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#decision-d4-the-proxy-joins-the-same-informer-adr-0011-x7-it-still-reaches-workers-by-direct-pod-ip).

### D4 follow-up. A restarted worker is a fresh incarnation, not an unchanged one

The discriminator is an address change, not ordinal identity. On `AddressChanged`, or a
`Removed` then `Added` across reconciles, the registry resets health to `Unknown` and the
OPTIONS probe re-classifies the worker at its new address. Preserved stale health would
black-hole the recovered worker.
[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#d4-follow-up-a-restarted-worker-is-a-fresh-incarnation-not-an-unchanged-one).

## Decision D5. The consistency we enforce is membership source and identity, not address representation

- **Identity** = StatefulSet pod name = `B2BUA_ORDINAL` = EndpointSlice `targetRef.name` =
  proxy cookie `w_pri`/`w_bak` id = callRef primary.
- **Membership source** = the one EndpointSlice informer, consumed by both.

Address representation differs per transport (DNS per connect for TCP repl, informer-fed Pod
IP for UDP proxy) because the re-resolution trigger differs. D1/D2 are the correctness floor,
D3 is additive robustness for repl, and D4 puts the proxy on the same floor.

### Failure modes that work in only one scheme (the consistency rationale)

Only DNS recovers with no membership delta. Only the informer gives event-driven readiness,
no CoreDNS dependency, sim-test support and set enumeration.
[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#failure-modes-that-work-in-only-one-scheme-the-consistency-rationale).

## Decision D6. The proxy gates k8s readiness on ≥1 routable worker, and exposes an empty-pool gauge

The informer-fed pool starts empty and fills asynchronously, and stays empty on a forbidden
or mis-named watch. The proxy's `GET /readyz` is `200` only when the registry holds at least
one `Alive` worker, and it backs the k8s `readinessProbe`. A misconfigured watch then surfaces
as a `kubectl rollout status` timeout. `sip_proxy_worker_pool_empty` is `1` iff no worker is
`Alive`. Liveness (`/healthz`) stays separate from readiness (`/readyz`), as on the worker.
[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#decision-d6-the-proxy-gates-k8s-readiness-on-1-routable-worker-and-exposes-an-empty-pool-gauge).

## Decision D7. Front-proxy HA via active/passive keepalived VRRP VIP

Superseded by [ADR-0056](0056-the-front-proxys-addresses-are-announced-by-bgp.md): the VIPs
are announced by BGP from every node whose proxy is Ready.
[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#decision-d7-front-proxy-ha-via-activepassive-keepalived-vrrp-vip).

## Consequences

- Deploy bakes no worker IPs. The proxy holds EndpointSlice watch RBAC
  (`templates/proxy-rbac.yaml`), and a worker's StatefulSet pod name stays its ordinal.
- A fixed `PROXY_WORKERS` list replaces discovery only when set. A kube client that fails to
  build is built again in the background, the proxy NotReady meanwhile (ADR-0046).
- CoreDNS is in the repl reconnect path (D3), bounded by backoff. Negative caching can delay
  a just-ready peer's first reconnect by the SOA negative TTL.
- A regression test lags the broadcast past its 256 capacity, then changes an address, and
  the puller still redirects.

[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#consequences).

## Deferred (with justification)

- DNS with a low TTL for the proxy: rejected for D4's informer.
- Lowering cluster-wide CoreDNS TTL: not needed.
- Per-connect resolution below the `ReplicationNetwork` seam: rejected, the seam stays
  `SocketAddr`-typed.

[Detail](appendix/0012-ha-addressing-and-membership-reconcile.md#deferred-with-justification).

## References

ADR-0011 (X1, X5, X7, X9, X10), ADR-0009, ADR-0002. Code paths:
[appendix](appendix/0012-ha-addressing-and-membership-reconcile.md#references).
