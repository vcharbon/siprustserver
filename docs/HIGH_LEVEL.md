# HIGH_LEVEL — B2BUA behaviour, dependencies & HA-ready call-context storage

**Status:** living design + tracking doc · **Date:** 2026-05-30
**Companion to:** [MIGRATION_PLAN_B2B.md](./MIGRATION_PLAN_B2B.md) (the slice plan),
[MIGRATION_STRATEGY.md](./MIGRATION_STRATEGY.md) (cross-cutting decisions),
[MIGRATION_STATUS.md](../MIGRATION_STATUS.md) (per-layer status).

This document is the canonical, **high-level** description of the basic B2BUA we
are porting: the rule catalogue, the dependency graph between layers, and — in
detail — the **call-context storage layer**, which must be designed *now* so that
HA (call-context replication across workers) ports in later as an **additive
slice, not a rewrite**.

Status legend (matches MIGRATION_STATUS): ✅ ported · 🟡 scaffolded · ⬜ pending.

---

## 1. What "basic B2BUA" means here

A back-to-back user agent terminates the A-leg dialog and originates an
independent B-leg dialog, bridging them. The basic scope:

- Accept inbound INVITE (A-leg), create an outbound INVITE (B-leg) to a target.
- Relay provisional/final responses A↔B; bridge SDP offer/answer.
- Confirm dialogs (200/ACK), maintain two independent dialog states.
- Relay in-dialog requests (BYE, re-INVITE, OPTIONS, INFO, UPDATE, PRACK, MESSAGE).
- Tear down the peer leg on BYE / CANCEL / timeout; handle the common races.
- Drive all timers off an injected clock.

**Explicitly deferred** (hooks noted, not built): call limiter, overload
controller / two-tier drain, the HTTP `CallDecisionEngine` (stubbed with a static
target), `SERVICE_LAYER` callflow rules, and REFER/transfer.

---

## 2. Architecture & crate dependency graph

```
                         ┌──────────────┐
                         │  sip-message │  (parse/serialize/SDP)  ✅ parser / ⬜ sdp,serializer
                         └──────┬───────┘
                                │
            ┌───────────────────┼───────────────────┐
            ▼                   ▼                    │
     ┌────────────┐      ┌────────────┐             │
     │  sip-time  │      │sip-foundation│ (shared    │  ← extracted when the
     │ Clock seam │      │ types: CallRef,           │    SipMessage↔Call↔RuleContext
     └─────┬──────┘      │ Partition, NodeId, …)     │    cycle forces it (ADR-0002)
           │             └──────┬───────┘             │
           ▼                    │                      ▼
     ┌────────────┐             │              (sip-message used by all above)
     │  sip-net   │  UDP transport, SignalingNetwork, recording
     └─────┬──────┘
           ▼
     ┌────────────┐
     │  sip-txn   │  transaction FSMs, SipRouter, PerCallDispatcher (per-call FIFO)
     └─────┬──────┘
           ▼
     ┌────────────────────────────────────────────────┐
     │  call                                           │
     │   • Call / Leg / Dialog model                   │
     │   • CallStore (hot map + single-writer)         │
     │   • CallCodec (snapshot serialization)          │
     │   • storage seam: SnapshotStore, Outbox,        │  ← HA seam lives here
     │     TopologyResolver, ReplicationService,        │    (§5) — wired single-node now
     │     RehydrationService                           │
     └─────┬───────────────────────────────────────────┘
           ▼
     ┌────────────┐
     │   rules    │  engine (Match/Matcher/RuleExecutor/ActionExecutor) + the rules
     └─────┬──────┘
           ▼
     ┌────────────┐
     │   b2bua    │  composition root (binary) + perf harness
     └────────────┘
```

Acyclic by construction (Rust crates cannot cycle — [ADR-0002]). A shared
`sip-foundation` types crate is extracted at the `call` layer when the
`SipMessage ↔ Call ↔ RuleContext` cycle forces it.

---

## 3. Dependency tracking (what blocks what)

| Capability | Depends on | Status of dependency | Blocks |
|---|---|---|---|
| Any async layer (timers) | `sip-time` clock seam | ⬜ | net, txn, call, rules |
| Send/recv SIP | `sip-net` transport | ⬜ | everything |
| Recording / `scopedAudit` | `sip-net` recording decorator | ⬜ | testkit `scopedAudit` |
| Dialog/transaction rules | `sip-txn` FSMs + dispatcher | ⬜ | all in-dialog rules |
| Refined views (`InDialogRequest`, …) | `sip-message` types | 🟡 (defined) | txn boundary, rules |
| **SDP bridging rules** | `sip-message` **`sdp.rs`** | ⬜ **(critical path)** | offer/answer relay, re-INVITE |
| `RuleContext` (rules see `Call`) | `call` model | ⬜ | rule engine + all rules |
| Snapshot replication | `call` **`CallCodec` + `schema`** | ⬜ | storage seam, HA |
| Partition/primary/backup resolution | `TopologyResolver` (HRW) | ⬜ (trivial single-node) | rehydration, multi-node |
| B-leg target selection | `CallDecisionEngine` **stub** | ⬜ | B-leg creation rule |
| Rule selection semantics | `rules` `Matcher` + registry | ⬜ | rule ordering correctness |

**Critical-path call-out:** the SDP utils (`sip-message/sdp.rs`, currently ⬜)
gate every media-bridging rule. Either land SDP before Slice 5, or ship rules with
an SDP-passthrough stub and backfill.

---

## 4. The basic B2B rule catalogue (high-level + tracking)

Ported from `b2bua/rules/defaults/` (CORE layer). Each rule is a **pure**
`handle(ctx) -> { actions, state }`; the `ActionExecutor` applies the actions.
Selection order: kind gate → column match → `filter` → layer (SERVICE>CORE) →
registration order, first-match-wins (port the source `Matcher` + the
unreachable-rule startup check). See §6 for the action vocabulary.

Legend per row — **Port:** direct (D) / re-express (R); **Dep:** extra dependency.

### 4.1 Session establishment
| Rule | Trigger (match) | Key actions | Port | Dep | Status |
|---|---|---|---|---|---|
| **B-leg create / bridge** (`InitialInviteHandler`+`TargetAdmission`) | request INVITE from-a, no peer | `create-leg(destination)`, `relay-to-peer`, `merge`, set `activePeer` | R | `CallDecisionEngine` stub; SDP | ⬜ |
| `relayProvisionalRule` | response 1xx from-b | `relay-to-peer` (map to a-leg), `send-provisional-to-leg` | D | SDP (early media) | ⬜ |
| `confirmDialogRule` | response 2xx/early from-b | `confirm-dialog`, `stamp-dialog-to-tag`, `relay-to-peer` | D | SDP (answer) | ⬜ |

### 4.2 In-dialog relay
| Rule | Trigger | Key actions | Port | Dep | Status |
|---|---|---|---|---|---|
| `relayByeRule` | request BYE | `relay-to-peer`, `terminate-leg` | D | — | ⬜ |
| `relayAckRule` | request ACK | `ack-leg` / `relay-to-leg` | D | — | ⬜ |
| `relayReinviteRule` | in-dialog re-INVITE | `relay-to-peer` | R | SDP | ⬜ |
| `relayReinviteResponseRule` | response to re-INVITE | `relay-to-peer` | R | SDP | ⬜ |
| `relayOptionsRule` / `relayInfoRule` / `relayUpdateRule` / `relayMessageRule` / `relayPrackRule` | matching method | `relay-to-peer` / `send-request-to-leg` | D | — | ⬜ |
| `relayNonInvite200Rule` | 200 for non-INVITE | `relay-to-peer` | D | — | ⬜ |

### 4.3 Termination & lifecycle
| Rule | Trigger | Key actions | Port | Dep | Status |
|---|---|---|---|---|---|
| `handleCancelRule` | request CANCEL from-a | `terminate-leg`, `respond 487`, `begin-termination` | D | — | ⬜ |
| `resolveCancelResponseRule` | 3xx–6xx for cancelled INVITE | `terminate-leg`, cleanup | D | — | ⬜ |
| `handleTimeoutRule` | timeout (Timer B/F) | `terminate-leg`, `begin-termination` | D | clock | ⬜ |
| `resolveByeResponseRule` | 200 for BYE | finalize `byeDisposition` | D | — | ⬜ |
| `resolveCrossByeRule` | simultaneous BYE | reconcile both legs | D | — | ⬜ |
| `terminatingSafetyTimeoutRule` | terminating > 64s | force-purge call | D | clock | ⬜ |

### 4.4 Provisional/response absorption & corner cases
| Rule | Trigger | Key actions | Port | Status |
|---|---|---|---|---|
| `absorbBye200Rule` / `absorbNotify200Rule` / `absorbOptions200Rule` | spurious 200 | drop (no relay) | D | ⬜ |
| `retransmit200Rule` | retransmitted 2xx INVITE | re-`relay-to-peer` ACK path | D | ⬜ |
| `cancel200CrossingRule` | CANCEL/200 race | resolve race | D | ⬜ |
| `reinviteGlareRule` | simultaneous re-INVITE | 491 handling | D | ⬜ |

### 4.5 Timers & failure
| Rule | Trigger | Key actions | Port | Dep | Status |
|---|---|---|---|---|---|
| `maxDurationRule` | timer max-duration | `begin-termination` | D | clock | ⬜ |
| `keepaliveRule` / `keepaliveTimeoutRule` | timer keepalive | `send-request-to-leg OPTIONS` / terminate | D | clock | ⬜ |
| `routeFailureRule` | callControl failure | `respond`, failover | R | stub | ⬜ |
| `noAnswerFailoverRule` | no-answer timeout | `create-leg` (next target) / `respond` | R | stub | ⬜ |
| `absorbStaleFailureRule` | stale failure response | drop | D | — | ⬜ |

> Authoring rule: for each rule, write its row's intent + match + actions here,
> then implement the pure `Rule`, port its tests, add a DSL scenario, flip status.
> Record any un-ported rule with justification (migration ritual).

---

## 5. Call-context storage layer (the HA seam) — design in detail

**This is the load-bearing section.** The user's constraint: even though the
stream-based HTTP replication transport and the SWIM cluster are **not wired now**,
the storage layer must be shaped so they drop in later without touching the call
or rules layers. We achieve that by porting the source's seams (`CallState`,
`PartitionedRelayStorage`, `Outbox`, `Snapshot`, `TopologyResolver`,
`ReplicationService`, `RehydrationService`, `CallCodec`) as **traits**, wiring the
**single-node in-memory impls now**, and deferring only the cross-node transport.

### 5.0 The replication model we are preserving (from source)

From `docs/lb-proxy-ha.md` + `docs/replication/{DESIGN,protocol,rehydration}.md`:

- **Topology by HRW (rendezvous) hashing.** Each call key ranks the cluster nodes
  identically on every node; `rank[0]` = **primary**, `rank[1]` = **backup**.
  Stored on the call as `_topology { pri, bak, gen }`; `gen` bumps when the pair
  changes (membership churn) so stale replicas are detectable. Minimal churn:
  removing a node only remaps calls that hashed to it.
- **Pull-based, self-healing.** Primary mutates state and, after each
  state-changing step, **publishes a snapshot** to a local **outbox**
  (last-writer-wins per callRef, bounded/coalescing). It **pushes** to the backup
  fire-and-forget to keep it **warm in memory**. The **backup periodically PULLs**
  to reconcile — *the pull is authoritative; pushes are only an optimization*. A
  missed push is corrected by the next pull.
- **Snapshot = `{ callRef, gen, seq, payload }`.** `seq` is a monotonic per-call
  sequence (last-writer-wins; lower `seq` dropped). `payload` is the
  msgpack-encoded `Call` via `CallCodec` (transient fields — semaphores, live
  socket refs, fibers — stripped; rebuilt on decode).
- **Pull-from-backup re-hydration.** When a node becomes primary for a partition
  it didn't own (startup / membership change / takeover), it pulls the partition's
  snapshots from the surviving backup, decodes, and `insertIfNewer` into its hot
  map. The partition stays **not-ready** (inbound SIP parked / 503'd by the proxy)
  until the stream completes. Falls back to the next replica; starts cold (counts
  lost calls) only if none reachable. Snapshots with older `gen` than local
  topology are dropped.

### 5.1 Model fields to carry now (even if unused single-node)

On `Call` (port from `CallModel.ts` — do **not** strip these):

```rust
pub struct Call {
    pub call_ref: CallRef,        // "{ordinal}|{aLegCallId}|{aLegFromTag}"
    // ... legs, dialogs, state, timers, cdr, ext (TypeMap) ...
    pub seq: u64,                 // monotonic per-call snapshot sequence
    pub topology: Option<CallTopology>,   // { pri, bak, gen }
    pub worker_index: Option<u32>,
    pub partition: Option<Partition>,
}
```

Stripping `seq`/`topology`/`partition` is the canonical "seam loss" that turns the
future HA slice into a rewrite — **forbidden** (record as debt if ever done).

### 5.2 Traits to define now (Rust shapes mirroring the source)

```rust
// ---- identity + ordering + payload (port of snapshot.ts) ----
pub struct Snapshot { pub call_ref: CallRef, pub gen: u64, pub seq: u64, pub payload: Bytes }
pub fn snapshot_of(call: &Call, encode: impl Fn(&Call) -> Bytes) -> Snapshot {
    Snapshot { call_ref: call.call_ref.clone(),
               gen: call.topology.map(|t| t.gen).unwrap_or(0),
               seq: call.seq, payload: encode(call) }
}

// ---- serialization boundary (port of CallCodec.ts + schema.ts) ----
pub trait CallCodec {                 // strips transient fields; schema-validated
    fn encode(&self, call: &Call) -> Bytes;
    fn decode(&self, bytes: &[u8]) -> Result<Call, CodecError>;
}

// ---- topology resolver (port of topology.ts + hrw.ts) ----
pub struct CallTopology { pub pri: NodeId, pub bak: NodeId, pub gen: u64 }
pub trait TopologyResolver {
    fn resolve(&self, call_key: &str) -> CallTopology;
    fn partition_of(&self, call_key: &str) -> Partition;   // stable hash mod N
    fn is_primary(&self, call_key: &str) -> bool;
    fn is_backup(&self, call_key: &str) -> bool;
    fn generation(&self) -> u64;
}

// ---- snapshot store = the call-cache seam (port of PartitionedRelayStorage) ----
pub trait SnapshotStore {
    fn put(&self, call_ref: &CallRef, partition: Partition, seq: u64, payload: Bytes) -> Result<()>;
    fn get(&self, call_ref: &CallRef) -> Result<Option<Bytes>>;            // last-writer-wins by seq
    fn get_partition(&self, partition: Partition) -> Result<Vec<(CallRef, u64, Bytes)>>; // → Stream later
    fn remove(&self, call_ref: &CallRef) -> Result<()>;
    fn size(&self) -> usize;
}

// ---- outbox (port of outbox.ts): bounded, coalescing, lww-per-callRef ----
pub trait Outbox {
    fn offer(&self, snap: Snapshot);
    fn since(&self, since: u64, limit: usize) -> Vec<Snapshot>;
    fn latest(&self, call_ref: &CallRef) -> Option<Snapshot>;
    fn for_partition(&self, partition: Partition) -> Vec<Snapshot>;
    fn drop(&self, call_ref: &CallRef);
    fn size(&self) -> usize;
}

// ---- replication service (port of ReplicationService.ts) ----
pub trait ReplicationService {
    fn publish(&self, snap: Snapshot) -> Result<()>;          // primary: → outbox
    fn push_to_backup(&self, snap: Snapshot) -> Result<()>;   // fire-and-forget (no-op single-node)
    fn pull_since(&self, since: u64, limit: usize) -> Result<Vec<Snapshot>>;   // serve pull
    fn pull_one(&self, call_ref: &CallRef) -> Result<Option<Snapshot>>;
    fn pull_partition(&self, partition: Partition) -> BoxStream<Snapshot>;     // takeover/rehydrate
    fn reconcile(&self, primary: NodeId, partition: Partition) -> Result<Reconciled>; // backup-driven
}
pub struct Reconciled { pub applied: usize, pub high_water: u64 }

// ---- rehydration service (port of RehydrationService.ts) ----
pub trait RehydrationService {
    fn rehydrate_partition(&self, partition: Partition) -> Result<Restored>;
    fn is_ready(&self, partition: Partition) -> Result<bool>;
    fn start(&self) -> Result<()>;
}
pub struct Restored { pub restored: usize, pub ready: bool }

// ---- the hot store consumers actually use (port of CallState.ts) ----
pub trait CallStore {
    fn with<R>(&self, call_ref: &CallRef, f: impl FnOnce(&mut Call) -> R) -> Option<R>; // single writer
    fn get(&self, call_ref: &CallRef) -> Option<Call>;
    fn persist(&self, call: &Call) -> Result<()>;        // bump seq → encode → put → publish
    fn insert_if_newer(&self, call: Call, gen: u64, seq: u64) -> bool;  // rehydration entry
    fn flush(&self, call_ref: &CallRef) -> Result<()>;   // encode → put → evict from hot map
    fn remove(&self, call_ref: &CallRef) -> Result<()>;  // terminate (delayed for retransmits)
}
```

### 5.3 The one method that keeps HA additive: `CallStore::persist`

Port the source flow exactly (`CallState.ts`):

```
persist(call):
    seq        = bump_seq(call.call_ref)        // monotonic per call
    call.seq   = seq
    partition  = topology.partition_of(call.call_ref)
    payload    = CallCodec.encode(call)
    snapshot_store.put(call.call_ref, partition, seq, payload)   // warm copy
    replication.publish(snapshot_of(call, encode))              // → outbox (→ push/pull later)
```

Because `persist` already routes through `SnapshotStore` + `ReplicationService`,
turning on HA later is: swap the in-memory `SnapshotStore`/no-op `push_to_backup`
for the networked impls and start the backup's pull loop. **No call-site changes
in `call` or `rules`.**

### 5.4 What we wire NOW vs DEFER

| Component | Single-node now (Slice 4) | HA later (additive) |
|---|---|---|
| `CallStore` | hot `HashMap` + per-call single-writer (actor/owned-task), `persist`/`flush`/`insert_if_newer`/`remove` | unchanged |
| `SnapshotStore` | **in-memory** (`PartitionedRelayStorageMemory` port) — *this is the "in-memory backup"* | peer in-memory stores + networked reads |
| `Outbox` | bounded coalescing buffer, exercised by `publish` | drained by backup pulls |
| `ReplicationService` | `publish`→outbox; `push_to_backup`=no-op; `pull_*` serve from local outbox/store | HTTP/1.1 chunked streaming transport (see protocol below) |
| `TopologyResolver` | single node: `pri=bak=self`, `gen=0`, `partition_of`=hash mod N | HRW over SWIM member list; `gen` bumps on churn |
| `RehydrationService` | reads local snapshot store (cold start = empty); `is_ready` gates intake | pull-from-backup over the wire; partition parking |
| `CallCodec` | **full port now** (msgpack + transient-field strip) — the payload format must be final | unchanged |
| Cluster (`SwimCluster`, `hrw`) | **not ported** (stub `TopologyResolver`) | SWIM membership + HRW |
| HTTP repl transport | **not ported** | the `/repl/*` endpoints below |

**Key point:** the *seam, the payload format (`CallCodec`), the ordering
(`seq`/`gen`), and the single-node impls are all built now and tested
single-node*. Only the network transport and cluster are deferred.

### 5.5 The wire protocol to preserve (defer the impl, fix the shape)

Port shape later (`docs/replication/protocol.md`) — recorded here so the traits
above already match it:

- `GET /repl/snapshots?since=<seq>&limit=<n>` → `pull_since` (incremental reconcile)
- `GET /repl/snapshot/<callRef>` → `pull_one` (targeted repair)
- `GET /repl/all?partition=<p>` → `pull_partition` (takeover re-hydration)
- **Framing:** length-prefixed msgpack `[u32 len][bytes]`, zero-length frame ends
  the stream. **Idempotent by `seq`** (re-pulling overlapping ranges is safe; a
  pull never mutates the primary). Note: this `[u32 len][msgpack]` framing is the
  *same* shape proposed for the §3-recording sink in the plan — share the codec.

### 5.6 Contracts to satisfy (port these tests single-node now)

From the source replication tests — these define the behaviour the single-node
impls must already honour, so the networked impls inherit a proven contract:

- **Topology** (`topology.test.ts`): HRW ranking stable & identical across nodes;
  `pri=rank[0]`,`bak=rank[1]`; `gen` bumps when the pair changes; minimal churn;
  `partition_of` = stable hash mod N.
- **Replication** (`ReplicationService.test.ts`): publish→outbox; `pull_since`
  returns `seq>since` capped at limit; idempotent across overlapping ranges;
  last-writer-wins (lower seq dropped); `push_to_backup` never throws on transport
  error; `pull_partition` streams all; outbox coalesces on overflow; terminated
  call dropped.
- **Rehydration** (`RehydrationService.test.ts`): rehydrate a partition from
  snapshots; insert only newer (gen/seq); partition `ready` only after stream
  completes; SIP parked/503'd until ready; fall back to next replica; cold-start
  counts lost calls; drop snapshots with older `gen`.

Single-node, "the backup" is the local in-memory `SnapshotStore` and "the
primary" is the same node — every test above runs without a network, and the
networked impls slot behind the same traits.

---

## 6. Rule action vocabulary (reference for §4)

The `RuleAction` enum to port (`b2bua/rules/framework/actions/`): `relay-to-peer`,
`relay-to-leg`, `respond`, `ack-leg`, `send-provisional-to-leg`,
`send-request-to-leg`, `send-prack-to-leg`, `confirm-dialog`, `update-leg-state`,
`stamp-dialog-to-tag`, `create-leg`, `destroy-leg`, `merge`, `split`,
`schedule-timer`, `cancel-timer`, `begin-termination`, `terminate-leg`,
`add-cdr-event`, `set-rule-state`, … Applied by `ActionExecutor`; post-conditions
checked by `InvariantEnforcer` / `ByeDispositionInvariant`. Rules never mutate
`Call` directly or perform I/O (the purity boundary — see [MIGRATION_PLAN_B2B.md]
§0/§7 and the HA invariants in §5).

---

## 7. High-level tracking (slices)

| Slice | Deliverable | Blocks | Status |
|---|---|---|---|
| 2 | `sip-time` clock seam; `sip-net` (transport, `SignalingNetwork`, recording, simulated); 4 wrappers; DSL v1 | all | ⬜ |
| 3 | `sip-txn` transaction FSMs, router, per-call dispatcher (single-writer) | call, rules | ⬜ |
| 4 | `call` model + `CallStore` + `CallCodec` + **storage/replication seam (§5) wired single-node** | rules, HA | ⬜ |
| 5 | `rules` engine + the §4 catalogue; finalize this doc's rule rows | b2bua | ⬜ |
| 6 | `b2bua` binary + criterion benches + kind-endurance A/B vs TS | — | ⬜ |
| (later) | **HA:** SWIM cluster + HRW + HTTP `/repl/*` transport + backup pull loop — *additive, behind §5 traits* | — | ⬜ |
| dep | `sip-message/sdp.rs` (critical path for media rules) | §4 media rules | ⬜ |

---

[ADR-0002]: ./adr/0002-cargo-workspace-crate-per-layer.md
[MIGRATION_PLAN_B2B.md]: ./MIGRATION_PLAN_B2B.md
[MIGRATION_STRATEGY.md]: ./MIGRATION_STRATEGY.md
