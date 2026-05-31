# Migration plan — fully-working B2BUA (HA-ready)

**Status:** proposal / for review · **Date:** 2026-05-30
**Goal:** reach a **fully working B2BUA** with the basic rule set (to be catalogued
in [`docs/HIGH_LEVEL.md`](./HIGH_LEVEL.md), which does not yet exist) and
**internal (in-process) call-context storage**, structured so that **HA —
call-context replication across B2B workers — is a later, additive slice rather
than a rewrite**. Then run a basic perf comparison against the TypeScript server
using the existing kind-based endurance framework.

This plan is grounded in a read of the source layout under
`portsource/sipjsserver/src/` (transport, call, rules) and the consolidated
decision docs ([MIGRATION_STRATEGY.md](./MIGRATION_STRATEGY.md),
[CONTEXT.md](../CONTEXT.md), [MIGRATION_STATUS.md](../MIGRATION_STATUS.md)).
**Exact TS signatures must still be re-confirmed at the start of each slice** and
the per-layer source SHA pinned in MIGRATION_STATUS (ritual step 1).

---

## 0. Key insight: the HA seam already exists in the source — port it, don't invent it

HA means replicating call-context state across workers. Replication is tractable
only if state mutation is **deterministic, ordered, observable, and
serializable**. The encouraging finding from reading the source: **the TS server
already enforces exactly these properties**, and they are the invariants we must
preserve through the port. We are not inventing an HA architecture — we are
porting one and keeping its seams intact while swapping its Redis backing for an
in-process store.

| HA-enabling property | Where it already lives in the source | Rust port obligation |
|---|---|---|
| **Single writer per call** | per-call `Semaphore` in `CallState.ts`; per-call FIFO via `TransactionLayer → SipRouter → PerCallDispatcher` (source ADR-0005) | Preserve: actor/owned-task or per-call mutex; never two tasks racing one call |
| **Mutation as explicit action + apply step** | rules are **pure** — `handle(ctx) -> { actions, state }`; `ActionExecutor` applies the `RuleAction[]` | Port the purity boundary intact: rules decide, executor mutates |
| **Per-call deterministic state** | `ruleState` (per-rule opaque blobs), explicit `Call`/`Leg`/`Dialog` model, `nowMs` injected via Clock | Keep all state in the `Call` aggregate; inject time (§2) |
| **Serializable state** | `CallCodec.ts` / `call/codec/` (msgpack-ish) | Port the codec; it is what replication ships |
| **Storage seam** | `PartitionedRelayStorage` over a `KvBackend` (Redis prod / in-memory test) behind `CallState` | Port the seam; in-memory `KvBackend` now, networked impl + **pull-based** replication later (see [HIGH_LEVEL.md](./HIGH_LEVEL.md) §5) |
| **HA topology already modelled** | `Call._topology { pri, bak, gen }`, `workerIndex`, `src/replication/`, `docs/replication/` | Carry the fields through the model now even if unused; do not strip them |

**Therefore the plan's HA stance is conservative: faithfully port the source's
purity + single-writer + codec + storage-trait seams; drop only the Redis backing
(per [MIGRATION_STRATEGY.md] "Redis sidecar for call cache → dropped") in favour
of an in-memory buffer + tokio cleanup. The future HA slice becomes a networked
`KvBackend` impl + a **pull-based** replication puller/server — no `call`/`rules`
call-site churn (see [HIGH_LEVEL.md](./HIGH_LEVEL.md) §5 for the exact seam).**
The one thing to guard against is *losing* a seam under porting expediency (e.g.
mutating `Call` inside a rule instead of returning an action) — every such
shortcut converts the future HA slice from "add a decorator" into "rewrite."

---

## 1. Layer inventory & sequencing

The requested pieces, mapped to source modules and the crate-per-layer plan
([source ADR-0005] also forces a transaction/dispatch layer between net and call,
which a "fully working B2BUA" requires):

| # | Piece (user ask) | Crate | Source modules | Slice |
|---|---|---|---|---|
| a | **Clock / test time** | `sip-time` (tiny) | Effect `Clock`/`TestClock` → `tokio::time` | 2 |
| b | **Network / UDP** | `sip-net` | `sip/{UdpTransport,BufferedUdpEndpoint,SignalingNetwork(.real/.realTracing/.simulated),ConnectivityGate}.ts` | 2 |
| b′| **Recording** | `sip-net` | `SignalingNetwork.realTracing.ts` (`drainTrace`/`NetworkTraceEntry`); pcap = source `docs/TODO-pcap-replay.md` | 2 |
| c | **Test framework DSL + 4 wrappers** | `sip-testkit` (dev) | `test-harness/framework/{dsl,interpreter,effectLayerTest,recordingHelpers}.ts`; source ADR-0013 | 2–3 |
| d | **Transaction / dispatch** | `sip-txn` | `sip/{TransactionLayer,SipRouter,PerCallDispatcher,Dialog}.ts`; source ADR-0005 | 3 |
| e | **Extensible call-context model** | `call` | `call/{CallModel,CallState,CallCodec,TimerService}.ts`, `call/codec/`; source ADR-0014, ADR-0016 | 4 |
| f | **Rule engine** | `rules` | `b2bua/rules/framework/*` (`RuleDefinition,Matcher,RuleRegistry,RuleExecutor,ActionExecutor,InvariantEnforcer`) | 5 |
| g | **The basic B2B rules** | `rules` + `docs/HIGH_LEVEL.md` | `b2bua/rules/defaults/*` | 5 |
| h | **Compose + perf** | `b2bua` bin + benches/endurance | `b2bua/B2buaCore.ts`, composition root | 6 |

**Forced dependency order** (Rust crates can't be cyclic — source has type cycles
`SipMessage ↔ Leg/Call ↔ RuleContext`, so a shared `sip-foundation` types crate
will likely be extracted at the `call` layer, as [source ADR-0002] anticipates):
`sip-message` → `sip-time` → `sip-net` → `sip-txn` → `call` → `rules` → `b2bua`.

**Out of scope for "basic B2B"** (note the hooks, don't build): `CallLimiter`
(separate `limiter` layer; source ADR-0004), `OverloadController`/two-tier drain
(source ADR-0008; but the transport **preIngress tier-1 brake** is in `sip-net`),
the HTTP `CallDecisionEngine`/`callControl` (stub with a static route),
`SERVICE_LAYER` callflow rules and REFER transfer (source `TransferRules` +
callflow services).

**Recommended slices** (each = one dated MIGRATION_STATUS entry, each follows the
ritual: interface → prod impl → test impl → property/parity tests → port all
tests + justify gaps):

- **Slice 2 — Network foundation:** clock seam; `UdpTransport`,
  `BufferedUdpEndpoint`, `SignalingNetwork` trait + real/simulated impls;
  recording (`realTracing` decorator); the 4 contract wrappers; scenario-DSL v1.
- **Slice 3 — Transaction/dispatch:** transaction FSMs, `SipRouter`,
  `PerCallDispatcher` (the per-call FIFO — first hard enforcement of single-writer).
- **Slice 4 — Call context:** the `Call`/`Leg`/`Dialog` model, `ext` slots,
  `CallCodec`, `CallStore` trait + in-process impl + tokio cleanup; carry the
  `_topology` HA fields.
- **Slice 5 — Rule engine + basic rules:** the declarative engine + `ActionExecutor`
  + `InvariantEnforcer`, then the `defaults/` CORE-layer rules; author
  `docs/HIGH_LEVEL.md`.
- **Slice 6 — Compose + perf:** the `b2bua` binary, criterion micro-benches, point
  the kind endurance framework at the Rust server, compare to TS.

---

## 2. Clock / test-time layer

**Source:** Effect `Clock` (prod) / `TestClock` (tests). Tests advance virtual
time via `TestClock.adjust` **in 100 ms chunks** so in-flight delivery fibers
observe intermediate values (`tests/harness/runner.ts`). Clock is consumed by
`CallState` timers, `CallLimiter` windows, `BufferedUdpEndpoint` idle-sweep,
`OverloadController`.

**Direct port?** *No 1:1 port — a re-expression.* Effect `Clock` → `tokio::time`
([MIGRATION_STRATEGY.md] construct map). Confidence **high**. The virtual-time
behaviour ports via `tokio::time::pause`/`advance`.

**Decision — DECIDED (2026-05-31): split the two jobs Effect fused.**
Implemented in `crates/sip-clock`.

Effect's `Clock`/`TestClock` did two jobs through one runtime-injected seam:
answer *"what time is it?"* (the `nowMs` value) **and** *"wake me later"*
(scheduling). Its universal power came from runtime DI, not from the type's
existence — Rust gives no free universal interception, so a hand-threaded trait
would only be as universal as the discipline threading it. The two jobs split
cleanly here, and **only one of them needs a seam:**

- **Behaviour — timers, deadlines, `CallState` timers, `CallLimiter` windows,
  idle-sweeps, `OverloadController` — runs on monotonic time via `tokio::time`
  directly** (`sleep`, `sleep_until`, `interval`, `timeout`, `Instant`).
  `tokio::time::pause`/`advance` is the universal test lever, ambient within the
  runtime exactly as `TestClock` was ambient within the Effect runtime — so it
  recovers Effect's transparent-test-clock power for *all* scheduling, for free,
  with **no trait** (wrapping it would re-implement a worse tokio). Confirmed
  with the user: all load-bearing behaviour is expressible in monotonic time;
  wall time is needed only for *timestamps*.
- **Wall-clock `now_ms()` is timestamps only** (log lines, call records) — never
  a behavioural input. It is the one thing `tokio::time::pause` cannot bend
  (pause moves the monotonic clock, not `SystemTime`), so it gets the injectable
  seam: a tiny `Clock` value (not a trait) — `Clock::system()` in prod,
  `Clock::test_at(anchor_ms)` in tests.

**The construction — monotonic-anchored `now_ms`.** `now_ms = anchor_wall_ms +
elapsed_since_anchor`, where the elapsed rides `tokio::time::Instant`. Two
properties fall out for free: (1) the prod timestamp never jumps backward (rides
the monotonic clock); (2) in tests the elapsed rides the *same* clock
`tokio::time` controls, so a single `tokio::time::advance(d)` moves the
behavioural timers **and** `now_ms()` together — one lever, no separate
`TestClock` counter.

Rejected the earlier draft's option B (a `Clock` trait with `now`/`sleep_until`/
`timer`): the `sleep_until`/`timer` methods duplicate what tokio already gives
ambiently and tempt a worse re-implementation of its timer wheel; only the
`now()` value is load-bearing.

**Caveats (recorded in `sip-clock` rustdoc).**
- `now_ms` drifts from true wall clock over long uptime (rides monotonic, won't
  track NTP). Fine for logs/records; read `SystemTime` directly at the rare site
  that must reconcile with an external wall clock (SIP `Date` header, billing).
- The HA "replicas compute identical deadlines" invariant changes shape: since
  behaviour is monotonic-local and `Instant`s are *not* portable across
  processes/restarts/replicas, replicated events must carry a *remaining
  duration* or an *absolute wall deadline* and the standby rebuilds its monotonic
  timer locally — never ship a raw `Instant`. Revisit at the failover slice.

**Tests (done).** `now_ms` advances in lockstep with `tokio::time::advance`;
monotonic non-decreasing; clones share one timeline; property test "`now_ms ==
anchor + advanced`" (replaces the old "deadline = f(now, timeout)" property —
deadlines are now monotonic). The source's 100 ms-chunk advance helper is
mirrored as `sip_clock::testkit::advance_in_chunks` (behind the `testkit`
feature) so ported timer scenarios observe intermediate values identically.

---

## 3. Network layer (`sip-net`) + recording

**Source signatures (to port).**

- `UdpTransport`: `send(buf, port, addr)`, `messages: Stream<UdpPacket>`,
  `metrics`, `localAddress`. `UdpPacket { raw, rinfo, arrivalMs, parsed? }`.
- `UdpEndpoint`: `send -> Result<(), SendError>`, `messages`, `queueDepth()`,
  `queueMax`, `counters { enqueued, tailDropped, preIngressDropped,
  preIngressReplies }`.
- `BindUdpOpts { ip, port, queueMax, preIngress?, reusePort?, roles?, raw? }`.
- **preIngress tier-1 brake:** `PreIngressHook(raw, rinfo, depth) ->
  accept | drop | reply(buf)` — stateless 503-with-jittered-Retry-After on new
  non-emergency INVITEs above `udpQueueTier1ThresholdPct`. (Overload *controller*
  is out of scope; this transport-level gate is in.)
- `BufferedUdpEndpoint`: non-blocking `send` (per-peer drainer fiber + bounded
  per-peer queue + idle-peer reclamation sweep), `peerCount()`, buffered counters.
- `SignalingNetwork`: `bindUdp(opts) -> UdpEndpoint`, `drainUndeliverable()`,
  `drainTrace()`, `transitDelayMs`, `inFlight()/bumpInFlight()`, `queueDepths()`,
  `awaitInFlight(timeoutMs)`. Impls: `.real`, `.realTracing`, `.simulated`
  (in-memory transit with delay/loss — the test transport).

**Direct port?** *Structure ports closely; mechanism re-expressed.* Effect service
→ `trait`; socket → `tokio::net::UdpSocket`; `Stream<UdpPacket>` → an `mpsc`
receiver. Confidence **medium-high** — the interface surface is clear; the
load-bearing detail is the backpressure model (`queueMax`/tail-drop,
per-peer buffered send).

**Decision N1 — recv concurrency (load-bearing for perf + HA).**

- **A — one socket, one recv task → dispatch to per-call owner (recommended start).**
  Matches single-writer (§0) and the source's queue model. *Cons:* one recv task
  can cap pps at extreme load — measured in slice 6, not assumed.
- **B — `SO_REUSEPORT` sharded sockets (source already has `reusePort` in
  `BindUdpOpts`), N recv tasks, call→shard affinity by Call-ID.** *Pros:* near-linear
  scaling; shard = partition aligns with HA partitioning. *Cons:* affinity must be
  stable. **Design the partition key now; switch A→B as a measured perf upgrade.**
- **C — work-stealing over a shared socket.** Reintroduces cross-task contention on
  call state; fights §0. Avoid.

**Recommendation: A now, partition key plumbed so B is drop-in.**

**Decision N2 — recording mechanism.** *Direct port — the source already does the
right thing.* `SignalingNetwork.realTracing` is a **decorator** that tees every
accepted recv + successful send into an in-memory buffer of `NetworkTraceEntry
{ src, dst, raw, sentMs, deliveredMs, delivered, seq }`, drained via
`drainTrace()`. Port it as `RecordingNetwork<T: SignalingNetwork>` — same trait,
toggleable, and it doubles as the `scopedAudit` test wrapper's capture and a
source for replay scenarios (§4).

**Decision N3 — recording sink under load (load-bearing for perf).** The source
trace buffer is *unbounded* and *test-only*. For a recorder usable under perf
load, tee to a **bounded `mpsc` → dedicated writer task; on overflow drop + count
(gap marker), never block** — recording must not apply backpressure to signaling
(blocking would distort the latency we measure). Keep the in-memory `drainTrace`
form for tests. **pcap** export (source `docs/TODO-pcap-replay.md`, an open TODO
there) is an optional `xtask`, not on the hot path; the internal length-prefixed
`(ts,dir,peer,bytes)` format doubles as replay fixtures.

**Tests.** Port transport round-trip + the `.simulated` transit/loss tests; the
**4 contract wrappers land here** (§4); recording decorator test (captured bytes
== sent bytes); loopback integration.

---

## 4. Test framework DSL + the 4 contract wrappers (`sip-testkit`, dev)

Two distinct things:

**(i) The 4 contract wrappers — source ADR-0013.** Canonical composition
`propertyTest(paranoidInputs(scopedAudit(impl)))`; **`parity` is built first and
passed in as the `impl`** (not part of the helper). In the source they wrap
`SignalingNetwork`.
- *Direct port?* **Re-expressed, not ported.** Effect `Layer` combinators →
  **decorator structs implementing the same `SignalingNetwork` trait**
  ([MIGRATION_STRATEGY.md] construct map). Confidence **high** on the pattern.
  - `scopedAudit` → the recording decorator (§3 N2) asserting an expected trace
    within a scope (acquire/release symmetry).
  - `paranoidInputs` → feed malformed inputs (reuse the parser torture corpus)
    through the live network path.
  - `propertyTest` → proptest generation over messages; record every call.
  - `parity` → run two impls, assert identical observable behaviour (the
    network-layer analogue of the message layer's compliance matrix; the build-first
    `impl`).
  - Port `recordingHelpers` (`recordSync`/`recordEffectCall`/`recordScopedAcquire`/
    `recordStreamLifecycle`) as the small set of recording combinators these need.

**(ii) The scenario DSL — the user said it "could be simplified."** Source:
`ComposableScenario` (named agents + ordered `Step`s: `send`/`expect`/`wait`/
`build`/`marker`, `or(...)` branching, `andThen` composition, `tier`,
`runOn(suts)`), interpreted by `interpreter.ts`, driven by `runDriveOnly` which
returns recordings + rule-engine result + scenario result.

- **A — faithful port.** *Pro:* fixtures port 1:1. *Con:* the DSL is Effect/Layer-
  shaped and large; porting its combinators fights Rust's grain.
- **B (recommended) — a slim Rust DSL** over the `.simulated` `SignalingNetwork`
  + `TestClock`: scenarios as **data** (`Vec<Step>` of `Inject` / `Advance` /
  `ExpectOut` / `ExpectState`), a `runDriveOnly`-equivalent that returns
  outbound trace + final `Call` state. Keep the *useful* source ideas (named
  agents, `or`-branching for racing responses, the 100 ms-chunk clock advance)
  and drop the SUT/tier machinery (kind endurance covers scale separately).
  *Pro:* small, deterministic; the single reusable asset that validates net →
  txn → call → rules **and** later proves replication determinism (run scenario,
  snapshot the action/event stream, replay on a second `CallStore`, assert
  identical state). *Con:* scenario fixtures re-authored (few for "basic B2B").
- **C — table-driven only.** Too rigid for multi-step dialog flows.

**Recommendation: B.** A data-described, clock-driven runner that emits the same
action stream the call layer consumes is worth more than a literal port.

---

## 5. Transaction / dispatch layer (`sip-txn`)

**Source:** `TransactionLayer.ts` (RFC 3261 INVITE/non-INVITE client+server FSMs),
`SipRouter.ts` (message → call/dialog key), `PerCallDispatcher.ts` (per-call FIFO
ordering — **source ADR-0005**), `Dialog.ts`. This is the layer the user's bullet
list omits but a working B2BUA needs, and it's where single-writer (§0) is
enforced.

**Direct port?** FSMs port almost literally (spec-pinned) — confidence
**medium-high**; router/dispatcher wiring re-expressed — confidence **medium**
(depends on the exact dialog-matching keys). Refined views from `sip-message`
(`InDialogRequest`, `InviteRequest`, `SipResponseTagged`) are built here at the
boundary.

**Decision X1 — dispatch model (first enforces §0).**

- **A — actor/owned-inbox per call (recommended).** `PerCallDispatcher` routes each
  inbound message to a per-call task (or per-call `mpsc` inbox) that is the single
  writer for the call's transactions + context. *Pros:* lock-free call state,
  natural FIFO (= source ADR-0005), the per-call message stream *is* the
  replication source. *Cons:* inbox bookkeeping + a reaper for ended calls
  (ties to §2 clock + §6 cleanup). This is the Rust expression of the source's
  per-call **semaphore**.
- **B — sharded `DashMap<key, CallState>` + per-entry lock.** *Cons:* contention,
  ordering subtleties, muddier replication story. Weaker fit for §0.

**Recommendation: A.** Transaction timers use the §2 clock seam.

**Tests.** Port the transaction FSM tests (highest-value — RFC-pinned); drive
INVITE/ACK/BYE + retransmission/timeout through the §4 DSL with `TestClock`.

---

## 6. Extensible call-context model (`call`)

**Source model (`CallModel.ts`).** `Call { callRef ("{ordinal}|{aLegCallId}|
{aLegFromTag}"), aLeg, bLegs[], activePeer (1↔1 bridge | null), aLegInvite
snapshot, tagMap, state (active|terminating|terminated), createdAt, limiterEntries,
timers, cdrEvents, emergency?, features?, activeRules?, ruleState? (per-rule opaque
blobs), ext? (per-service keyed slots — source ADR-0016), _topology? {pri,bak,gen}
(HA), workerIndex, … }`. `Leg { legId, callId, fromTag, source, state
(trying|early|confirmed|terminated), disposition, dialogs[], byeDisposition?,
pendingInviteTxn?, ext?, kind?, adopted? (source ADR-0014) }`. `Dialog { sip {…RFC
§12 routeSet/CSeq/tags/remoteTarget}, ext {remoteCSeq, inboundPendingRequests,
ackBranch, cachedSdp} }`.

**Storage (`CallState.ts`).** In-memory `MutableHashMap<callRef,Call>` + SIP-key
index (`leg:{callId}|{tag}` → callRef) + **per-call `Semaphore`** (serial
processing) + cache backing `PartitionedRelayStorage` (Redis prod / in-memory
fake test). Flush lifecycle: new → confirmed (200 OK) → idle flush+evict →
terminated cleanup (delayed for retransmissions). `CallCodec.ts` / `call/codec/`
serializes `Call`.

**Direct port?** *Model + codec port closely; storage is re-architected.* Per
[MIGRATION_STRATEGY.md], the **Redis call-cache sidecar is dropped** → in-memory
buffer + tokio cleanup. So the `Call`/`Leg`/`Dialog` shapes and `CallCodec` are a
direct port; the storage backing is replaced. Confidence **medium-high** on the
model; **medium** on lifecycle timing details. A shared `sip-foundation` types
crate is likely extracted here (the `SipMessage ↔ Call ↔ RuleContext` cycle —
source ADR-0002).

**Decision C1 — extensibility mechanism (the user's "extensible") — port source
ADR-0016.** The source already chose per-service **typed `ext` slots**
(`ext?: Record<string, unknown>` on `Call` and `Leg`, keyed by service id; rules
read them through typed views). Port as a **`TypeMap`/`AnyMap` keyed by type**
(matching the project's `TypedHeader` open-extension idiom in CONTEXT.md):
- *vs generics `CallContext<E>`:* rejected — viral type parameter through
  `CallStore`/dispatcher/rules; can't hold multiple independent extensions.
- *vs `Box<dyn Extension>`:* rejected — weaker typing, downcasting.

  Typed core + `ext: TypeMap` keeps the hot path monomorphic and matches the
  source's per-service model. The `ruleState` per-rule blobs port as typed,
  codec-serialized state owned by the `Call` aggregate.

**Decision C2 — mutation model (the HA decision) — preserve the source's purity.**
The source *already* separates decision from mutation: rules return `RuleAction[]`,
`ActionExecutor` applies them to `Call`. **Keep this boundary intact**: external
inputs (messages, timers) → the per-call owner runs rules → gets actions →
`ActionExecutor` applies → state changes → `flush` snapshots the post-apply `Call`.
The **body snapshot** (callGen-versioned) is what replication ships — see
[HIGH_LEVEL.md](./HIGH_LEVEL.md) §5.
- *Anti-pattern to forbid:* mutating `Call` directly inside a rule's `handle`
  (the source forbids it; rules are pure and return state). Any such shortcut in
  the port is HA debt — record it explicitly.
- *Replication payload (settled by the source):* the source ships the **body
  snapshot**, pulled by the backup and applied under a `callGen` content gate (not
  an event/action log). The Rust port keeps that; an action/event log stays a
  possible-but-unneeded alternative. The codec (`CallBodyCodec`) is what makes the
  snapshot shippable — finalize it at the `call` slice.

**Decision C3 — storage seam + lifecycle (in-process now, HA-ready).** Port the
source's **three-trait stack** (detailed in [HIGH_LEVEL.md](./HIGH_LEVEL.md) §5):
`CallStore` (hot map + per-call single-writer) → `PartitionedRelayStorage`
(role-partitioned `pri:`/`bak:` + flat `idx:`) → **`KvBackend`** (the actual
swap point: in-memory now, networked later; its atomic `channel_write_update`
populates the replication log on every `flush`). **Cleanup via the §2 clock**
(tokio timer reaps idle/terminated calls — the Rust replacement for Redis TTL).
**Carry `_topology {pri,bak,gen}` + `workerIndex` in the model even though unused
now** — stripping them is the kind of seam-loss that makes HA a rewrite. HA later =
a networked `KvBackend` + a **pull-based** puller/repl-log server (the backup
*pulls* body snapshots from the primary and applies them under a `callGen` gate) —
no call-site changes in `call`/`rules`.

> **Cross-worker shared state note.** [MIGRATION_STRATEGY.md] already flags Redis
> (or a small Rust service) *may* back the **limiter**'s cross-worker state,
> decided at that layer. For call-context HA we follow the source: **pull-based
> body-snapshot replication** between workers (topology assigned by the front-proxy
> via the Record-Route cookie, *not* a shared DB) — keeps the hot path in-process.
> The wire format itself is open to change ([HIGH_LEVEL.md](./HIGH_LEVEL.md) §5.5);
> capture the chosen one in an ADR at the HA slice.

**Tests.** Port `call/` unit tests + `CallCodec` round-trip; property-test the
apply step (apply random valid action sequences, assert invariants: legs
consistent, `activePeer` bridge integrity, no state regressions); **determinism
test** = two stores fed the same action stream reach codec-identical state (the
free HA pre-flight, given C2).

---

## 7. Rule engine (`rules`)

**Source (`b2bua/rules/framework/`).** Declarative `RuleDefinition { id, name,
alwaysActive, stateSchema, paramsSchema, match: Match, init, handle(ctx) ->
{ actions, state }, layer (CORE=0|SERVICE=1), overrides?, composesWith? }`.
`Match` = discriminated union (`request`/`response`/`timer`/`timeout`/`cancelled`/
`internal-event`) with optional columns (method, callState, legState,
legDisposition, direction, status/statusClass, timerType, …) + an optional pure
`filter`. `RuleContext { call, callRef, event, sourceLeg, sourceDialog, direction,
config, callControl, limiter, nowMs }`. `RuleAction` = ~40-variant union (relay-to-
peer/leg, respond, ack-leg, send-provisional, create-leg, destroy-leg, merge,
split, confirm-dialog, update-leg-state, schedule-timer, cancel-timer,
begin-termination, terminate-leg, add-cdr-event, set-rule-state, …). `RuleRegistry`
+ `Matcher` selection: **kind gate → column match → filter → layer precedence
(SERVICE>CORE) → registration order (first-match-wins)**; startup
`validateMatchSchemas` flags unreachable rules. `RuleExecutor` runs the selected
rule; `ActionExecutor` applies actions; `InvariantEnforcer`/`ByeDispositionInvariant`
assert post-conditions.

**Direct port?** *Largely a direct architectural port — and notably the source is
already pure (rules decide, executor mutates), which is exactly the HA-friendly
shape.* The decision is purely *how to represent dispatch in Rust*. Confidence
**medium** — the framework is sizeable; the `Match`/`RuleAction` unions are the
bulk of the work but are mechanical.

**Decision R1 — rule + match representation (load-bearing for perf).**

- **A — `trait Rule { fn match_descriptor() -> Match; fn handle(&self, ctx) ->
    RuleHandleResult }` + `Vec<Box<dyn Rule>>`, `Match` as a data struct
    interpreted by a ported `Matcher` (recommended).** *Pros:* closest to the
    source; preserves `validateMatchSchemas` (data-introspectable matches),
    `overrides`, layering, first-match-wins; open to `SERVICE_LAYER`/extension
    rules later. *Cons:* dynamic dispatch on the matched rule (negligible — one
    rule runs per event, not a hot inner loop).
- **B — `enum Rule` + hand-written match.** *Pros:* static dispatch. *Cons:*
  closed set; **loses the data-driven `Matcher` and unreachable-rule validation**,
  which are core source features. Rejected.
- **C — fully data-driven rules (actions as serialized data).** The source is
  already partly there (`paramsSchema`, `activeRules` from HTTP). Overkill for
  basic B2B; keep as a later option behind the trait.

**Recommendation: A.** Port `Match` as data + an interpreting `Matcher` so
`validateMatchSchemas`, layering, and `overrides` come across; rule bodies are
trait impls. `RuleAction` → a Rust `enum`; `ActionExecutor` → a `match` over it.

**Decision R2 — purity boundary — port as-is (non-negotiable for HA).** Rules
stay pure (`handle -> { actions, state }`); only `ActionExecutor` mutates `Call`
and only the executor performs I/O (sends via `sip-net`). This is both the source
design and HA invariant §0/C2. `nowMs` enters via `RuleContext` from the §2 clock.

**Decision R3 — selection semantics — port exactly.** kind→column→filter→layer→
registration-order, first-match-wins, with the unreachable-rule startup check.
Document the order in `HIGH_LEVEL.md`; test determinism.

**Tests.** Port per-rule unit tests (decision = pure function of `RuleContext` →
ideal for table tests + proptest); engine tests (selection precedence, unreachable
detection, `ActionExecutor` effects, `InvariantEnforcer`); full flows via §4 DSL.

---

## 8. The basic B2B rules + `docs/HIGH_LEVEL.md`

**`docs/HIGH_LEVEL.md` does not exist — authoring it is part of this slice** and
is the canonical, ordered catalogue. The source `b2bua/rules/defaults/` gives the
exact CORE-layer set to port for "basic B2B" (transfer/`SERVICE_LAYER`/callflow
deferred):

| Source group | Rules (port for basic B2B) |
|---|---|
| **Lifecycle** (`LifecycleRules`) | `handleCancelRule`, `resolveCancelResponseRule`, `handleTimeoutRule` |
| **Dialog** (`DialogRules`) | `relayProvisionalRule`, `confirmDialogRule`, `relayNonInvite200Rule`, `absorbBye200Rule`/`absorbNotify200Rule`/`absorbOptions200Rule` |
| **Relay** (`RelayRules`) | `relayByeRule`, `relayAckRule`, `relayReinviteRule`, `relayOptionsRule`, `relayUpdateRule`, `relayInfoRule`, `relayPrackRule`, `relayMessageRule` |
| **Terminating** (`TerminatingRules`) | `resolveByeResponseRule`, `resolveCrossByeRule`, `terminatingSafetyTimeoutRule` |
| **Corner cases** (`CornerCaseRules`) | `retransmit200Rule`, `cancel200CrossingRule`, `reinviteGlareRule`, `relayReinviteResponseRule` |
| **Timer** (`TimerRules`) | `maxDurationRule`, `keepaliveRule`, `keepaliveTimeoutRule` |
| **Failure** (`FailureRules`) | `routeFailureRule`, `noAnswerFailoverRule`, `absorbStaleFailureRule` |

Plus the **B-leg creation / bridge** path (the `create-leg` + `merge`/`activePeer`
actions; source `InitialInviteHandler.ts` + `TargetAdmission.ts`) — the heart of
the B2BUA. **`callControl`/`CallDecisionEngine` is stubbed** with a static target
for basic B2B (no HTTP decision service yet).

**Direct port?** Per rule, on read. **SDP-bridging rules depend on `sip-message`
SDP utils (`sdp.rs`, currently ⬜ in MIGRATION_STATUS)** — sequence the SDP port
first, or stub SDP as passthrough initially. The `_shared/sdpDiff.ts` custom helper
is a prerequisite for re-INVITE/transfer media handling (defer with transfer).

**Process per rule:** (1) write the `HIGH_LEVEL.md` entry (intent, match, actions,
ordering/layer), (2) port as a pure `Rule`, (3) port its tests, (4) add a DSL
scenario. List un-ported rules with justification (ritual).

---

## 9. Perf comparison (the end goal)

- **Micro-benches (criterion):** extend the existing parser bench (INVITE ~5.3µs,
  200 OK ~4.9µs today) with: transaction FSM step, `ActionExecutor` apply,
  full rule-engine selection+handle, `CallCodec` round-trip. Cheap regression guards.
- **Endurance / load:** **keep the kind-based JS endurance framework as-is** (user
  confirmed). The Rust server speaks the same SIP wire protocol, so the framework
  points at the Rust UDP port unchanged — ideally only host/port differs → true
  apples-to-apples A/B vs the TS baseline. (Source `docs/k8s-endurance.md` /
  `K8S_test.md` document the existing harness.)
- **Metrics (same harness, both servers):** sustained calls/sec; setup latency
  p50/p95/p99; **memory per active call** (the dropped-Redis in-process model
  should win clearly); CPU at fixed load; GC pauses (TS only — expected Rust win).
  The §3 recorder captures traces for correctness spot-checks under load.
- **Method:** fix scenario + offered load, identical kind config/hardware, ramp to
  saturation, report curves. Record numbers per release in `docs/PERF_BASELINE.md`.
- **Optional later (only if measured):** a native Rust load generator if the JS
  framework itself becomes the bottleneck.

---

## 10. Risks, prerequisites, open questions

1. **Missing inputs:** author `docs/HIGH_LEVEL.md` (§8); pin per-layer source SHAs
   in MIGRATION_STATUS (ritual step 1).
2. **SDP utils pending** in `sip-message` gate SDP-bridging rules (§8) — sequence
   first or stub passthrough.
3. **Shared types crate** (`sip-foundation`) will likely be forced at the `call`
   layer by the `SipMessage ↔ Call ↔ RuleContext` cycle (source ADR-0002) — plan
   for the extraction, don't fight it.
4. **Perf-bearing choices** N1 (single-recv → `SO_REUSEPORT` shard) and X1
   (actor-per-call) are designed to be *measured* in slice 6 and upgraded without
   call-site churn (partition key plumbed early).
5. **HA is out of scope to implement now**, but the obligation is to **preserve the
   source's existing seams** (purity, per-call FIFO, `CallCodec`, `CallStore`
   trait, `_topology` fields). **Any expedient shortcut that loses a seam
   (esp. mutating `Call` inside a rule, or dropping `_topology`/codec) must be
   recorded as HA debt — it converts the future HA slice from "add a decorator"
   into "rewrite the call layer."**
6. **Source contract-test fidelity:** the 4 wrappers (ADR-0013) and `.simulated`
   transport are how the source proves network behaviour; porting them faithfully
   (§4) is what lets us trust the perf comparison is comparing equivalent servers.

---

## 11. ADRs to write alongside

- **ADR — clock seam:** `Clock` trait over `tokio::time` (§2 B).
- **ADR — network concurrency & partitioning:** single-recv now, `SO_REUSEPORT`
  shard later, partition key = Call-ID (§3 N1); recording = `realTracing`-style
  decorator with bounded sink (§3 N2/N3).
- **ADR — call storage seam & HA strategy:** `CallStore` (hot map) over
  `PartitionedRelayStorage` over `KvBackend`; in-memory now, **pull-based**
  body-snapshot replication later (not shared DB); carry `_topology` (§6 C3,
  [HIGH_LEVEL.md](./HIGH_LEVEL.md) §5).
- **ADR — preserve rule purity & dispatch model:** data `Match` + interpreting
  `Matcher` + trait rules + pure `handle` + `ActionExecutor` (§7 R1/R2/R3).
- **ADR — extension model:** typed `ext` `TypeMap` (port of source ADR-0016) (§6 C1).

[MIGRATION_STRATEGY.md]: ./MIGRATION_STRATEGY.md
[source ADR-0002]: ../portsource/sipjsserver/docs/adr/
[source ADR-0005]: ../portsource/sipjsserver/docs/adr/0005-per-call-fifo-via-router-and-workers.md
[source ADR-0013]: ../portsource/sipjsserver/docs/adr/0013-effect-layer-test.md
[source ADR-0016]: ../portsource/sipjsserver/docs/adr/0016-per-service-typed-extensions.md
