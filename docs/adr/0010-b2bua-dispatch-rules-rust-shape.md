# 0010: B2BUA Rust shape. One crate, per-call FIFO, rule engine, replication-aware store

**Status:** accepted.

**Source:** sipjsserver `src/sip/{SipRouter,PerCallDispatcher}.ts`, `src/call/CallState.ts`,
`src/b2bua/rules/`, `src/decision/`, `src/cdr/CdrWriter.ts`.

## Context

The per-call FIFO dispatcher and router (ADR-0007 §X3), the in-memory call store
(`CallState`, ADR-0008) and the rule engine depend on each other: rules mutate the `Call`,
the dispatcher serializes per call, the store holds the `Call`. They sit on the `call` data
model, `sip-txn`, `sip-message`, `sip-net` and `sip-clock`.

## Decision X1. One `crates/b2bua` crate

Rust forbids crate cycles; the dispatcher ↔ router ↔ rules ↔ store are mutually
dependent, so they share one crate (`crates/b2bua`) on top of `call`. The test
harness lives in a separate `crates/b2bua-harness` (X9).

## Decision X2. Per-call FIFO = per-call queues + worker tasks + a global semaphore

A faithful port of source ADR-0004/0005, not the txn-layer actor (ADR-0007 X2).
The dispatcher owns `callRef → queue`; each call has one worker task that runs
its handler bodies in strict FIFO order; a global `Semaphore` caps total
in-flight handlers. Handler bodies run on spawned sub-tasks the worker awaits, so
a panicking handler is isolated and the worker survives. cap-drop / queue-drop /
saturation are atomic counters. An offer to a call's queue either queues the
event or hands it back with why it was discarded and its owed answer (an
answer, a forget of its transaction, or nothing), which the router renders. An
event the protocol will not send again queues past the bounds in FIFO order
instead, up to a per-call overflow ceiling (the queue depth); the release, the
reaper verdicts, every timer fire, every client transaction's outcome and every
answer to a request the call sent off its turn (ADR-0037) queue past every
bound. Rationale: a slow handler on one call must not block other calls, and a single
actor (the txn-layer choice) would stall every call. `CallState` adds a per-`callRef`
lock as a second serialization layer (uncontended under the dispatcher; it
orders the out-of-band callers, the departure sweep and the reclaim and reap
passes, against the call's turns). One mutex stands per `callRef` while any
task holds or waits on it; the last guard of a call no longer resident removes
its entry, so a release under the lock is never crossed by a second mutex. A
pass running on the router loop or on the reaper's task never waits on the
lock: it takes it only when free and otherwise queues the step as a turn of the
call's own queue.

Each event class's room, lifetime-cap counting and owed answer: one row of the dispatch table, [`dispatch/class.rs`](../../crates/b2bua/src/dispatch/class.rs).

## Decision X3. Replication-aware `CallStore` seam, in-memory impl no-ops HA

`CallState` holds live calls *typed* in memory (the `sip_index` is the routing
fast-path); the persistence path encodes via `MsgpackCodec` through a `CallStore`
trait shaped after `PartitionedRelayStorage`. The trait's method signatures
already carry every HA parameter (partition `role`/`primary`, propagate
`peer`/`direction`, `call_gen`, index keys, `ttl`), and
`InMemoryCallStore` ignores them. The replicating implementation (ADR-0011) plugs into the
same seam with no change to the rule engine, the dispatcher or `CallState`. The terminate
path runs through a `BufferedTerminateWriter` (non-blocking submit and drainer), so the
router never blocks on the store, whichever store is behind the seam.

## Decision X4. Decision-engine adapter seam + a scripted test backend

The `CallDecisionEngine` trait (`new_call` / `call_failure` / `call_refer`) is
the seam the B2BUA calls to route a new INVITE. The production HTTP adapter is
deferred; a `ScriptedDecisionEngine` emulates the jssip reference backend by
inspecting the request JSON (R-URI / To / `X-*` headers / body) and returning a
route/reject, so existing SIPp scenario scripts keep producing the same call
flows. `apply_route` + `handle_initial_invite` translate the response into call
state (features, service ext, limiter, b-leg creation).

## Decision X5. Rule engine: first-match, layer-ranked, with framework invariants

Rules are `RuleDefinition`s with a declarative `Match` (request/response/timer/
timeout/cancelled/internal-event columns + a sync corner-case `filter`). The
matcher ranks by layer (desc) then registration order, drops `overrides`-displaced
rules, and the first handler returning actions wins. Actions run through an
`ActionExecutor` that mutates a working `Call` and emits typed effects. The
B2BUA *regenerates* messages on the peer leg's own transaction/dialog rather than
rewriting bytes (back-to-back UAs: independent tags/CSeq/Contact). The
`InvariantEnforcer` performs cleanup on the `→ terminated` transition
(cancel-all-timers, write-cdr, remove-call-last) and promotes
`terminating → terminated` once all legs resolve. (This is the happy-path
property; [ADR-0020](./0020-call-reaper-obligations-rule-view.md) adds the call
reaper + obligation vocabulary that make it exactly-once across the panic / dropped
-event / lost-timer / wedged-`Terminating` escape routes.)

The basic-B2BUA default rule set is ported (relay / dialog / absorb / lifecycle /
terminating / corner-case / failure / timer). The 18x-management strategies
(`relayFirst18xTo180`, `promote18xPemTo200`, PEM/fake-prack) and REFER transfer
(SERVICE_LAYER) are deferred. Their action vocabulary is defined but dormant.

## Decision X6. B2BUA-local timer DelayQueue

`TimerService` owns one `tokio_util::time::DelayQueue` driver (the ADR-0007
shape) firing `CallEvent::Timer`s the router routes through the per-call FIFO.
It is B2BUA-local because `sip-txn`'s queue is private to its actor. It rides
`tokio::time`, so `pause`/`advance` drives it in tests.

## Decision X7. Buffered CDR through the recording

`CdrWriter` (`write` / `read_all`); the `InMemoryCdrWriter` lets tests assert
exactly one CDR per terminated call; `BufferedCdrWriter` is the production
drop-on-overload buffer (passthrough at `queue_max == 0`). The CDR is written
once, on termination, carrying the accumulated `Call.cdr_events`.

## Decision X8. OPTIONS 200 minimal; drain/health deferred

Out-of-dialog OPTIONS is answered inline so a fronting proxy's health probe passes. The
readiness/OPTIONS gate of ADR-0011 X6 sets the answer (200, or 503 not-ready or
draining).

## Decision X9. `b2bua-harness` test crate (extends ADR-0006)

The alice ↔ b2bua ↔ bob e2e tests run in a dedicated `crates/b2bua-harness`
that depends on both `scenario-harness` and `b2bua`, binding a real `B2buaCore`
on the simulated fabric via `Harness::bind_sut`. This keeps `scenario-harness`
generic (no B2BUA knowledge) and avoids a dev-dependency cycle like `with_proxy`'s. The
fluent UAC's `expect_response` absorbs an unsolicited `100 Trying`, as a real UAC does
(`scenario-harness/src/agent.rs`).

## Deferred (with justification)

- **18x-management strategies and REFER transfer**: SERVICE_LAYER policy modules,
  user-deferred. Vocabulary present, dormant.
- **Real HTTP decision adapter**: the scripted backend stands in.
- **Failover via `call_failure`**: the rule path relays the failure + tears the
  call down; the async failover round-trip (and the `refer-async-http`
  re-entrant fire-and-forget) lands with the transfer/decision-HTTP slice.
- **Real CallLimiter** (row 20): no-op admit/decrement.
- **Tracing / OTel span machinery**: no tokio analogue.

## References

- [`crates/b2bua/src/dispatch/`](../../crates/b2bua/src/dispatch/),
  [`router/`](../../crates/b2bua/src/router/),
  [`store/`](../../crates/b2bua/src/store/),
  [`b2bua-core/src/rules/`](../../crates/b2bua-core/src/rules/)
- [ADR-0007](./0007-transaction-layer-rust-shape.md) (X3 deferral),
  [ADR-0008](./0008-call-context-data-model.md) (CallState deferral),
  [ADR-0006](./0006-scenario-harness-recording-first.md) (SUT seam).
