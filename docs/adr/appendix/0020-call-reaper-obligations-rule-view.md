# Appendix: Call reaper, obligation vocabulary, and the narrowed rule-facing call view

The decision part: [ADR-0020](../0020-call-reaper-obligations-rule-view.md).

## Context

Cleanup is purely transition-driven, and four in-process escape routes skip it entirely. The
parenthesis of each states how the dispatch table and the reaper close it.

1. **Handler panic** — `dispatch.rs` swallows the `JoinError`; the call leaks
   forever, zero CDR, timers live.
2. **Queue/cap drop** — a dropped event may be the BYE or timer that was
   supposed to terminate the call; nothing notices. (A dropped non-INVITE
   request is readmitted on its retransmission and a dropped INVITE
   answered: the owed answer of its row in the dispatch table
   (`dispatch/class.rs`), paid by `router::owed`, and by `router::turn` for a
   body aborted before it ran; a `Cancelled` is queued past the bounds,
   up to a per-call ceiling past which the reaper tears the call down on the
   wire (BYE, CANCEL: its peers are alive, unlike a stale call's), and a
   verdict, every timer fire of the call (one per armed timer, the
   terminating call's timeout among them), every client transaction's
   outcome (at most three per transaction the call sent) and every answer to
   a request the call sent off its turn past every bound. A
   non-INVITE request left without a final when the call ends is forgotten at
   its release, and an in-dialog INVITE answered 481 there.)
3. **Lost `TerminatingTimeout`** — the 32 s stuck-in-Terminating watchdog is
   itself a losable timer. (It is admitted past every dispatch bound —
   the time bound. The work bound is `max_messages_per_call_lifetime`: the
   events offered for a call, counted at dispatch in every state until its
   release is queued, and past it a `message-cap` verdict ends the call: what
   its dispatch-table row does not keep past the cap is refused, a request
   answered where refused (`dispatch/class.rs`). Installed only with the
   reaper on.)
4. **Best-effort CDR** — `BufferedCdrWriter` drops on overflow; a panic before
   `WriteCdr` writes nothing; additionally `process_result` executes
   `RemoveCall` (which propagates the replica delete) *before* the buffered
   `WriteCdr` lane, so a failure in that window loses the CDR everywhere.

`RuleContext` exposing a bare `&Call` shows 33 public fields, including the `(p,b)` version
vector, relay frame state, tracing fields and rate-limit counters, and a `b2bua-sdk` that
re-exports the whole struct makes every framework internal a rule could read part of the
integrator stability contract (ADR-0016 X6).

## Decision X1: a Call reaper module owning "released exactly once, one CDR"

The public surface is three entry points (`new`, the dispatcher hooks, `start`).
Everything behind them (strike ledger, sweep cadence, wedge criteria,
watermarks) is implementation. There is no second teardown vocabulary: a verdict that rides
the CORE_LAYER rules makes reaps visible in the docgen state diagrams, and the RFC audit gates
reaper-emitted BYEs like any message. `reaper-stale` mirrors `terminating-safety-timeout`: the
call is dead. `reaper-fatal-error` follows a handler panic whose peers are alive, so its
teardown sends a BYE to each confirmed leg, a CANCEL to each early b-leg, and the final the
caller is owed. The discharge bypasses the rules because the rules are the thing that failed.
No `CallEvent` variant and no SDK change come with the reaper.

## Decision X2: in-process guarantee; the CDR lane reorder

A durable CDR WAL and a delete-after-CDR replica protocol were considered and rejected: they
couple CDR delivery to replica lifetime for a tail risk billing does not require.

Interpreting `RemoveCall` after the `WriteCdr` lane closes the in-process panic window between
eviction and CDR enqueue, and shrinks the out-of-scope crash window too. Nothing depends on
CDR-last, and the call is already evicted from routing before any slow sink runs.

## Decision X3: scope: primary-served + re-hydrated; the acting-backup terminal contract

Re-hydrated (reclaimed) calls must be covered at 100%. A takeover copy is filtered store-side
by the takeover mark, and `bak:` Elements are not in the live map. A reaped termination
propagates its delete through the terminate-writer, so the HA protocol is untouched.

With the primary as sole discharge authority, exactly-once holds by construction, with no
cross-node CDR or limiter idempotency. The primary discharges exactly once: immediately if
alive (its Reclaim-tail puller reconciles the dominating terminal into the LIVE map and
discharges via the funnel, ADR-0014), or on reboot reclaim (a reclaimed
`Terminated`/`Terminating` body discharges instead of re-serving).

If the primary never returns, the deferred Element's alive-timer expires. The
paced replica reap then evicts it, releases the call's limiter key and counts its
CDR lost. The record is lost with the primary (the accepted double-failure); the
limiter slot and the memory are not. The reap is the only eviction site of an
expired body, and no read evicts one before it. So each expired deferred terminal
is released once, whichever partition holds it: a replica its primary flushed, or
the takeover copy's own terminal.

A late reverse-flush racing the discharge cannot resurrect the call. The store-side
**delete tombstone** (apply-side delete-wins) rejects a re-creating `Put` of a
recently-deleted call; a new call on the same ref is not buried (ADR-0014,
"`(p,b)` orders one call incarnation"). It is required because the `(p,b)`
vector structurally cannot let a backup's discharged-marker apply to a primary
that has bumped `p`. The X1 promise holds (one funnel, one CDR); the reap task
is a durability backstop, not a reconciliation timer (ADR-0014 causality preserved).

## Decision X4: liveness is a last-touched stamp, refreshed at worker dequeue

Touching must not dirty the call or trigger a replication flush, hence a side map. The stamp
is refreshed at dequeue, not at enqueue: a wedged worker still *receives* retransmits, so
enqueue-stamping masks exactly the failure being hunted. It is not refreshed in the router:
zero caller lines means no caller can forget. A freshly reclaimed long-hold call starts its
idle clock at reclaim. Timer deadlines are no input because keepalive catch-up smoothing makes
a reclaimed call look overdue.

The stamp is cleared in `remove` and `drop_local`; an orphan never enters either map. A
healthy call, even an idle long-hold, dispatches a keepalive timer event every interval, so
staleness provably means lost events, never quietness. `enabled = false` is a debugging escape
hatch.

## Decision X5: confirm-watermark under the per-call lock

The watermark makes the sweep's check-then-act safe against racing traffic. Without the guard
the reaper could create the leak class it exists to kill.

## Decision X6: two-strike escalation; hung handlers are aborted, never bypassed

A handler that *hangs* holds both the worker and the per-call lock. Aborting the spawned body
drops the lock guard cleanly. A lockless hard-reap was considered and rejected: it was the only
design that acted without the per-call lock. Reaper bookkeeping is prunable node-local state,
which is why it carries no exactly-once.

A turn past a per-call cap is reported through the same hook (ADR-0057 §4): it
strikes a call not struck yet, its `fatal-error` verdict naming the cap; a later
turn's trip while that verdict is queued strikes nothing, and a verdict turn's
own trip is the second strike. The hook acts whether or not the sweep runs, and
a call's strikes are pruned at its release.

## Decision X7: the obligation vocabulary: the Call is the ledger

The resource-release mechanism is an **extraction, not an addition**: `invariants::enforce`
derives limiter decrements from `call.limiter_entries` and dedupes `WriteCdr`, and that
derivation lives in `obligations.rs`.

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
- A refactor-equivalence property test pins the extraction (old vs new `enforce`:
  identical effect multisets over arbitrary snapshots).

The speculative seams are cut: no `WedgePolicy` trait (a config struct with per-state
deadlines), no `PanicSink` trait (plain hooks/senders), no unused `ReapDisposition` variants.

## Decision X8: narrow the rule-facing call: `RuleCall` view, full width, view-first

`RuleContext.call` becomes a **refined view** (the CONTEXT.md idiom, extended
from SIP messages to the call): `RuleCall<'a>`, a newtype over `&'a Call` in
`b2bua-sdk` exposing only the semantic read surface (~16 fields: legs, state +
`sm_cursors`, features, service slices, `ext`, tag map, callback context, the
A-leg INVITE snapshot, `created_at`, `cdr_events`). **No `Deref` to `Call`, no
raw escape hatch** — the framework (executor, `ActionExecutor`, router) keeps
its own `&Call`. Framework internals (`topology`, pending-via/CSeq frame
state, tracing, counters, `limiter_entries`, `timers`, policy-update fields)
leave the rule-author interface entirely; the reaper's per-call context is
invisible to rules. The write side is untouched — rules already mutate
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
