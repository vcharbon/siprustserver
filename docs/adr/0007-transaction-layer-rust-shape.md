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
timer, keyed by role and branch. One driver pops due timers. Memory is flat in
the number of *pending* timers, not tasks, and there is a single wakeup path.

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

## Decision X4 — transaction identity: one map per role, the §17 match rules

A request matches only a server transaction and a response only a client one,
so the actor keeps one map per role. A client transaction is keyed by its
branch; a response matches it on branch and CSeq method (§17.1.3). A server
transaction is keyed by the request's top-Via branch, sent-by and method, an
ACK naming its INVITE's transaction and a CANCEL its INVITE's (§17.2.3, §9.2).
A request that comes back to the node that sent it, with no other hop's Via
on top, therefore opens a server transaction beside its sender's client one.

- **Sent-by** compares the host case-insensitively and the port as written:
  an omitted port is not an explicit 5060, as an omitted default component
  does not match an explicit one in a URI (§19.1.4). `received`, `rport` and
  the transport are no part of it, so a hop's §18.2.1 stamping leaves a copy
  matching. The comparison lives in `sip-message` (`SentByRef`, ADR-0025).
- **Collisions.** Two senders that pick one branch are two transactions —
  §17.2.3 compares the sent-by because branch uniqueness (§8.1.1.7) holds per
  UA only. A sender that reuses a branch for another method is read the same
  way: §17.2.3 names the method among the match conditions, so the second
  request is a new transaction, not a retransmission. Each gets the whole
  §17.2 behaviour, and a CANCEL matches only the INVITE of its own sent-by.
- **Cost.** A message is matched by an identity borrowed from it: one hash
  lookup, no allocation. Opening a server transaction copies its key into the
  map, the call index and each of its timers.

## Deferred (with justification)

- **`transactionBreakdown` gauge** (per-(method,role,state) walk of the map) —
  observability not asserted by the ported tests; the `method` field is carried
  so it is a pure addition later.
- **Tracing / OTel span re-parenting** (the `forkDetachedInScope` /
  `DETACHED_PARENT` machinery, `ForkSiteTracker`) — an Effect-tracer artefact
  with no tokio analogue; a send error is counted
  (`b2bua_txn_send_errors_total`), not traced.

New-call admission is not this layer's: it is the admission ladder above it
(`b2bua::admission`, ADR-0037). This layer judges only the backlog rung
(ADR-0037 item 6) and answers a copy of an INVITE the node already refused.
- **`send` legacy combined wrapper** — the source kept it only for incremental
  call-site migration; the Rust API ships `send_request`/`send_response`/
  `send_raw` directly.

## RNG seam

`newTag` / `newBranch` (the source's fiber-local Effect `Random`, deferred from
the message slice) land here as `IdGen` — a small **injectable value** (not a
trait), mirroring the clock seam: `IdGen::seeded(seed)` for deterministic tests,
`IdGen::from_entropy()` in production.

A response is matched to its client transaction by branch and CSeq method
(§17.1.3), so identifiers must be unguessable off-path: each is HMAC-SHA256
over a counter, keyed from the OS RNG in production (the seed is the key in
tests). The response's source is not compared with the transaction's
destination: RFC 3261 §18.1.2 does not ask it, a NAT, load balancer or
multi-homed peer answers from another address, and the on-path host left able
to forge can spoof the source too.

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
