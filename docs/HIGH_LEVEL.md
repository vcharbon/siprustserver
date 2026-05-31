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

> **§5 is grounded in the actual source** (`portsource/sipjsserver/src/{cache,
> storage,replication,call}` + `docs/replication/*` + `docs/lb-proxy-ha.md`),
> read directly. The replication **wire protocol is explicitly open to change**
> in the Rust port (the user's call); what we must keep stable is the **storage
> seam and the replication _semantics_** (§5.0). The first draft of this section
> guessed an HRW/Snapshot/Outbox model — that was wrong and has been replaced.

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
     ┌────────────┐      ┌──────────────┐           │
     │  sip-time  │      │sip-foundation│ (shared    │  ← extracted when the
     │ Clock seam │      │ types: CallRef,           │    SipMessage↔Call↔RuleContext
     └─────┬──────┘      │ WorkerId, Role, …)        │    cycle forces it (ADR-0002)
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
     │   • Call / Leg / Dialog model (+ _topology)     │
     │   • CallStore (hot map + per-call single-writer)│
     │   • CallBodyCodec (body serialization)          │
     │   • storage seam:  PartitionedRelayStorage      │  ← HA seam lives here
     │       over KvBackend (in-mem now / networked    │    (§5) — populates the
     │       later)                                     │    replication log single-node
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

The HA replication machinery (puller, repl-log server, readiness controller,
peer bootstrap) is a **future `repl` crate** that depends on `call`'s `KvBackend`
trait — *no `call`/`rules` code changes when it lands* (that is the whole point of §5).

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
| Replication body payload | `call` **`CallBodyCodec`** | ⬜ | storage seam, HA |
| Partition role (pri/bak) | `_topology` cookie parse (HRW is in the **front-proxy**, not the worker) | ⬜ (trivial single-node) | HA |
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
replication transport (the HTTP stream) is **not wired now**, the storage layer
must be shaped so it drops in later without touching `call` or `rules`. The
source already factors this cleanly; we port its **seam and semantics**, wire the
**single-node in-memory backend now**, and defer only the cross-node transport.
**The wire format itself is open to change** (§5.5) — only the seam and semantics
below are fixed.

### 5.0 The replication model we are preserving (from source)

From `docs/lb-proxy-ha.md` + `docs/replication/{architecture,protocol,
call-cache-backup}.md` + `src/cache`, `src/storage`, `src/replication`:

- **Topology is assigned by the front-proxy, not the worker.** At INVITE time the
  `sip-front-proxy` LoadBalancer picks `{w_pri, w_bak}` (primary + backup worker
  ordinals, via HRW/rendezvous on its side) and stamps them into an **HMAC-signed
  Record-Route cookie** that survives on every in-dialog request. The worker just
  **reads** the cookie into `Call._topology { pri, bak, gen }`. So the worker port
  needs *no* HRW resolver — only a cookie parse + a `partition_of` that compares
  `self` to `pri`/`bak`. (HRW lives in the proxy, which is out of scope here.)
- **Single-owner invariant (non-negotiable).** `pri:{P}:call:{ref}` is written
  **only ever by P**. If the proxy fails over to backup `B` while `P` is down, `B`
  serves the request, advances state, bumps `gen`, and writes to **`bak:{P}:call:
  {ref}` on B's own backend** — `B` never becomes primary; the cookie's `w_pri`
  stays `P`. No moment of dual ownership.
- **Storage layout (key-value, role-partitioned).** Each worker's backend holds:
  `pri:{owner}:call:{ref}` (calls it owns), `bak:{owner}:call:{ref}` (warm backups
  of others' calls), `idx:{key}` (flat index → callRef; **derived from the body**,
  see below), and per-peer replication **channels** (`propagate:{self}->{peer}`)
  plus counters/epoch/watermarks.
- **Pull-based replication.** The primary's flush atomically appends a channel
  entry; the backup runs a **puller** that long-polls the primary and **applies**
  entries to its own `bak:` partition. The pull is authoritative and self-healing
  — a missed update is corrected by the next pull. Streaming keeps the backup
  **warm in memory** (sub-second steady-state lag).
- **`callGen` content gate.** `Call._topology.gen` is the per-call version, bumped
  on every flush *before* encoding. The puller applies a frame only if
  `incoming.callGen > local.callGen` (deletes apply unconditionally). Separately,
  a per-channel `(entryGen, counter)` **lex watermark** drives the pull cursor
  (`entryGen` = writer incarnation/epoch, `counter` = per-bucket monotonic). These
  are three distinct numbers — keep them distinct.
- **Indexes are derived from the body, never shipped.** The receiver recomputes
  `idx:*` from the decoded `Call` (`callIndexKeysFromUnknown`) and writes them in
  the same atomic op as the body. The wire carries only the body.
- **Pull-from-backup re-hydration.** On boot/takeover a worker bumps its **epoch**,
  enumerates peers, and pulls each peer's relevant partition until **caught up to
  the head-at-open** (or a 30 s ceiling). A sidecar-wiped boot uses a one-shot
  **peer-scan bootstrap** (pull the peer's `bak:{self}:*` back into local `pri:`).
  The partition stays **not-ready** (the proxy parks/503s SIP for it) until caught
  up; epoch mismatch ⇒ full resync from `since=0`; no reachable peer ⇒ cold start
  (counts lost calls).

### 5.1 Model fields to carry now (even if unused single-node)

On `Call` (port from `CallModel.ts` — do **not** strip these):

```rust
pub struct Call {
    pub call_ref: CallRef,        // "{primaryOrdinal}|{aLegCallId}|{aLegFromTag}" — self-describing
    // ... legs, dialogs, state, timers, cdr, ext (TypeMap) ...
    pub topology: Option<CallTopology>,   // { pri, bak, gen } — from the Record-Route cookie
    pub worker_index: Option<u32>,
    // body also carries written_at_ms (set at flush) for replication-lag measurement
}
pub struct CallTopology { pub pri: WorkerId, pub bak: Option<WorkerId>, pub gen: u64 } // gen = callGen
```

There is **no separate `seq` field** — the per-call version *is* `topology.gen`
(`callGen`). `call_ref` encodes the primary ordinal so any worker can parse it
(`derive_call_ref` / `parse_call_ref`). Stripping `topology`/`gen` or making
`call_ref` non-self-describing is the canonical "seam loss" — **forbidden** (record
as debt if ever done).

### 5.2 The seam to define now (Rust shapes mirroring the source)

Three traits, in dependency order. The **bottom one (`KvBackend`) is the real HA
seam** — it is where the in-memory-now / networked-later swap happens, and its
`channel_write_*` methods populate the replication log on every flush *even
single-node*, so the puller/server are pure additions later.

```rust
// ---------- (1) CallBodyCodec — finalize the body format NOW (port of call/codec) ----------
pub trait CallBodyCodec {                  // strips transient fields (semaphores, sockets, fibers)
    fn encode(&self, call: &Call) -> Bytes;
    fn decode(&self, bytes: &[u8]) -> Result<Call, CodecError>;
}   // start with msgpack (source has msgpack + a protobuf `call.proto` option)

// ---------- (2) PartitionedRelayStorage — worker-facing (port of cache/PartitionedRelayStorage) ----------
pub enum Role { Pri, Bak }
pub trait PartitionedRelayStorage {
    fn put_call(&self, role: Role, owner: WorkerId, call_ref: &CallRef,
                body: Bytes, indexes: &[String], ttl: Duration,
                call_gen: u64, peer: Option<WorkerId>) -> Result<()>;   // → KvBackend.channel_write_update
    fn get_call(&self, call_ref: &CallRef) -> Result<Option<Bytes>>;    // pri:self else bak:{wPri}
    fn delete_call(&self, role: Role, owner: WorkerId, call_ref: &CallRef,
                   peer: Option<WorkerId>) -> Result<()>;               // tombstone + announce
    fn resolve_index(&self, index_key: &str) -> Result<Option<CallRef>>; // flat idx:* lookup
    fn partition_of(&self, call_ref: &CallRef) -> (Role, WorkerId);     // cookie/_topology, not HRW
}

// ---------- (3) KvBackend — THE storage seam (port of storage/KvBackend) ----------
// In-memory impl NOW; networked-KV/redis impl LATER. Same trait either way.
pub trait KvBackend {
    // body store
    fn body_get(&self, key: &str) -> Result<Option<Bytes>>;
    fn body_set(&self, key: &str, val: Bytes, ttl: Duration) -> Result<()>;
    fn body_del(&self, key: &str) -> Result<()>;
    fn body_mget(&self, keys: &[String]) -> Result<Vec<Option<Bytes>>>;
    // primary-side: ATOMIC {body + derived indexes + channel entry}. Returns the channel counter.
    fn channel_write_update(&self, a: ChannelWriteUpdate) -> Result<ChannelWriteResult>;
    fn channel_write_tombstone(&self, a: ChannelWriteTombstone) -> Result<ChannelWriteResult>;
    // server-side: read a batch of channel entries since a (gen,counter) watermark (for the repl stream)
    fn channel_pull_batch(&self, a: ChannelPullBatch) -> Result<ChannelPullResult>;
    // puller-side: ATOMIC apply of a replicated body/delete into the local backend
    fn apply_replica_update(&self, a: ReplicaUpdate) -> Result<()>;
    fn apply_replica_delete(&self, a: ReplicaDelete) -> Result<()>;
    fn counter_read(&self, key: &str) -> Result<u64>;
}
pub struct ChannelWriteResult { pub counter: u64 }
```

The hot map consumers use is `CallStore` (port of `CallState.ts`) — it owns the
in-memory `Call` map + the per-call **single-writer** discipline and delegates
persistence to `PartitionedRelayStorage`:

```rust
pub trait CallStore {
    fn with<R>(&self, call_ref: &CallRef, f: impl FnOnce(&mut Call) -> R) -> Option<R>; // single writer
    fn get(&self, call_ref: &CallRef) -> Option<Call>;
    fn flush(&self, call_ref: &CallRef) -> Result<()>;   // §5.3 (eviction flush via a buffered writer)
    fn remove(&self, call_ref: &CallRef) -> Result<()>;  // tombstone, delayed for retransmits
    fn rehydrate_apply(&self, frame: ReplFrame) -> Result<()>; // §5.0 partition-flip + callGen gate
}
```

### 5.3 The flush path that keeps HA additive

Port the source flow (`CallState.flushToRedis`) exactly — this is what makes the
replication log real from day one:

```
flush(call_ref):
    call        = hot_map[call_ref]
    if body unchanged since last flush:           // flushCache dedup (CallState.ts)
        refresh TTL only; return
    call.topology.gen += 1                         // bump callGen BEFORE encoding (D7 conflict resolution)
    call.body.written_at_ms = clock.now()
    body        = codec.encode(call)
    indexes     = call_index_keys(call)            // derived; never shipped
    (role,owner)= storage.partition_of(call_ref)
    peer        = propagate_peer_for(role, owner, call.topology)   // who backs this call up
    storage.put_call(role, owner, call_ref, body, indexes, ttl, call.topology.gen, peer)
        └─ KvBackend.channel_write_update {ATOMIC: body_set + idx:* + append propagate:{self}->{peer}}
```

Because `channel_write_update` appends to the per-peer channel on every flush,
**turning on HA later is: add a process that reads that channel
(`channel_pull_batch`) and serves it to peers, plus a puller that applies it
(`apply_replica_update`).** No call-site changes in `call` or `rules`. Eviction
and terminate flushes route through a buffered async writer
(`BufferedTerminateWriter`) so the hot path never blocks on persistence.

### 5.4 What we wire NOW vs DEFER

| Component | Single-node now (Slice 4) | HA later (additive, `repl` crate) |
|---|---|---|
| `CallStore` (hot map + single-writer) | full | unchanged |
| `PartitionedRelayStorage` | full (`partition_of` = cookie; single node ⇒ `self==pri`) | unchanged |
| `KvBackend` | **in-memory impl** — *this is the "in-memory backup" store*; `channel_write_*` populate the local replication log | add networked impl + cross-node reads; same trait |
| `CallBodyCodec` | **full now** — body format must be final (msgpack; protobuf optional) | unchanged |
| `_topology` carry + `gen` bump on flush | full | unchanged |
| `rehydrate_apply` (partition-flip + callGen gate) | **logic implemented + unit-tested now** (no wire) | wired to the puller |
| Repl-log server (serves `channel_pull_batch`) | not built | built |
| Puller (long-poll peer, apply) | not built | built |
| Readiness controller + peer-scan bootstrap + epoch | not built (single node = always ready) | built |
| Cookie HMAC verify | not built (proxy concern) | built with the proxy port |

**Key point:** the seam, the body format (`CallBodyCodec`), the atomic
`channel_write_update`, the partition-flip/callGen apply logic, and the in-memory
backend are all **built and tested single-node now**. Only the network transport
and the boot/readiness orchestration are deferred — and they sit behind the
`KvBackend` trait.

### 5.5 Wire protocol — semantics fixed, format negotiable

The source ships a **long-poll NDJSON `/replog`** stream (`GET /replog?caller=&
gen=&counter=&chunk_size=`) with `Data`/`Noop` frames, a `(gen,counter)` lex
watermark, and `Noop` doubling as heartbeat + caught-up signal; rehydration adds a
`/bootstrap` endpoint. **The user has confirmed we may change this for the Rust
port.** What must be preserved is the **semantics** (so the storage seam stays
valid whatever we pick):

1. **Pull/resumable from a `(epoch, counter)` watermark** — reconnect resumes; no
   missed entries; idempotent re-pull of overlapping ranges.
2. **Caught-up signal** distinct from data (so readiness can gate on "caught up to
   head-at-open").
3. **Per-call ordering + `callGen` content gate**; deletes unconditional.
4. **Body-only frames; indexes re-derived by the receiver.**
5. **Epoch handshake** so a wiped/restarted peer triggers a full resync.

Candidate simplifications for the Rust port (decide at the HA slice, record an ADR):
- Replace NDJSON with **length-prefixed msgpack** `[u32 len][bytes]` over a plain
  TCP/HTTP2 stream — this is the *same* framing proposed for the §-recording sink
  in the plan, so one codec serves both. (No version negotiation needed —
  pre-production, [MIGRATION_STRATEGY.md].)
- Or carry the channel over a **gRPC server-stream** if we want backpressure +
  keepalive for free. Either way the `KvBackend.channel_pull_batch` /
  `apply_replica_*` seam is unchanged.

### 5.6 Contracts to satisfy (port these single-node now)

From the source replication tests — these define the behaviour the single-node
seam must already honour, so the networked impl inherits a proven contract:

- **Codec round-trip** (`protocol-codec.test.ts`): `encode`/`decode` of the body
  is lossless; transient fields stripped & rebuilt.
- **Atomic write** (KvBackend tests): body + derived `idx:*` + channel entry land
  atomically or not at all; no half-state visible to a reader.
- **Channel ordering** (`server-emission-loop` + protocol.md): `channel_pull_batch`
  returns entries `(gen,counter) > since`, lex-ordered across buckets, capped at
  limit; idempotent across overlapping ranges; a caught-up/heartbeat marker when
  drained.
- **Apply gate** (`EchoApply` semantics): partition flip (`pri↔bak`); apply only if
  `incoming.callGen > local.callGen`; delete unconditional; `null`/missing body ⇒
  implicit delete; cold local (`callGen = -∞`) ⇒ create-if-absent.
- **Bounded primary cost** (`primary-bounded-cost.test.ts`): repeated flushes of
  the same call **coalesce** in the channel (one live entry per callRef), so the
  log can't grow unbounded under churn.
- **Reverse propagation** (`scenario-reverse-propagation.test.ts`): a backup-side
  write (`bak:{P}`) flows back so that when `P` returns it lands in `pri:{P}`.
- **Rehydration / readiness** (`peer-scan-bootstrap.test.ts` + `ReadinessController`):
  pull a partition from a peer; insert only newer (`callGen`/epoch); partition
  `ready` only after caught-up-to-head (or 30 s ceiling); epoch mismatch ⇒ resync
  from `since=0`; idempotent re-run; cold start counts lost calls.

Single-node, "the backup" is the local in-memory `KvBackend` and "the primary" is
the same node — codec, atomic-write, channel-ordering, and apply-gate tests all run
**without a network**; the bootstrap/readiness tests are written against the seam
and re-run once the transport lands.

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
§0/§7). State changes from rules flow through `ActionExecutor` → `CallStore`, which
is exactly what `flush` (§5.3) later replicates.

---

## 7. High-level tracking (slices)

| Slice | Deliverable | Blocks | Status |
|---|---|---|---|
| 2 | `sip-time` clock seam; `sip-net` (transport, `SignalingNetwork`, recording, simulated); 4 wrappers; DSL v1 | all | ⬜ |
| 3 | `sip-txn` transaction FSMs, router, per-call dispatcher (single-writer) | call, rules | ⬜ |
| 4 | `call` model + `CallStore` + `CallBodyCodec` + **storage seam (§5): `PartitionedRelayStorage` over in-memory `KvBackend`, flush populates the channel, apply-gate logic unit-tested** | rules, HA | ⬜ |
| 5 | `rules` engine + the §4 catalogue; finalize this doc's rule rows | b2bua | ⬜ |
| 6 | `b2bua` binary + criterion benches + kind-endurance A/B vs TS | — | ⬜ |
| (later) | **HA `repl` crate:** networked `KvBackend` + repl-log server + puller + readiness/bootstrap + chosen wire (§5.5) — *additive behind the §5 seam* | — | ⬜ |
| dep | `sip-message/sdp.rs` (critical path for media rules) | §4 media rules | ⬜ |

---

[ADR-0002]: ./adr/0002-cargo-workspace-crate-per-layer.md
[MIGRATION_PLAN_B2B.md]: ./MIGRATION_PLAN_B2B.md
[MIGRATION_STRATEGY.md]: ./MIGRATION_STRATEGY.md
