# 0011 — HA replication: peer-to-peer pull, no Redis

**Status:** accepted (2026-05-31); **superseded in part by
[ADR-0014](0014-reactive-only-takeover-version-vector.md)** (2026-06-05) and its
Model-Y terminal-handling rules. The current takeover, stream-topology,
wire-protocol, and terminal-discharge design lives in ADR-0014: takeover is
**reactive-only**, the acting-backup **self-releases** (deferring any terminal
discharge to the primary), reconciliation is the `(p,b)` version vector, and the
wire is four frames over two single-flow sockets. The sections below are trimmed
to what ADR-0014 did not replace.

**Source:** sipjsserver @ `fffc4ac6`,
`src/replication/{ReplLogServer,PullerFiber,ReplicationProtocol,ReplicationSupervisor,PeerScanBootstrap,ChannelStream,ChannelIndex,EpochCounter,genCounter,EchoApply,ReadinessController,PullerHttpTransport}.ts`,
`src/cache/{PartitionedRelayStorage,PeerEnumerator,PeerCachePort,PeerCacheClient,StickinessCookie,WorkerReadiness,BufferedTerminateWriter}.ts`,
`src/sip-front-proxy/health/{HealthProbe,WorkerRegistryControl}.ts`.

## Context

Call-context high availability in the source is a **pull-based replication log
with watermarks**, *mediated by Redis*: a primary dual-writes each call into
Redis `pri:{owner}` / `bak:{owner}` partitions and appends to per-peer
`propagate:{peer}` ZSETs (callRef-keyed, counter-scored — so re-writing a call
*moves* its single entry: the set is already implicitly compacted); a backup
long-polls `/replog` and applies anything newer than its `(gen, counter)`
watermark; a rebooting primary re-hydrates via `/bootstrap`. Redis was only the
*substrate* — the mechanism is the log + watermark + pull.

The Rust port **drops Redis** (`docs/MIGRATION_STRATEGY.md`: "Redis sidecar
dropped; in-memory buffer + tokio cleanup instead"). The HA seam is already
carved (ADR-0010 X3): `CallStore` carries `role`/`primary`/`peer`/`direction`/
`call_gen`/`ttl`, `callRef = {primary}|{callId}|{fromTag}` encodes ownership,
the proxy emits the `w_pri`/`w_bak` HRW stickiness cookie (ADR-0009) and has a
pre-wired `WorkerHealth::NotReady`. This ADR records the shape of the missing
engine: the transport, the in-process change log (replacing the Redis ZSETs),
the puller/supervisor, bootstrap, the k8s topology provider, and the readiness
state machine. The decisions below were resolved in a design grilling and pin
the choices a future reader would otherwise wonder about. The detailed wire
spec, reconnect FSM, and slice plan live in
[`docs/plan/on-proper-migration-of-lazy-pancake.md`](../plan/on-proper-migration-of-lazy-pancake.md);
the visual exchange diagrams in [`docs/ha-replication.html`](../ha-replication.html).

## Decision X1 — drop Redis; replication is direct peer-to-peer pull

Without the Redis substrate, each b2bua becomes **both a replication server**
(exposes its change stream + a bootstrap scan) **and a client** (pulls from
every peer). Membership is full-mesh (HRW 2nd-best backup is per-call any peer),
so a worker is simultaneously *primary* for its own calls and *backup* for
peers' calls — there are two forward channels per ordered pair. This is the same
`PullerFiber` ↔ `ReplLogServer` protocol the source ran, with the source of
frames moved from "Redis ZSET walk" to an "in-process changelog walk" and the
network from "HTTP-to-Redis-backed-server" to "peer-to-peer connection."

## Decision X2 — transport is its own message-granular reliable seam

Replication is a reliable, ordered, framed stream — unlike SIP's UDP datagrams —
so it gets **its own seam** (`repl-net::ReplicationNetwork`), parallel to
`sip-net::SignalingNetwork`, *not* a reliability layer bolted onto the UDP sim.
The seam moves **whole messages** (one encoded frame = one byte array), so the
frame **codec plays through every test path** and the recording layer can
decode+display each replication message (ADR-0006). Real impl = tokio TCP +
4-byte length prefix; sim impl = in-process ordered delivery.
**Surprising-without-context rationale:** real `TcpStream` I/O readiness does
**not** obey `tokio::time::pause`, so the fake-clock tests (goals 1–2) *cannot*
use real TCP — the sim transport is mandatory, not a convenience. Real TCP is
goal-3 (kind) only.

## Decision X3 — forward changelog: per-peer compacted ring of refs, body from store at send

The per-peer change stream is an in-memory **ordered, compacted (latest-per-
callRef) changelog of callRef *references*** (`{counter, callRef, op}`) — the
faithful in-process equivalent of the source's compacted `propagate:{peer}`
ZSET / `ChannelStream`+`ChannelIndex`. The **body is read live from the store at
send time** (the log stays tiny; a re-update just moves the ref's counter;
deletes leave a TTL-reaped tombstone). A dead peer's cursor **auto-cleans** after
a TTL; it re-bootstraps on return. Because it is compacted-by-callRef the log
always equals the live set, so a cold puller from `(gen, 0)` gets everything and
a caught-up one gets only deltas.

## Decision X4 — re-hydration: scan + conservative watermark + tail (per flow)

> Topology updated by [ADR-0014](0014-reactive-only-takeover-version-vector.md)
> §Stream topology — re-hydration and backup-re-subscription run on **two separate
> single-flow sockets** with distinct watermarks (one changelog, two
> partition-filtered cursors); bootstrap is implicit when `since==(0,0)`. The
> per-flow scan + conservative-watermark + tail mechanism below is **unchanged**.

On boot a primary re-hydrates its owned calls by scanning a backup's
`bak:{primary}` partition (`PeerScanBootstrap`). The server copies just the
callRef **keys** under a brief lock, releases, then streams bodies in ~128
batches reading each current body under a short lock — so a slow/crashing puller
never holds the call-map lock. It captures `W = changelog head` **at scan
start**; the client seeds its watermark to `W` and **keeps tailing**, so any
mutation the acting-backup makes during/after the scan (`counter > W`) is
re-delivered (idempotent by the `(p,b)` version vector). Snapshot consistency is
therefore *irrelevant* — correctness is bootstrap + conservative watermark + tail,
not the snapshot.

## Decision X5 — fail-back: readiness-gated reclaim; hard timer is a readiness bound only

When a primary reboots it reclaims its calls (vs *sticky failover*, where
taken-over calls would live out their life on the backup, or *hard fencing* with
epoch leases). Recovery is **purely causal**
([ADR-0014](0014-reactive-only-takeover-version-vector.md)): the rebooting primary
re-materialises its `pri:{self}` partition into its live map (**reclaim**), and a
dialog that receives in-dialog traffic during the outage is taken over reactively
on the survivor — there is **no time-based ownership flip**. The **hard timer
survives only as a readiness/liveness bound**: best-effort re-hydration never
blocks startup, so a node can **boot and serve even when peers are unreachable**
(liveness over completeness; ADR-0014 §Stream-topology Decision 11). The
flip-instant window is covered by SIP retransmission + the proxy's
ACK/CANCEL-to-primary rule (ADR-0009 / ADR-0014 Decision 5). Rejected: sticky
failover (needs new per-call proxy ownership-flip) and hard fencing (epoch
propagation + per-txn fence checks + a 2-phase drain handshake — too much
machinery for "ultra early").

The *backup* half of fail-back — how and when the acting-backup releases the live
call it took over — is owned by ADR-0014: the backup **self-releases** on the
served transaction's terminal state and **defers** any terminal discharge to the
reclaiming primary.

## Decision X6 — readiness/OPTIONS gate: re-hydrated + backup-current

A `NotReady → Ready → Draining` state machine (`ReadinessController` /
`WorkerReadiness`) self-reports via OPTIONS — `200`=alive, `503 + not-ready`,
`503 + draining/Retry-After:0` — consumed by the proxy's existing
`WorkerHealth`. **Ready** when bootstrap re-hydration has completed for all
*reachable* peers (best-effort, hard-timer bounded) **and** the forward pulls are
"current", where the **`current` flag is set the instant the head `Noop`
arrives** (per-peer, sticky across reconnects — the source's `everCaughtUp`).
Not "strictly converged" (the reverse tail never quiets under load → the signal
would degenerate to the timer) and not "re-hydrated only" (the node would be a
weak backup right after boot). This replaces the always-200 stub (ADR-0010 X8).

## Decision X7 — topology: shared membership crate, promoted + factored

Peer discovery is abstracted behind a shared `topology::Membership` (`Peer{
ordinal, host}` + `snapshot()` + a `changes()` watch; `Static | Simulated | K8s`
impls). It is **promoted out of the proxy's existing `WorkerRegistry`** — a
well-factored `ArcSwap`+`broadcast` abstraction whose membership source is the
shared `topology::K8sMembership` EndpointSlice informer — so the k8s watcher is
written **once** (in `topology`, as `K8sMembership`) and consumed by both the
proxy (annotating SIP addr + health/draining/fresh-pod over it) and the b2bua
(deriving the replication address from `ordinal + host + config`). Membership is **port-agnostic** (a k8s pod has one
address, many ports). "Which elements do I back up" *dissolves* into the
contents of each peer's per-peer changelog (HRW 2nd-best, full mesh) — there is
no separate retrieval. Rejected: reuse the proxy registry whole (drags routing/
health/`ProxyAddr` baggage into the b2bua, couples crates against ADR-0002) and
build a second independent provider (two membership sources + two k8s watchers).
(The proxy was migrated onto this shared informer as `ComposedWorkerRegistry` in
[ADR-0012](0012-ha-addressing-and-membership-reconcile.md) D4, which owns that
realization timeline.)

## Decision X8 — server never blocks the call path; strict lock/ownership discipline

The call-mutation path appends to the changelog **non-blocking** (`try_send` /
move-ref + notify — the `BufferedTerminateWriter` pattern), touching no socket
and waiting on no subscriber: a slow or dead client must never stall call
processing. There is **no app-level eviction buffer** — TCP flow-control + the OS
socket buffer are the backpressure, and the compacted changelog is the bounded
backing (a lagging cursor simply reads latest-per-call when it catches up). The
invariant: neither the append path nor the drain path holds the call-DB or
changelog lock across any I/O/await, and both survive the call being removed
mid-send. The store holds each encoded body as **`Arc<[u8]>`** (produced once by
flush); the drain clones the `Arc` under a brief lock (refcount bump, no copy, no
re-encode, no contention with the typed routing map), drops the guard, then
writes on the owned `Arc`. Safe under concurrent rewrite by the **immutable-
shared-body invariant** (a rewrite swaps the slot to a new `Arc`; the in-flight
drain keeps its old one alive) — the same `ArcSwap` discipline the registry uses.

## Decision X9 — wire protocol: positional-msgpack frames

Each message is a positional-msgpack array (ADR-0008 ethos), tag-discriminated by
element 0. The **canonical frame set is four frames — see
[ADR-0014](0014-reactive-only-takeover-version-vector.md) §12**: `PullRequest`
(opens one flow; bootstrap is implicit when `since==(0,0)`), `Data` (one mutation,
carrying the `(p,b)` version vector — `call_gen`/`call_bgen`), `Noop` (catch-up
edge + idle keepalive), `ResetToBootstrap` (the watermark fell off the compacted
tail). The earlier `Ack` frame and the `PullMode` / `Bootstrap`-vs-`Replog`
two-request handshake are **removed**.

**Two distinct generations** (a source-side terminology trap, `EpochCounter` vs
the call body's `_topology.gen`): `gen` = incarnation (per worker-restart, high
word of the watermark), `call_gen` = the call's primary counter `p` of the `(p,b)`
version vector. A rebooted worker serves under a higher `gen`/counter-0, so
`(new_gen, 0) > (old_gen, *)` and pullers apply without a manual reset; a missed
delete during a disconnect self-evicts via the call `ttl`.

## Decision X10 — three test tiers; the sim transport is what makes 1 & 2 possible

(1) A **pure HA-framework** harness — several in-process replication-subsystem
nodes (`CallStore + changelog + sim transport + puller/supervisor + topology +
Clock(test_at)`, **always under fake clock**, no SIP), driving put/delete/crash/
reboot/partition and asserting convergence. (2) **Fully simulated failover** —
proxy + ≥2 b2buas over the SIP sim fabric *and* the replication sim fabric, fake
clock: crash → failover → reboot → reclaim. (3) **Real chaos** on kind (real TCP
+ real k8s topology). Tiers 1–2 hinge on X2's sim transport (real TCP can't run
under `tokio::time::pause`). Every tier is recording-first (ADR-0006): the
replication exchange renders as a sequence diagram beside the SIP exchange.

## Decision X11 — fail-back, backup half (superseded by ADR-0014)

X11 originally closed the *backup* half of fail-back with an eager
(membership-driven) takeover and an explicit `Deactivate{ as_of: Watermark }`
handshake. Both were **removed before they shipped** by
[ADR-0014](0014-reactive-only-takeover-version-vector.md): takeover is
reactive-only and the acting-backup **self-releases** on the served transaction's
terminal state, deferring any terminal discharge to the reclaiming primary. Two
facts from X11's analysis remain load-bearing and are recorded here:

- **Why exclusive ownership matters (the correctness bug X11 found).** If two
  nodes serve the *same* dialog, each sends in-dialog keepalive OPTIONS under its
  own CSeq counter; a strict UAS answers the lower-CSeq OPTIONS with `500`, so
  `absorb-options-200` does not fire, `KeepaliveTimeout` trips at 5 s, and a
  **healthy call is torn down**. Double-serving is a correctness bug, not just a
  leak — which is why the backup must hold a takeover copy only while actively
  serving and self-release deterministically.

- **Reclaim vs re-hydration.** A reclaiming primary **re-serves** (materialises
  each `pri:` call into the live map and re-arms timers), not merely **re-stores**.
  Reclaim is a **bulk sweep on bootstrap-complete**, not lazy-on-in-dialog: a
  long-hold quiescent call receives no inbound in-dialog request (the B2BUA *sends*
  the keepalives), so lazy would never fire for exactly the calls that would
  otherwise leak.

## Deferred (with justification)

- **k8s `Membership` impl + real TCP transport** — tier-3 only; the sim impls
  carry tiers 1–2 (X10).
- **N-way backup** — single backup (HRW 2nd-best) now; the per-peer changelog
  keeps N-backup an additive change, not a rewrite.
- **Incarnation-gen real source** (boot wall-clock vs k8s pod start epoch) —
  injectable seam now (test = seed, like `IdGen::seeded`); real source finalised
  with the k8s slice.
- **Sticky failover / hard fencing** — rejected (X5). The best-effort flip proved
  lossy under tier-3 chaos (orphaned takeover copies double-serving the same
  dialog), so fail-back was hardened in
  [ADR-0014](0014-reactive-only-takeover-version-vector.md) (reactive takeover +
  acting-backup self-release on transaction completion) — still short of full hard
  fencing (no per-txn fence, no epoch leases, no 2-phase drain).

## References

- [`docs/plan/on-proper-migration-of-lazy-pancake.md`](../plan/on-proper-migration-of-lazy-pancake.md)
  (full wire spec, reconnect FSM, slice plan), [`docs/ha-replication.html`](../ha-replication.html)
  (Mermaid exchange diagrams).
- ADR-0009 (proxy HRW + stickiness cookie + fresh-pod guard),
  ADR-0010 X3 (replication-aware `CallStore` seam — the deferral this fills),
  ADR-0008 (positional msgpack), ADR-0006 (recording-first harness),
  ADR-0002 (crate-per-layer / acyclicity).
- `crates/topology/`, `crates/repl-net/`, `crates/b2bua/src/repl/`,
  `crates/ha-harness/` (to be built per the slice plan).
