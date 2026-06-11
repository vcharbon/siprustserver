# 0020 — Call reaper, obligation vocabulary, and the narrowed rule-facing call view

**Status:** accepted (2026-06-11)

## Context

The terminal-cleanup guarantee of ADR-0010 X5 (`invariants::enforce` appends
cancel-all-timers / write-cdr / limiter-decrement / remove-call-last on the
`→ Terminated` transition) is an *implementation property of the happy path*,
not an interface any module owns. Cleanup is purely transition-driven; an
architecture review (2026-06-11) found four in-process escape routes that skip
it entirely:

1. **Handler panic** — `dispatch.rs` swallows the `JoinError`; the call leaks
   forever, zero CDR, timers live.
2. **Queue/cap drop** — a dropped event may be the BYE or timer that was
   supposed to terminate the call; nothing notices.
3. **Lost `TerminatingTimeout`** — the 32 s stuck-in-Terminating watchdog is
   itself a losable timer.
4. **Best-effort CDR** — `BufferedCdrWriter` drops on overflow; a panic before
   `WriteCdr` writes nothing; additionally `process_result` executes
   `RemoveCall` (which propagates the replica delete) *before* the buffered
   `WriteCdr` lane, so a failure in that window loses the CDR everywhere.

Separately, `RuleContext` exposes a bare `&Call` (33 public fields, including
the `(p,b)` version vector, relay frame state, tracing fields and rate-limit
counters), and `b2bua-sdk` re-exports the whole struct — so the integrator
stability contract (ADR-0016 X6) currently includes every framework internal a
rule could ever read.

Three competing interface designs (minimal / extensible / caller-optimised)
were produced and hybridised. The decisions below pin the hybrid.

## Decision X1 — a Call reaper module owning "released exactly once, one CDR"

A new `crates/b2bua/src/reaper.rs` owns the promise: **every call admitted to
the live map is released exactly once, through the existing `release_call` +
`invariants::{finalize, enforce}` path, with exactly one CDR.** Public surface
≈ three entry points (`new` / dispatcher hooks / `start`); everything else
(strike ledger, sweep cadence, wedge criteria, watermarks) is implementation.

There is **no second teardown vocabulary**: a reaper verdict is a synthetic
`CallEvent::InternalEvent { topic: "reaper" }` down the existing re-entry
channel, through the per-call FIFO and per-call lock, handled by **two
CORE_LAYER rules in `defaults.rs`** (`reaper-stale`, `reaper-fatal-error`)
that mirror `terminating-safety-timeout` — so reaps are visible in the docgen
state diagrams and reaper-emitted BYEs are gated by the RFC audit like any
message. Only the strike-2 **discharge** path bypasses the *rules* stage
(rules are the thing that failed); it still runs
`finalize → enforce → process_result → release_call(Terminated)`. No new
`CallEvent` variant, no SDK change.

## Decision X2 — in-process guarantee; the CDR lane reorder

The guarantee covers panics, dropped events, lost timers, and wedged
`Terminating`. **Hard-crash windows are explicitly out of scope** — HA reclaim
owns post-crash recovery; a durable CDR WAL and a delete-after-CDR replica
protocol were considered and rejected (couples CDR delivery to replica
lifetime for a tail risk billing does not currently require).

One funnel change ships with this: `process_result` interprets the terminal
`RemoveCall` **after** the buffered `WriteCdr` lane (today the replica delete
propagates before the CDR is enqueued). This closes the in-process
panic-window between eviction and CDR enqueue, and incidentally shrinks the
out-of-scope crash window. Nothing depends on CDR-last; `enforce` still forces
RemoveCall last *within the critical lane* and the call is already evicted
from routing before any slow sink runs.

## Decision X3 — scope: primary-served + re-hydrated; the acting-backup terminal contract

The sweep covers **only calls this node serves live as primary** — including
re-hydrated (reclaimed) calls, which must be covered at 100%. It never selects
an acting-backup **takeover copy** (filtered store-side by the takeover mark)
and structurally cannot see `bak:` **Elements** (they are not in the live
map). **Self-release stays push-based on `CallQuiesced`** (ADR-0014) — the
reaper adds no time-based input to HA reconciliation, and a reaped
termination propagates its delete through the existing terminate-writer, so
the HA protocol is untouched.

The **acting-backup terminal contract**: a backup terminates a call (CDR +
cleanup + propagated delete) only when *serving an explicit termination
message* (BYE/CANCEL) during its takeover window. Timer-driven teardown
(GlobalDuration, keepalive timeout) of a failed-over call waits for the
reclaiming primary's reaper. An Element whose primary never returns expires by
TTL with no CDR — an accepted gap (an "abandoned-Element CDR" was considered
and rejected: duplicate-CDR risk under late reclaim for a case outside the
"live-served + re-hydrated" promise).

## Decision X4 — liveness is a last-touched stamp, refreshed at worker dequeue

Liveness derives from **one input only**: a node-local, store-side
**last-touched stamp** — a side map in `CallState`, **never a `Call` field**
(touching must not dirty the call or trigger a replication flush). It is
refreshed by a dispatcher hook **at worker dequeue** (not at enqueue: a wedged
worker still *receives* retransmits, so enqueue-stamping masks exactly the
failure being hunted; not in the router: zero caller lines means no caller can
forget) and at every materialisation site (`create`,
`hydrate_from_replica`, `materialize_if_absent`) — so a freshly reclaimed
long-hold call starts its idle clock at reclaim. **Never `created_at`, never
timer deadlines** (keepalive catch-up smoothing makes a reclaimed call look
overdue — the stale-`KeepaliveTimeout` bug class).

Stamp membership mirrors live-map membership: cleared in `remove` and
`drop_local`; an orphan never enters either. The idle threshold defaults to
3× the keepalive cadence and `B2buaConfig::validate()` rejects anything below
2× — a healthy call (even an idle long-hold) dispatches a keepalive timer
event every interval, so staleness provably means lost events, never
quietness. The reaper is **on by default**; `enabled = false` is a debugging
escape hatch under which the exactly-once promise does not hold.

## Decision X5 — confirm-watermark under the per-call lock

The sweep records the stamp it observed; the synthetic reap event carries it
as a watermark, and `process` discards the event under the per-call lock if
the stamp has moved or the entry is gone. This makes the sweep's
check-then-act safe against racing traffic **and prevents a late reap event
for a released call from resurrecting it from the replica store via on-demand
reclaim** — without the guard the reaper could create the leak class it
exists to kill.

## Decision X6 — two-strike escalation; hung handlers are aborted, never bypassed

The dispatcher reports handler failures through a hook where the `JoinError`
is swallowed today. **Strike 1** (first panic, or first stale verdict): a
synthetic event through the normal rules. **Strike 2** (the rules path itself
failed, or the verdict event never ran): **discharge** — under the per-call
lock, the last persisted snapshot is forced terminal
(`ByeDisposition::ByeTimeout` on unresolved legs + a reason-carrying
`CdrEvent`) and run through the ordinary enforce pipeline.

A handler that *hangs* holds both the worker and the per-call lock; the
escalation for an undelivered verdict is `abort_in_flight` — abort the spawned
body (the lock guard drops cleanly), then let the queued verdict flow through
the normal funnel. A lockless hard-reap was considered and rejected (the only
design that acted without the per-call lock). Exactly-once is carried by
peek-under-lock plus `enforce`'s `became_terminated` edge — never by reaper
bookkeeping, which is prunable node-local state.

## Decision X7 — the obligation vocabulary: the Call is the ledger

The generic resource-release mechanism is an **extraction, not an addition**:
`invariants::enforce` already derives limiter decrements from
`call.limiter_entries` and dedupes `WriteCdr`. That derivation moves to
`crates/b2bua/src/obligations.rs`:

- An **obligation** is a per-call consequence that must be discharged exactly
  once at release, **derivable from the persisted `Call` snapshot alone**
  (including `ext` slices) and idempotently dischargeable through the existing
  effect vocabulary. Kinds are pure over the snapshot — closures are
  unrepresentable, so the ledger survives rehydration by construction.
- `ObligationKind` (`settle` — derive + dedupe + append in one idempotent pass
  per kind, since dedupe semantics are kind-local; plus an `owed` audit view)
  with `LimiterObligations` and `CdrObligation` as verbatim extractions;
  `ObligationSet::settle` is called by `enforce` (normal path) and thereby by
  the strike-2 discharge (bypass path) — **one derivation, every terminal
  path**. A future kind (e.g. a media-port release) plugs in via: record the
  allocation on the Call, add one effect variant + one `process_result` arm,
  register the kind. The reaper is never edited.
- A **parallel allocation registry was rejected** (the mirror/slice divergence
  hazard; would not survive failover, while `limiter_entries`/`cdr_events`
  ride the replicated Element for free).
- The extraction lands as its own commit, gated by a refactor-equivalence
  property test (old vs new `enforce`: identical effect multisets over
  arbitrary snapshots).

Per the designs' own simplification analysis, the speculative seams are cut:
no `WedgePolicy` trait (a config struct with per-state deadlines), no
`PanicSink` trait (plain hooks/senders), no unused `ReapDisposition` variants.

## Decision X8 — narrow the rule-facing call: `RuleCall` view, full width, view-first

`RuleContext.call` becomes a **refined view** (the CONTEXT.md idiom, extended
from SIP messages to the call): `RuleCall<'a>`, a newtype over `&'a Call` in
`b2bua-sdk` exposing only the semantic read surface (~16 fields: legs, state +
`sm_cursors`, features, service slices, `ext`, tag map, callback context, the
A-leg INVITE snapshot, `created_at`, `cdr_events`). **No `Deref` to `Call`, no
raw escape hatch** — the framework (executor, `ActionExecutor`, router) keeps
its own `&Call`. Framework internals (`topology`, pending-via/CSeq frame
state, tracing, counters, `limiter_entries`, `timers`, policy-update fields)
leave the rule-author interface entirely; the reaper's new per-call context is
born invisible to rules. The write side is untouched — rules already mutate
only via `RuleAction` through the executor.

The **full** narrowing ships at once (not a minimal view): the migration is
compiler-driven, what breaks is exactly the fields rules already use, and the
SDK has one external adapter (the announcement crate) — the cheapest moment
this will ever be (ADR-0016 X6: easier to open than to close). The view lands
**first**, before the obligation extraction and the reaper. `Leg` stays
concrete this pass (its one true internal is `pending_invite_txn`); a
`RuleLeg` view is deferred until it earns its keep.

## Consequences

- `process_result` and `release_call` become `pub(crate)` (the discharge path
  calls them); `RouterCtx` gains `obligations` and the reaper handle.
- `PerCallDispatcher` gains dequeue/failure/exit hooks and
  `abort_in_flight` (one `AbortHandle` per in-flight body).
- `B2buaConfig` gains `reaper_enabled` / `reaper_sweep_interval_sec` /
  `reaper_idle_max_sec` (on by default, `0` derives 3× keepalive); the runner
  diff is empty (defaults are the contract; env knobs are a later additive
  change).
- New metrics: `handler_panics_total`, `reaper_sweeps_total`,
  `reaper_verdicts_total`, `reaper_discharged_total` (alarm — expected ~0; the
  reap *reason* travels in the CDR disposition, not a metric label), and the
  `store_touched` ledger gauge; `assert_fully_reaped` in the harness gains
  `touched_count == 0`.
- Harness scenarios that deliberately freeze a call beyond the idle threshold
  must stretch `idle_after` or disable the reaper — one knob.
- The b2bua-sdk surface shrinks (breaking for integrators); the announcement
  crate migrates in the same slice.
- CDR delivery stays at-most-once at the sink (`BufferedCdrWriter`
  drop-on-overload is unchanged); the promise is "exactly one emit".

## References

- [ADR-0010](./0010-b2bua-dispatch-rules-rust-shape.md) X5 (the invariant
  enforcer this deepens), [ADR-0014](./0014-reactive-only-takeover-version-vector.md)
  (self-release / no time-based reconciliation — the red line X3 respects),
  [ADR-0016](./0016-callflow-service-state-machines.md) (CORE_LAYER rules,
  SDK contract, `ext` slices).
- `CONTEXT.md` — "Call reaper vocabulary" (Call reaper, Obligation,
  Last-touched stamp, Acting-backup terminal contract).
- Implementation plan: [docs/plan/call-reaper-obligations-rule-view.md](../plan/call-reaper-obligations-rule-view.md).
