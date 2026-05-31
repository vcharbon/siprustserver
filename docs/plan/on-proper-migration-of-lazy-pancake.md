# HA replication migration — peer-to-peer, no Redis

## Context

The JS server (`../sipjsserver`) achieves call-context high availability through a
**pull-based replication log with watermarks**, *mediated by Redis*: every primary
dual-writes call state into Redis (`pri:{owner}` / `bak:{owner}` partitions) and into
per-peer `propagate:{peer}` ZSETs; backups long-poll a `/replog` stream and apply
anything newer than their watermark; a rebooting primary re-hydrates via `/bootstrap`.
Redis was only the *substrate* — the real mechanism is the log + watermark + pull.

The Rust port **drops Redis** (per `docs/MIGRATION_STRATEGY.md`). The mechanism becomes
**direct peer-to-peer**: each b2bua is *both* a replication server (exposes its change
stream + a bootstrap scan) and a client (pulls from every peer). The HA *seam is already
carved*: `CallStore` already takes `role`/`primary`/`peer`/`direction`/`call_gen`/`ttl`
but `InMemoryCallStore` **no-ops all of them**; `callRef = {primary}|{callId}|{fromTag}`
encodes ownership; the proxy already emits the `w_pri`/`w_bak` HRW stickiness cookie and
has a `WorkerHealth::NotReady` state pre-wired. Missing: the transport, the in-process
change log (replacing Redis ZSETs), the puller/supervisor, bootstrap, the k8s topology
provider, and the readiness state machine.

Goal: three tiers of testability — (1) a **pure HA-framework layer** test (several
in-process nodes; data in/out; reboot; network faults; always under fake clock); (2)
**fully simulated** failover (fake net + fake clock: proxy + ≥2 b2buas + crash/reboot/
reclaim); (3) **real chaos** via kind. Critical, cross-cutting work, sliced below.

---

## Settled decisions

1. **Transport = its own seam, message-granular, reliable, connection-oriented**
   (one whole encoded frame = one byte array), parallel to SIP's `SignalingNetwork`.
   Real = tokio TCP + length-prefix; sim = in-process. The frame **codec plays through
   in every path** and each message is recording-capturable. *Rationale (ADR):* real
   `TcpStream` I/O does **not** obey `tokio::time::pause`, so fake-clock tests (goals
   1–2) **cannot** use real TCP — the sim transport is mandatory. Real TCP is goal-3.
2. **Forward-replication source = per-peer ordered, compacted (latest-per-callRef)
   changelog of `callRef` *references*** (`{counter, callRef, op}`); the **body is read
   live from the store at send time**; deletes leave a TTL-reaped tombstone. Faithful to
   the JS Redis ZSET (callRef-keyed, counter-scored → already compacted). **Auto-clean**:
   a dead peer's cursor is dropped; it re-bootstraps on return. Compacted ⇒ the log
   always equals the live set, so a cold puller from `(gen,0)` gets everything.
3. **Re-hydration (bootstrap) = snapshot-keys + lazy batched stream + conservative
   watermark + tail.** Copy `bak:{primary}` callRef *keys* under a brief lock, release,
   stream bodies in ~128 batches reading each current body under a short lock; capture
   `W = head` **at scan start**; client seeds watermark to `W` and **keeps pulling**, so
   acting-backup mutations during/after the scan (`counter > W`) are re-delivered
   (idempotent by `callGen`). A slow/crashing puller never holds the map lock.
   **Re-hydration and backup-re-subscription are the same pull stream**: frames are
   `partition`-tagged (`pri` = reclaim, `bak` = back-up). Bootstrap = bulk pre-seed.
4. **Fail-back = readiness-gated reclaim + hard-timer backstop.** Post-bootstrap the
   delta is small. A **hard timer** bounds re-hydration and (a) breaks the fail-back
   deadlock (flip at expiry even if the backup keeps writing) and (b) lets a node **boot
   and serve even when peers are unreachable** — re-hydration is best-effort and never
   blocks startup. Liveness over completeness. Flip-instant race covered by SIP
   retransmit + the proxy's existing ACK/CANCEL-to-primary rule.
5. **Topology = promote + factor.** Extract a shared **membership crate**
   (`Peer{ordinal, host}` + `snapshot()` + `changes()` watch; `Static | Simulated | K8s`
   impls, **k8s watcher written once**) out of the proxy's `WorkerRegistry`. The proxy's
   health/draining/fresh-pod/`ProxyAddr` become a thin proxy-side layer over it; the
   b2bua depends on the membership core only, deriving its repl address from
   `ordinal+host+config`. "Elements I back up" *dissolves* into per-peer changelog
   contents (HRW 2nd-best, full mesh) — no separate retrieval.
6. **Readiness/OPTIONS = re-hydrated + backup-current.** Add `NotReady → Ready →
   Draining` (today the b2bua always answers 200, [router.rs:80-98](../../crates/b2bua/src/router.rs#L80-L98)).
   Self-reported via OPTIONS: `200`=alive, `503+not-ready`, `503+draining/Retry-After:0`;
   consumed by the proxy's existing `WorkerHealth`. **Ready** when bootstrap re-hydration
   done for all *reachable* peers (best-effort, hard-timer bounded) **and** the forward
   pulls are "current" — the **`current` flag set the instant the head `Noop` arrives**
   (per-peer, sticky across reconnects; JS `everCaughtUp`). SIGTERM → Draining.
7. **Goal-1 node = replication subsystem only, always with timer.** `{CallStore +
   per-peer changelog + sim ReplicationNetwork + puller/supervisor + topology view +
   Clock(test_at) + timer service}`, no SIP/router/rules, under `tokio::time` pause/
   advance. Controls: `node.put/delete`, `node.crash()` (drop tasks + **wipe** memory),
   `node.reboot()` (same ordinal, empty memory, bootstrap+resubscribe),
   `fabric.partition/delay/cut/reconnect`.
8. **Sequencing = hybrid**, and **the HA harness generates a recording-first report**
   of the replication message exchange (reusing the scenario-harness renderers).
9. **Body materialization = store `Arc<[u8]>`, zero-copy snapshot read.** The store
   already holds the encoded body (flush wrote it); the drain reads it back via
   `get_call` returning an `Arc<[u8]>` clone (refcount bump, no copy, no re-encode, no
   contention with the typed routing map). Safe under concurrent rewrite by the
   **immutable-shared-body invariant** (a rewrite swaps the slot to a new `Arc`; the
   in-flight drain keeps its old `Arc` alive) — the same `ArcSwap` discipline the proxy
   registry already uses.

---

## Wire protocol (`repl-net`)

**Contract:** the transport moves *whole messages* — each a **positional-msgpack array**
(ADR-0008 ethos), tag-discriminated by element 0. Real TCP delimits with a 4-byte BE
length prefix (the only real-transport-only concern; unit-tested separately); the sim
delivers the `Vec<u8>` whole; the recording decorator captures+decodes each message.
**Two generations (never conflate):** `gen` = incarnation (per worker-restart, high word
of the watermark); `call_gen` = content version (`CallTopology.gen`, LWW).

```
# client -> server
PullRequest      [0, proto_ver:u16, caller:str, mode:u8, since_gen:u64, since_counter:u64, chunk:u32]
                 # mode 0=Replog (tail), 1=Bootstrap (re-hydrate); since_* ignored for Bootstrap
Ack              [1, caller:str, up_to_gen:u64, up_to_counter:u64]      # optional; lets server trim retention

# server -> client
Data  [2, gen:u64, counter:u64, op:u8, partition:u8, call_ref:str, call_gen:i64, body_ttl_ms:i64, indexes:[str], body:bin|nil]
                 # op 0=create 1=update 2=delete; partition 0=pri 1=bak (derivable from call_ref)
                 # body = positional-msgpack Call (the Arc<[u8]> read from the store); nil for delete/expired
Noop             [3, gen:u64, counter:u64]                              # caught-up marker / bootstrap terminal (head)
ResetToBootstrap [4, reason:str]                                       # since fell off the compacted tail -> re-pull
```
*(Dropped from JS: `latency_ms`, `__writtenAtMs` — vestigial.)*

**Exchange — steady-state (mode=Replog), push-to-buffer / client-pull:**
```
A → B:  PullRequest(Replog, since=(gW,cW), chunk=128)        # ONCE; opens a subscription
B → A:  Data… (entries (gen,counter)>since, ascending; partition=bak: B's calls A backs up;
               partition=pri: A's calls B took over [Reverse])
B → A:  Noop(head)                                          # drained -> A sets current=true (sticky)
        …B keeps PUSHING new Data as calls mutate; Noop again when idle…
A → B:  Ack(up_to)                                          # periodic; B trims retention
```
**Exchange — re-hydration (mode=Bootstrap):**
```
A → B:  PullRequest(Bootstrap, chunk=128)
B: snapshot bak:A KEYS (brief lock); W = head AT SCAN START
B → A:  Data(op=create, partition=pri, …)…                  # lazy ~128/batch body reads
B → A:  Noop(W)                                             # TERMINAL: end of bootstrap
A: import as pri:A; seed Replog watermark to W; then PullRequest(Replog, since=W) on same conn
```
**Apply gate / edge cases:** apply `Data` iff `(gen,counter) > watermark`, then advance;
`Noop` → `current=true`, advance if greater. **LWW**: if local `call_gen ≥ frame.call_gen`,
skip the body write but still advance (idempotent re-delivery, multi-source-safe).
`delete` → remove + brief tombstone. **Reboot/incarnation**: a rebooted worker serves
under a higher `gen`, counter reset to 0; `(new_gen,0) > (old_gen,*)` so pullers apply
without a manual reset. **Missed delete** during a disconnect self-evicts via the call
TTL (`body_ttl_ms` / store `ttl`) — JS-faithful backstop, no prune pass.

---

## Streaming, buffering & lock discipline

- **Server never blocks the call path.** The call-mutation path does a **non-blocking**
  changelog bump (move the callRef ref to a new counter + notify) — mirrors
  `BufferedTerminateWriter::submit_put` (`try_send`,
  [store/terminate_writer.rs](../../crates/b2bua/src/store/terminate_writer.rs)). It
  touches no socket and waits on no subscriber.
- **No app-level eviction buffer.** Backpressure = TCP flow-control + the OS socket
  buffer; the **compacted changelog is the bounded backing** (bounded by live calls). A
  slow client's cursor just lags and reads **latest-per-call** when it catches up (stale
  intermediates shed for free). The server keeps writing until TCP dies (drop subscriber
  + cursor) or the client catches up.
- **The invariant:** neither the append path nor the drain path holds a lock on the call
  DB *or* the changelog across any I/O/await, and both **survive the call being removed
  mid-flight.** The drain: brief lock → `get_call` returns `Arc<[u8]>` (refcount bump) →
  **drop guard** → `socket.write(arc).await` on owned bytes. If the callRef is already
  gone → emit `delete`. Concurrent rewrite is safe (Decision 9).

---

## Puller reconnect state machine (per-peer, client side)

```
States:  Parked · Connecting · Bootstrapping · Tailing{ever_current} · Backoff{attempt}
```
| From | Trigger | To | Action |
|---|---|---|---|
| (start) | topology adds peer | Connecting | — |
| Connecting | ok, **self cold-start/reset** | Bootstrapping | `PullRequest(Bootstrap)` |
| Connecting | ok, **self has state** (TCP blip) | Tailing | `PullRequest(Replog, since=retained W)` |
| Connecting | connect fails | Backoff | `attempt++` |
| Bootstrapping | terminal `Noop{head}` | Tailing | seed W=head; `PullRequest(Replog, since=W)` |
| Tailing | `Data (gen,ctr)>W` | Tailing | apply (LWW); W=(gen,ctr) |
| Tailing | `Noop` | Tailing | **`ever_current=true`** (sticky) |
| Tailing | `ResetToBootstrap` | Bootstrapping | discard W; re-pull |
| Tailing/Bootstrapping | recv None / send fail (**peer crash/reboot**) | Backoff | retain W |
| Backoff | `min(init·2^attempt, max)` elapsed | Connecting | — |
| any | topology **removes** peer | Parked | interrupt; **retain W forever** |
| Parked | topology **re-adds** peer (maybe new addr) | Connecting | reconnect from retained W |

**k8s pod-reboot walkthrough:** B dies → TCP disconnect (`Tailing→Backoff`) or topology
`Removed` (`→Parked`), **W retained per-ordinal**. B returns (maybe new IP →
`AddressChanged`) → `Connecting→Tailing` from W. B restarted with a higher `gen`,
counter 0, so every `(new_gen,*)` frame beats W → B serves its full compacted changelog
and the puller applies it; missed deletes self-evict via TTL. **`ever_current` is sticky
across reconnects** — transient disconnects do not revert the node to NotReady (Decision
6); live liveness is the proxy's OPTIONS-timeout→Dead concern.

---

## Components & files

### New crates
- **`crates/topology/`** — shared membership (S1): `trait Membership { snapshot() ->
  Vec<Peer>; changes() -> broadcast::Receiver<MemberDelta> }`, `Peer{ordinal,host}`;
  `StaticMembership`, `SimulatedMembership` (clock-injected, test-driven; mirror
  [registry/simulated.rs](../../crates/sip-proxy/src/registry/simulated.rs)),
  `K8sMembership` (EndpointSlice informer — S11). Backed by `ArcSwap` + `broadcast`
  (lift `RegistryState`, [registry/mod.rs:86-122](../../crates/sip-proxy/src/registry/mod.rs#L86-L122)).
- **`crates/repl-net/`** — transport + frame codec (S2,S3), parallel to `sip-net`,
  **call-agnostic** (opaque `Vec<u8>`/`Arc<[u8]>` bodies): `Frame` enum + positional
  msgpack codec + length-prefix; `trait ReplicationNetwork { connect; listen }`,
  `ReplicationConnection{ send; recv }`; `SimulatedReplicationNetwork` (in-process,
  ordered, **bounded-buffer/non-blocking-append**, fault switchboard:
  delay/stall/cut/partition/reconnect, paused-time-cooperative), `RealReplicationNetwork`
  (tokio TCP), `RecordingReplicationNetwork` (captures+decodes — feeds the report).
- **`crates/ha-harness/`** — goal-1 pure-HA harness (S9): `HaCluster` of N nodes,
  always fake-clock; `crash/reboot/partition` + recording report.

### Modified / extended
- **`crates/sip-proxy/src/registry/`** — refactor `WorkerRegistry` to layer
  health/draining/`ProxyAddr`/`lookup_by_address` over `topology::Membership` (S1);
  preserve ADR-0002 acyclicity (proxy → topology, not the reverse).
- **`crates/b2bua/src/repl/`** *(new module, S4–S8)* — engine, coupled to
  `CallStore`/`CallState`:
  - `ReplicatingCallStore` over `InMemoryCallStore`: honour `peer`/`direction`/`call_gen`/
    `ttl` ([store/call_store.rs:54-104](../../crates/b2bua/src/store/call_store.rs#L54-L104));
    `put_call` stores body **and** bumps the per-peer changelog atomically; store holds
    `Arc<[u8]>` bodies, `get_call` returns Arc clones (Decision 9).
  - `Changelog` (compacted ordered ref-log; tombstone TTL; dead-peer auto-clean).
  - `Puller` + `Supervisor` (FSM above; watermark apply-gate; `current`-on-`Noop`;
    reconcile via `topology::changes()`; watermark retention per ordinal).
  - `Bootstrap` server scan + client import + seed + tail.
  - `Readiness` state machine + OPTIONS responder (replace always-200 at
    [router.rs:80-98](../../crates/b2bua/src/router.rs#L80-L98)).
  - Incarnation-gen source (injectable; real = boot wall-clock/k8s pod epoch; test =
    seed — mirror `IdGen::seeded`).
- **`crates/b2bua-harness/`** — goal-2 (S10): proxy SUT + ≥2 b2bua SUTs + repl fabric +
  crash/reboot; extend `B2buaSut`
  ([b2bua-harness/src/lib.rs](../../crates/b2bua-harness/src/lib.rs)).
- **`deploy/`** + chaos crate — goal-3 (S11).

### Reuse (do not reinvent)
`CallStore` HA params / `PartitionRole` / `PropagateDirection`
([store/call_store.rs](../../crates/b2bua/src/store/call_store.rs)); `derive_call_ref` /
`partition_of` / `call_index_keys`
([crates/call/src/callref.rs](../../crates/call/src/callref.rs)); `CallTopology` + Call
msgpack codec ([crates/call/src/model.rs](../../crates/call/src/model.rs));
`BufferedTerminateWriter` / `CallState::load_owned` / `flush`
([crates/b2bua/src/store/](../../crates/b2bua/src/store/)); `RegistryState` (ArcSwap+
broadcast) + `SimulatedWorkerRegistry`; `Clock::test_at` + `advance_in_100ms_chunks`;
`SimulatedSignalingNetwork.send_fault`; recording-first harness + SVG/text/HTML renderers
(ADR-0006); `IdGen::seeded`; proxy `WorkerHealth::NotReady` + fresh-pod guard +
ACK/CANCEL-to-primary.

---

## Slice plan (hybrid sequencing)

| Slice | Scope | Test focus | Deps |
|---|---|---|---|
| **S1** | `topology` crate; refactor proxy registry onto it | membership deltas; scale/restart; proxy regression | — |
| **S2** | `repl-net` frame model + codec + length-prefix | round-trip property tests; framing | — |
| **S3** | `repl-net` transport seam (sim+bounded-buffer+fault+recording, real TCP) | connect/send/recv; faults; non-blocking append; recording | S2 |
| **➤ vertical skeleton** | minimal S4+S5: `put on A → appears on B` under fake clock | first goal-1 test | S1–S3 |
| **S4** | `ReplicatingCallStore` + compacted changelog + `Arc<[u8]>` bodies | mutation→entries; compaction; auto-clean (timer) | store |
| **S5** | Puller + supervisor (FSM, watermark, current-on-Noop, backoff, reconcile) | convergence; watermark retention; backoff (timer) | S1–S4 |
| **S6** | Bootstrap / re-hydration (lazy-batch scan + seed + tail) | reboot recovery; concurrent-mutation; hard timer | S5 |
| **S7** | Readiness/OPTIONS state machine | OPTIONS transitions vs pull/bootstrap state (timer) | S5,S6 |
| **S8** | Reverse / takeover (acting-backup mutate → reclaim via tail; callGen) | takeover-then-reclaim convergence | S4–S6 |
| **S9** | `ha-harness` (goal-1, always fake-clock) + **recording report** | data in/out; reboot; partition; convergence; readable trace | S3–S8 |
| **S10** | Goal-2 simulated failover (proxy + 2 b2bua + repl fabric + crash/reboot) | full message-flow matrix; fake net+clock; report | S9 + harness |
| **S11** | Goal-3: `K8sMembership` + `RealReplicationNetwork` + `deploy/` + kind chaos | real chaos robustness | all |

---

## Test architecture (three goals)

- **Goal 1 — pure HA** (`ha-harness`, S9): N replication-subsystem nodes (Decision 7),
  always fake-clock. Scenarios: write-converges; crash→reboot re-hydrates; partition→heal;
  takeover→reclaim; dead-peer auto-clean; watermark survives disappear/reappear;
  slow/crashing bootstrap holds no lock; buffer-full → drop subscriber → reconnect.
  **Property test:** eventual convergence — after quiescence every reachable node's view
  of a call equals the latest `callGen`.
- **Goal 2 — simulated failover** (`b2bua-harness`, S10): `SimulatedSignalingNetwork`
  (alice/bob/proxy/b2buas) + `SimulatedReplicationNetwork`, fake clock. Canonical:
  INVITE→b2bua1; **crash b2bua1**; proxy routes in-dialog to b2bua2; BYE on b2bua2;
  **reboot b2bua1**; re-hydrate + Ready; next message lands back on b2bua1 with correct
  state. Plus the matrix (crash mid-INVITE, crash during re-hydration, partition during
  failover, double-fault).
- **Goal 3 — real chaos** (S11): kind, real TCP transport, real k8s topology; pod
  kill/restart + netem partitions; assert call survival + convergence.
- **Reporting (all tiers):** `RecordingReplicationNetwork` captures every frame; the
  scenario-harness renderers project a sequence diagram of the **replication exchange**
  (Data/Noop, pull streams, bootstrap, crash/reboot/partition markers), beside the SIP
  exchange in goal-2.

---

## Deferred / confirm-at-implementation
- **Incarnation-gen real source** (boot wall-clock vs k8s pod start epoch) — finalize at S11.
- **callGen bump points** — confirm in S4/S8 (LWW already in the model).
- **Replication addressing** — fixed offset from SIP port vs separate `B2BUA_PEERS` grammar — S1/S3.
- **Backup fan-out** — single backup (HRW 2nd-best) now; per-peer changelog keeps N-backup a later extension, not a rewrite.

## Documentation deliverables
- **`CONTEXT.md`**: add the HA glossary — forward/reverse replication, re-hydration,
  backup re-subscription, incarnation-gen vs callGen, watermark, current flag, element
  (= call replica), readiness states.
- **New ADR `docs/adr/0011-ha-replication-peer-to-peer.md`**: the Redis→P2P shift and
  its hard-to-reverse trade-offs — (a) message-granular transport seam + fake-clock-vs-
  real-TCP rationale, (b) compacted ref changelog + read-from-store `Arc<[u8]>` bodies,
  (c) readiness-gated reclaim + hard timer (vs sticky-failover / hard-fencing).

---

## Verification
- **Per-slice:** each slice green via `cargo test -p <crate>` (after `source ~/.cargo/env`).
- **Vertical-skeleton gate:** a 2-node `ha-harness` test proves `put on A → appears on B`
  under `tokio::time` advance.
- **Goal-1 acceptance:** `cargo test -p ha-harness` — convergence + reboot + partition +
  takeover/reclaim + auto-clean, deterministic under fake clock; a sample recording
  report renders the replication exchange.
- **Goal-2 acceptance:** `cargo test -p b2bua-harness` — crash→failover→reboot→reclaim
  passes with fake net + fake clock; report shows SIP + replication together.
- **Goal-3 acceptance:** kind chaos suite (S11) — call survival + convergence under pod
  kills + partitions.
- **Cross-reference** against JS reference traces under
  `../sipjsserver/test-results/fake-clock/` (CLAUDE.md hint) where applicable.
