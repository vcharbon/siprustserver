# 0010 — B2BUA Rust shape: one crate, per-call FIFO, rule engine, replication-aware store

**Status:** accepted (2026-05-31)

**Source:** sipjsserver @ `fffc4ac69c8aeef26cf48fe73469503145c9732b`,
`src/sip/{SipRouter,PerCallDispatcher}.ts`, `src/call/CallState.ts`,
`src/b2bua/rules/`, `src/decision/`, `src/cdr/CdrWriter.ts`.

## Context

The B2BUA-only slices — the per-call FIFO dispatcher + router (deferred from the
transaction layer, ADR-0007 §X3), the in-memory call store (`CallState`,
deferred from the data-model slice, ADR-0008), and the rule engine — land
together because they are mutually dependent (rules mutate the `Call`, the
dispatcher serializes per call, the store holds the `Call`). They sit on the
already-ported `call` data model, `sip-txn`, `sip-message`, `sip-net`, and
`sip-clock`.

## Decision X1 — one `crates/b2bua` crate

Rust forbids crate cycles; the dispatcher ↔ router ↔ rules ↔ store are mutually
dependent, so they share one crate (`crates/b2bua`) on top of `call`. The test
harness lives in a separate `crates/b2bua-harness` (X9).

## Decision X2 — per-call FIFO = per-call queues + worker tasks + a global semaphore

A faithful port of source ADR-0004/0005, not the txn-layer actor (ADR-0007 X2).
The dispatcher owns `callRef → bounded mpsc`; each call has one worker task that
runs its handler bodies in strict FIFO order; a global `Semaphore` caps total
in-flight handlers. Handler bodies run on spawned sub-tasks the worker awaits, so
a panicking handler is isolated and the worker survives. cap-drop / queue-drop /
saturation are atomic counters. Rationale: a slow handler on one call must not
block other calls — the single actor (the txn-layer choice) would stall every
call, which is exactly what `PerCallDispatcher` exists to prevent. `CallState`
adds a per-`callRef` lock as a second serialization layer (uncontended under the
dispatcher; it guards out-of-band callers like a future orphan sweep).

## Decision X3 — replication-aware `CallStore` seam, in-memory impl no-ops HA

`CallState` holds live calls *typed* in memory (the `sip_index` is the routing
fast-path); the persistence path encodes via `MsgpackCodec` through a `CallStore`
trait shaped after `PartitionedRelayStorage`. The trait's method signatures
already carry every HA parameter — partition `role`/`primary`, propagate
`peer`/`direction`, `call_gen`, index keys, `ttl` — but the shipped
`InMemoryCallStore` ignores them. A replicating implementation (propagate-set /
topology-gen / a peer drain) drops in later with **zero changes** to the rule
engine, the dispatcher, or `CallState`. The terminate path runs through a
`BufferedTerminateWriter` (non-blocking submit + drainer) so the router never
blocks on the store — the seam is identical even though the in-memory store
cannot stall.

## Decision X4 — decision-engine adapter seam + a scripted test backend

The `CallDecisionEngine` trait (`new_call` / `call_failure` / `call_refer`) is
the seam the B2BUA calls to route a new INVITE. The production HTTP adapter is
deferred; a `ScriptedDecisionEngine` emulates the jssip reference backend by
inspecting the request JSON (R-URI / To / `X-*` headers / body) and returning a
route/reject, so existing SIPp scenario scripts keep producing the same call
flows. `apply_route` + `handle_initial_invite` translate the response into call
state (features, service ext, limiter, b-leg creation).

## Decision X5 — rule engine: first-match, layer-ranked, with framework invariants

Rules are `RuleDefinition`s with a declarative `Match` (request/response/timer/
timeout/cancelled/internal-event columns + a sync corner-case `filter`). The
matcher ranks by layer (desc) then registration order, drops `overrides`-displaced
rules, and the first handler returning actions wins. Actions run through an
`ActionExecutor` that mutates a working `Call` and emits typed effects. The
B2BUA *regenerates* messages on the peer leg's own transaction/dialog rather than
rewriting bytes (back-to-back UAs: independent tags/CSeq/Contact). The
`InvariantEnforcer` guarantees cleanup on the `→ terminated` transition
(cancel-all-timers, write-cdr, remove-call-last) and promotes
`terminating → terminated` once all legs resolve.

The basic-B2BUA default rule set is ported (relay / dialog / absorb / lifecycle /
terminating / corner-case / failure / timer). The 18x-management strategies
(`relayFirst18xTo180`, `promote18xPemTo200`, PEM/fake-prack) and REFER transfer
(SERVICE_LAYER) are **deferred** — their action vocabulary is defined but dormant.

## Decision X6 — B2BUA-local timer DelayQueue

`TimerService` owns one `tokio_util::time::DelayQueue` driver (the ADR-0007
shape) firing `CallEvent::Timer`s the router routes through the per-call FIFO.
It is B2BUA-local because `sip-txn`'s queue is private to its actor. It rides
`tokio::time`, so `pause`/`advance` drives it in tests.

## Decision X7 — buffered CDR through the recording

`CdrWriter` (`write` / `read_all`); the `InMemoryCdrWriter` lets tests assert
exactly one CDR per terminated call; `BufferedCdrWriter` is the production
drop-on-overload buffer (passthrough at `queue_max == 0`). The CDR is written
once, on termination, carrying the accumulated `Call.cdr_events`.

## Decision X8 — OPTIONS 200 minimal; drain/health deferred

Out-of-dialog OPTIONS is answered 200 OK inline so a fronting proxy's health
probe passes. `DrainingState` / `WorkerReadiness` / `OverloadController` (the
serving/draining/ready 200/503/silence matrix + Tier-3 admission) are a distinct
deferred slice — see the new MIGRATION_STATUS "Draining / readiness / overload"
line.

## Decision X9 — `b2bua-harness` test crate (extends ADR-0006)

The alice ↔ b2bua ↔ bob e2e tests run in a dedicated `crates/b2bua-harness`
that depends on both `scenario-harness` and `b2bua`, binding a real `B2buaCore`
on the simulated fabric via `Harness::bind_sut`. This keeps `scenario-harness`
generic (no B2BUA knowledge) and avoids the dev-dependency cycle the proxy slice
took with `with_proxy`. One additive harness change: the fluent UAC's
`expect_response` now absorbs an unsolicited `100 Trying` (a real UAC does, and a
stateful txn layer emits one) — see `scenario-harness/src/agent.rs`.

## Deferred (with justification)

- **18x-management strategies + REFER transfer** — SERVICE_LAYER policy modules;
  user-deferred. Vocabulary present, dormant.
- **Real HTTP decision adapter** — the scripted backend stands in.
- **Failover via `call_failure`** — the rule path relays the failure + tears the
  call down; the async failover round-trip (and the `refer-async-http`
  re-entrant fire-and-forget) lands with the transfer/decision-HTTP slice.
- **Real CallLimiter** (row 20) — no-op admit/decrement.
- **HA replication transport** — the `CallStore` seam carries the params; the
  replicating impl + orphan-sweep / `loadOwnedCalls` rehydrate are the HA slice.
- **Draining / readiness / overload** — minimal OPTIONS 200 only (X8).
- **Tracing / OTel span machinery** — no tokio analogue this slice.

## References

- [`crates/b2bua/src/dispatch.rs`](../../crates/b2bua/src/dispatch.rs),
  [`router.rs`](../../crates/b2bua/src/router.rs),
  [`store/`](../../crates/b2bua/src/store/),
  [`rules/`](../../crates/b2bua/src/rules/)
- [ADR-0007](./0007-transaction-layer-rust-shape.md) (X3 deferral),
  [ADR-0008](./0008-call-context-data-model.md) (CallState deferral),
  [ADR-0006](./0006-scenario-harness-recording-first.md) (SUT seam).
