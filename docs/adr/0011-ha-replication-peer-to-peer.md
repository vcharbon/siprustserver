# 0011: HA replication, peer-to-peer pull, no Redis

**Status:** accepted; superseded in part by
[ADR-0014](0014-reactive-only-takeover-version-vector.md) and its terminal-handling rules.
ADR-0014 owns takeover (reactive-only), acting-backup self-release (terminal discharge
deferred to the primary), `(p,b)` reconciliation and the wire (five frames over two
single-flow sockets). The sections below hold what ADR-0014 did not replace. X5 amended by
ADR-0055 §6 (fencing by worker identity compared for equality).

The source mapping, the rationale of each choice and the rejected alternatives are in the
[appendix](appendix/0011-ha-replication-peer-to-peer.md).

## Context

The source runs a pull-based replication log with watermarks over Redis. The Rust port drops
Redis. The HA seam already exists (ADR-0010 X3): `CallStore` carries the role, primary and
generation, `callRef = {primary}|{callId}|{fromTag}` encodes ownership, and the proxy emits
the `w_pri`/`w_bak` HRW stickiness cookie (ADR-0009). This ADR records the engine's shape.
The wire spec, reconnect FSM and slice plan are in
[the migration plan](../plan/on-proper-migration-of-lazy-pancake.md), the exchange diagrams
in [ha-replication.html](../ha-replication.html).
[Detail](appendix/0011-ha-replication-peer-to-peer.md#context).

## Decision X1. Drop Redis; replication is direct peer-to-peer pull

Each b2bua is both a replication server (its change stream and a bootstrap scan) and a
client that pulls from every peer. Membership is full mesh (the HRW second-best backup is any
peer per call), so a worker is primary for its own calls and backup for its peers' calls,
with two forward channels per ordered pair.

## Decision X2. Transport is its own message-granular reliable seam

Replication gets its own seam, `repl-net::ReplicationNetwork`, parallel to
`sip-net::SignalingNetwork`. It moves whole encoded frames, so the codec runs on every test
path and the recording layer decodes each message (ADR-0006). The real impl is tokio TCP
with a 4-byte length prefix; the sim impl is in-process ordered delivery. Real TCP readiness
ignores `tokio::time::pause`, so the fake-clock tiers need the sim transport.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x2-transport-is-its-own-message-granular-reliable-seam).

## Decision X3. Forward changelog: per-peer compacted ring of refs, body from store at send

The per-peer change stream is an in-memory, ordered changelog of callRef references
(`{counter, callRef, op}`), compacted to the latest per callRef. The body is read from the
store at send time. A re-update moves the ref's counter, and a delete leaves a TTL-reaped
tombstone. A dead peer's cursor cleans itself after a TTL, and the peer re-bootstraps on
return. The log equals the live set, so a cold puller from `(gen, 0)` gets everything and a
caught-up one gets only deltas.

## Decision X4. Re-hydration: scan, conservative watermark and tail (per flow)

ADR-0014 §Stream topology runs re-hydration and backup re-subscription on two single-flow
sockets. Each flow's bootstrap scans the partition, capturing `W = changelog head` at scan
start, then tails from `W`. A mutation during or after the scan (`counter > W`) is delivered
again, idempotent under `(p,b)`, so snapshot consistency does not matter.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x4-re-hydration-scan-conservative-watermark-and-tail-per-flow).

## Decision X5. Fail-back: readiness-gated reclaim; hard timer is a readiness bound only

A rebooting primary reclaims its calls by re-materialising its `pri:{self}` partition.
Recovery is purely causal (ADR-0014), with no time-based ownership flip. The hard timer only
bounds readiness: a node boots and serves even when peers are unreachable (liveness over
completeness). ADR-0014 owns the backup half.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x5-fail-back-readiness-gated-reclaim-hard-timer-is-a-readiness-bound-only).

## Decision X6. Readiness/OPTIONS gate: re-hydrated and backup-current

A `NotReady → Ready → Draining` state machine self-reports through OPTIONS: `200` alive,
`503` with not-ready, `503` with draining and `Retry-After: 0`. A node is Ready once
re-hydration from every reachable peer has completed (hard-timer bounded) and its forward
pulls are current. A pull is current once the head `Noop` arrives, sticky across reconnects.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x6-readinessoptions-gate-re-hydrated-and-backup-current).

## Decision X7. Topology: shared membership crate, promoted and factored

Peer discovery sits behind `topology::Membership` (`Peer{ordinal, host}`, `snapshot()`, a
`changes()` watch; `Static | Simulated | K8s` impls). The k8s watcher is written once, as
`K8sMembership`, and both the proxy and the b2bua consume it. Membership is port-agnostic.
Which calls a node backs up is the content of each peer's changelog.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x7-topology-shared-membership-crate-promoted-and-factored).

## Decision X8. Server never blocks the call path; strict lock/ownership discipline

The call-mutation path appends to the changelog without blocking, touching no socket. TCP
flow control is the backpressure, with no app-level eviction buffer. Neither the append nor
the drain path holds the call-DB or changelog lock across I/O or an await. The store holds
each encoded body as an `Arc<[u8]>`, which the drain clones under a brief lock.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x8-server-never-blocks-the-call-path-strict-lockownership-discipline).

## Decision X9. Wire protocol: positional-msgpack frames

Each message is a positional-msgpack array (ADR-0008), tag-discriminated by element 0. The
frame set is ADR-0014 §12's. `gen` is the worker incarnation (the high word of the
watermark), and `call_gen` is the call's `p`.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x9-wire-protocol-positional-msgpack-frames).

## Decision X10. Three test tiers; the sim transport is what makes 1 and 2 possible

Tier 1 is a pure HA-framework harness under the fake clock. Tier 2 is fully simulated
failover over the SIP and replication sim fabrics. Tier 3 is real chaos on kind. Every tier
is recording-first (ADR-0006).
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x10-three-test-tiers-the-sim-transport-is-what-makes-1-and-2-possible).

## Decision X11. Fail-back, backup half (superseded by ADR-0014)

Two nodes serving one dialog tear a healthy call down through CSeq-conflicting keepalives,
so a backup holds a takeover copy only while serving. A reclaiming primary re-serves its
calls in a bulk sweep on bootstrap-complete.
[Detail](appendix/0011-ha-replication-peer-to-peer.md#decision-x11-fail-back-backup-half-superseded-by-adr-0014).

## Deferred (with justification)

- N-way backup: one backup now, and the per-peer changelog keeps N backups additive.
- The real source of the incarnation gen: an injectable seam.
- Sticky failover and hard fencing: rejected (X5).

[Detail](appendix/0011-ha-replication-peer-to-peer.md#deferred-with-justification).

## References

ADR-0009, ADR-0010 X3, ADR-0008, ADR-0006, ADR-0002. Plan and code paths:
[appendix](appendix/0011-ha-replication-peer-to-peer.md#references).
