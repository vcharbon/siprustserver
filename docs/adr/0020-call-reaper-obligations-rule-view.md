# 0020. Call reaper, obligation vocabulary, and the narrowed rule-facing call view

**Status:** accepted (2026-06-11). Amended with ADR-0055 §5 (X3), ADR-0047 D8 (X3) and
ADR-0057 §4 (X6).

The escape routes in full, the rationale of each section and the consequences in detail:
[appendix](appendix/0020-call-reaper-obligations-rule-view.md).

## Context

The terminal-cleanup guarantee of ADR-0010 X5 (`invariants::enforce` appends cancel-all-timers,
write-cdr, limiter-decrement and remove-call-last on the `→ Terminated` transition) is a
property of the happy path, not an interface any module owns. Four in-process routes skip it:
a handler panic, a dropped event that was to terminate the call, a lost `TerminatingTimeout`,
and a best-effort CDR. Separately, a rule that reads a bare `&Call` puts every framework
internal into the integrator stability contract (ADR-0016 X6). Detail:
[appendix, context](appendix/0020-call-reaper-obligations-rule-view.md#context).

## Decision X1: a Call reaper module owning "released exactly once, one CDR"

`crates/b2bua/src/reaper.rs` owns the promise: every call admitted to the live map is released
exactly once, through `release_call` + `invariants::{finalize, enforce}`, with exactly one CDR.

A reaper verdict is a synthetic `CallEvent::InternalEvent { topic: "reaper" }` down the
re-entry channel, through the per-call FIFO and lock, handled by CORE_LAYER rules in
`defaults.rs`. `reaper-stale` forces the legs terminal with no wire message.
`reaper-fatal-error` (strike 1) tears an Active call down on the wire through the ordinary
teardown and forces a terminating call terminal (ADR-0051 §5). Only the strike-2 discharge
bypasses the rules, and it still runs `finalize → enforce → process_result →
release_call(Terminated)`. Detail: [appendix X1](appendix/0020-call-reaper-obligations-rule-view.md#decision-x1-a-call-reaper-module-owning-released-exactly-once-one-cdr).

## Decision X2: in-process guarantee; the CDR lane reorder

The guarantee covers panics, dropped events, lost timers and wedged `Terminating`. Hard-crash
windows are out of scope: HA reclaim owns post-crash recovery. `process_result` interprets the
terminal `RemoveCall` after the buffered `WriteCdr` lane. `enforce` still forces `RemoveCall`
last within the critical lane. Detail: [appendix X2](appendix/0020-call-reaper-obligations-rule-view.md#decision-x2-in-process-guarantee-the-cdr-lane-reorder).

## Decision X3: scope: primary-served + re-hydrated; the acting-backup terminal contract

The sweep covers only calls this node serves live as primary, re-hydrated calls included. It
never selects an acting-backup takeover copy and cannot see `bak:` Elements. Self-release stays
push-based on `CallQuiesced` (ADR-0014): the reaper adds no time-based input to HA
reconciliation.

The acting-backup terminal contract: a backup answers the wire, records the terminal into the
`(p,b)` Element, reverse-flushes it with a short grace TTL as its alive-timer, and
self-releases its live copy. It never discharges a call (no CDR, no limiter release, no
propagated delete), not even on an explicit BYE or CANCEL, with the exceptions of ADR-0047 D8:
a copy whose primary has departed discharges the call, and one whose primary reads dead at a
dead verdict releases the limiter key at its end, the CDR still deferred. The primary is the
sole discharge authority. If it never returns, the paced replica reap evicts the expired
Element, releases its limiter key and counts its CDR lost. A store-side delete tombstone stops a late reverse-flush from resurrecting the call.
Detail: [appendix X3](appendix/0020-call-reaper-obligations-rule-view.md#decision-x3-scope-primary-served--re-hydrated-the-acting-backup-terminal-contract).

## Decision X4: liveness is a last-touched stamp, refreshed at worker dequeue

Liveness derives from one input: a node-local, store-side last-touched stamp, a side map in
`CallState`, never a `Call` field. A dispatcher hook refreshes it at worker dequeue, and every
materialisation site (`create`, `hydrate_from_replica`, `materialize_if_absent`) sets it. Never
`created_at`, never timer deadlines. Stamp membership mirrors live-map membership. The idle
threshold defaults to 3× the keepalive cadence, and `B2buaConfig::validate()` rejects anything
below 2×. The reaper is on by default; with `enabled = false` the exactly-once promise does
not hold. Detail: [appendix X4](appendix/0020-call-reaper-obligations-rule-view.md#decision-x4-liveness-is-a-last-touched-stamp-refreshed-at-worker-dequeue).

## Decision X5: confirm-watermark under the per-call lock

The sweep records the stamp it observed and the reap event carries it as a watermark.
`process` discards the event under the per-call lock if the stamp has moved or the entry is
gone, so a late reap event never resurrects a released call through on-demand reclaim.

## Decision X6: two-strike escalation; hung handlers are aborted, never bypassed

The dispatcher reports handler failures through a hook. Strike 1 (first panic, or first stale
verdict) is a synthetic event through the rules. Strike 2 (the rules path failed, or the verdict
never ran) is the discharge: under the per-call lock, the last persisted snapshot is forced
terminal (`ByeDisposition::ByeTimeout` on unresolved legs and a reason-carrying `CdrEvent`)
and run through `enforce`. For a hung handler, `abort_in_flight` aborts the spawned body and
lets the queued verdict run. Exactly-once is carried by peek-under-lock and `enforce`'s
`became_terminated` edge, never by reaper bookkeeping.

A turn past a per-call cap is reported through the same hook (ADR-0057 §4) and strikes a call
not struck yet. The hook acts whether or not the sweep runs, and a call's strikes are pruned at
its release. Detail: [appendix X6](appendix/0020-call-reaper-obligations-rule-view.md#decision-x6-two-strike-escalation-hung-handlers-are-aborted-never-bypassed).

## Decision X7: the obligation vocabulary: the Call is the ledger

`crates/b2bua/src/obligations.rs` holds the release derivation `enforce` performs. An
obligation is a per-call consequence discharged exactly once at release, derivable from the
persisted `Call` snapshot alone (including `ext` slices), and idempotently dischargeable
through the effect vocabulary. `ObligationSet::settle` is called by
`enforce`, and so by the strike-2 discharge: one derivation, every terminal path. A new kind
records its allocation on the Call, adds one effect variant and one `process_result` arm, and
registers itself. The reaper is never edited. Detail:
[appendix X7](appendix/0020-call-reaper-obligations-rule-view.md#decision-x7-the-obligation-vocabulary-the-call-is-the-ledger).

## Decision X8: narrow the rule-facing call: `RuleCall` view, full width, view-first

`RuleContext.call` is `RuleCall<'a>`, a newtype over `&'a Call` in `b2bua-sdk` exposing only
the semantic read surface. It has no `Deref` to `Call` and no raw escape hatch. Framework internals stay
out of the rule-author interface. Rules mutate only through `RuleAction`. Detail:
[appendix X8](appendix/0020-call-reaper-obligations-rule-view.md#decision-x8-narrow-the-rule-facing-call-rulecall-view-full-width-view-first).

## Consequences

- `B2buaConfig` carries `reaper_enabled`, `reaper_sweep_interval_sec` and
  `reaper_idle_max_sec` (on by default, `0` derives 3× keepalive).
- CDR delivery stays at-most-once at the sink. The promise is "exactly one emit".

The rest: [appendix, consequences](appendix/0020-call-reaper-obligations-rule-view.md#consequences).

## References

- [ADR-0010](./0010-b2bua-dispatch-rules-rust-shape.md) X5,
  [ADR-0014](./0014-reactive-only-takeover-version-vector.md) (the red line X3 respects),
  [ADR-0016](./0016-callflow-service-state-machines.md) (CORE_LAYER rules, SDK contract, `ext`
  slices).
- `CONTEXT.md`, "Call reaper vocabulary".
- Implementation plan: [docs/plan/call-reaper-obligations-rule-view.md](../plan/call-reaper-obligations-rule-view.md).
