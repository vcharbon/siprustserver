# 0007 — Transaction layer Rust shape: actor + single DelayQueue

**Status:** accepted (2026-05-31)

**Source:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`,
`src/sip/TransactionLayer.ts`.

## Context

The transaction layer (RFC 3261 §17 client/server FSMs + retransmission timers)
is the next migration slice. The source is one Effect fiber draining a single
inbound stream over a **lock-free** `MutableHashMap`, with **two timer fibers
per client transaction** (retransmit loop + Timer B/F) and a cleanup fiber per
completed server transaction (Timer H/J). JS is single-threaded, so the map
needs no synchronisation and "thousands of parked fibers" is cheap.

Porting to multi-threaded tokio forces three choices the user flagged as
scalability-bearing. The user picked the scalable option for each.

## Decision X1 — timers: one `DelayQueue` driver, not a task per timer

A literal port spawns ~2 tokio tasks per client txn + 1 per completed server
txn. At 50K concurrent calls that is ~100–150K timer tasks — viable but heavy
on scheduler bookkeeping and memory.

**Chosen:** a single [`tokio_util::time::DelayQueue`] holds every pending SIP
timer, keyed by branch. One driver pops due timers. Memory is flat in the
number of *pending* timers, not tasks, and there is a single wakeup path.

This does **not** contradict the sip-clock ADR's "don't re-implement a worse
timer wheel" caveat: `DelayQueue` *is* tokio's timer wheel (it rides
`tokio::time`), not a hand-rolled one. `tokio::time::pause`/`advance` therefore
drives it in tests exactly like every other behavioural timer — recovering the
source's `TestClock`-advance test ergonomics for free.

Retransmit progression (`interval`/`elapsed`, INVITE-doubles vs.
non-INVITE-caps-at-T2) is carried on the transaction and re-inserted on each
fire, reproducing the source loop's send cadence exactly (sends at +500 / +1500
/ +3500 / … ms).

## Decision X2 — the txn map: an actor owns it, no shared lock

The source map is touched lock-free only because JS is single-threaded. On
tokio the ingest path, the send API, and every firing timer would race it.

**Chosen:** a single **owner task** ("the actor") owns the `HashMap` and the
`DelayQueue` and is the **only writer**. It `select!`s over (1) the external
send API (commands over an mpsc; callers await a oneshot reply), (2) inbound
packets it `recv`s and parses **inline** (as the source's single fiber did),
(3) the next timer expiry, (4) the safety-net sweep. No `Mutex`, no `DashMap`.

Rejected `Arc<Mutex<HashMap>>` (a global lock is a contention point under load
and timer tasks would block the recv path) and sharded/`DashMap` (ordering
subtleties; weakest fit for the single-writer/FIFO seam the HA plan preserves).

The actor is the Rust expression of the source's "single fiber over the map".
The metrics surface is backed by shared atomics the owner updates **before** it
replies to a command, so a synchronous read right after an `await` reflects the
mutation (e.g. `active_transactions()` is `== map.len()` immediately after
`send_request().await`).

## Decision X3 — scope: TransactionLayer only; dispatch deferred

The MIGRATION_STATUS "Transaction / dispatch" row groups `TransactionLayer` +
`SipRouter` + `PerCallDispatcher`. The latter two implement the **per-call
FIFO** (source ADR-0005) and depend on the call layer + rule engine (both
unported).

**Chosen (confirmed with the user):** port `TransactionLayer` only, into its own
crate `sip-txn`. Rationale the user gave: **the transaction layer is shared by
the proxy and the B2BUA, whereas the per-call FIFO is a B2BUA-only concern.**
Coupling the FSMs to the dispatcher would wrongly drag a B2BUA concept into the
proxy's path. The single-writer property the dispatcher provides downstream is,
*at this layer*, already provided structurally by the actor (X2). `SipRouter` /
`PerCallDispatcher` land with the call/rules slices.

## Deferred (with justification)

- **Tier-3 overload admission gate** (`overload.shouldAdmit` + stateless 503 on
  new INVITEs) — depends on `OverloadController` / `AppConfig` (b2bua slice).
  The transport-level **Tier-1** pre-ingress brake is already in sip-net; this
  Tier-3 gate, and the `buildStatelessReject503Buffer` / `isEmergencyRequest`
  helpers it needs, defer with their dependencies. This layer admits
  unconditionally for now.
- **`transactionBreakdown` gauge** (per-(method,role,state) walk of the map) —
  observability not asserted by the ported tests; the `method` field is carried
  so it is a pure addition later. `messagesProcessed` / inbound+outbound byte
  counters **are** ported.
- **Tracing / OTel span re-parenting** (the `forkDetachedInScope` /
  `DETACHED_PARENT` machinery, `ForkSiteTracker`) — an Effect-tracer artefact
  with no tokio analogue; send errors are currently swallowed silently (no
  tracing dep yet).
- **`send` legacy combined wrapper** — the source kept it only for incremental
  call-site migration; the Rust API ships `send_request`/`send_response`/
  `send_raw` directly.

## RNG seam

`newTag` / `newBranch` (the source's fiber-local Effect `Random`, deferred from
the message slice) land here as `IdGen` — a small **injectable value** (not a
trait), mirroring the clock seam: `IdGen::seeded(seed)` for deterministic tests,
`IdGen::from_entropy()` in production.

## No property / parity tests

Unlike the network layer, the source `TransactionLayer` has no `propertyTest`
and no `parity`/compliance-matrix decorator (those wrap `SignalingNetwork`).
The ritual's "property + Layer comparison" step is therefore N/A here; the four
behavioural suites are the whole test surface.

## References

- [`crates/sip-txn/src/layer.rs`](../../crates/sip-txn/src/layer.rs) — the actor
- Source: `src/sip/TransactionLayer.ts`; per-call FIFO: source ADR-0005
- [ADR-0005 — network layer Rust shape](./0005-network-layer-rust-shape.md);
  sip-clock decisions: [MIGRATION_PLAN_B2B §2](../MIGRATION_PLAN_B2B.md)
