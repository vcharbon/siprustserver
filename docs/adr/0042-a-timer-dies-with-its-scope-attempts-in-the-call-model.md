# A timer dies with its scope; attempts are in the call model

**Status:** accepted (2026-10-10)

**Owner:** `crates/call/src/model/timer.rs`, `attempt.rs` and
`helpers/attempt.rs`; the engine side is `crates/b2bua/src/router/timer_screen.rs`,
`rules/actions/retire.rs`, `attempts.rs` and `rules/lifecycle_delivery.rs`.

## Context

A timer was named by a string (`NoAnswer:b-1`) and cancelled by that string,
so each rule that ended a leg or a call had to know every timer it should
cancel, and a leg that ended without the right cancel left its timer armed. A
fire already handed to the router when its timer was re-armed reached the
rules, which re-checked by hand whether the scope it guarded was still alive.
A service that needed a watchdog per routing answer kept its own cursor of
which answer it was on, its own suffixed timer ids, and an observer rule that
ran beside the rule that claimed the routing answer's turn — only when ranked
above that rule, and not at all on the turns no event exposed (a limiter
refusal, a failover a service claimed).

The call model had no notion of a routing answer and the legs it dials: the
dialled legs shared one vector with the media legs a service parks, and the
next leg id was predicted as `b-{len+1}` wherever a rule needed it.

## Decision

1. **A timer is a typed key under a generation.** `TimerKey { timer_type,
   scope }`, `TimerScope::{Call, Attempt(id), Leg(id), Obligation(o)}`. A call
   holds at most one entry per key; arming a key replaces it under the call's
   next generation (`Call::timer_generation`, replicated). Cancel takes the
   key. There is no string id.
2. **A stale fire is screened.** The driver fires `(key, generation)`. The
   router drops a fire whose key the ledger holds under another generation, or
   not at all (trace `timer.drop`), and removes the entry it delivers. A rule
   that handles a fire handles a live instance; it re-checks only business
   conditions. A fire across a takeover or reclaim is screened the same way.
3. **Leaving a scope retires it.** `retire(Scope)` disarms every timer of the
   scope, beside the ladders it owes: a cancelled, destroyed or terminated
   leg retires `Leg`; an attempt's end retires `Attempt`; a service machine's
   deactivation retires its service timers; the start of the call's
   termination retires `Call` (the teardown's own safety net, the limiter
   refresh and the deadlines of answers awaited off the turn excepted). No
   rule cancels a timer its scope already took.
4. **An attempt is one routing answer that dials and the legs it dials.**
   `Call.attempts: Vec<Attempt>`, `Attempt { id, cause, decision_ordinal,
   legs, no_answer, outcome }`; a dialled leg names its attempt
   (`Leg.attempt`), a media leg names none. Causes: the initial route
   (`Route`, dialled by the core or by the service its routing deferred to), a
   setup failover (`Failover`), an established call's reroute (`Reroute`), a
   transfer target (`Transfer`). Every route admission refuses is an attempt
   that ends at once `Refused`; a fold computed off the call's turn carries the
   routes refused on its way for the engine to record. An attempt ends once:
   `Answered` (a leg answered), `Failed` (every leg ended and waits on
   nothing), `Superseded` (the next attempt opened while a leg still rang or
   was being cancelled), `CallEnded`, `Refused`. A destination `CreateLeg`
   opens its attempt (`AttemptOpening`); one engine function opens them all.
   The next leg id is the engine's (`next_b_leg_id`).
5. **Lifecycle events replace observer rules.** At the end of each turn the
   engine settles the open attempts, retires the ended ones, and delivers
   `AttemptStarted { attempt, route }`, `AttemptEnded { attempt, outcome }`
   and `LegEnded { leg, cause }` — the ones the turn caused, in that order per
   attempt, then the legs — to every service whose machine is active,
   whichever rule claimed the turn. A machine the turn deactivated still
   hears them for its call-ext writes. A handler may write call ext, move its
   own machine's cursor and arm or cancel its own service timers; anything
   else is an authoring bug. A timer a handler arms under an attempt already
   ended is retired with it. There is no observing rule.
6. **The deadline of a routing answer is a `Deadline`.** `Off` or
   `After(duration)`, resolved once per attempt: absent, zero or negative is
   off, anything else is honoured at millisecond precision.
7. **The model states it.** The per-turn model invariants
   hold that no armed timer names a terminated leg, an ended attempt or a
   service machine without a cursor, and the trace records `timer.arm`,
   `timer.cancel`, `timer.fire` and `timer.drop` per call (ADR-0026).

## Consequences

- A rule that ends a leg, an attempt or the call writes no cancel for the
  timers guarding it; a service's watchdog per routing answer is a timer
  scoped to the attempt, armed on `AttemptStarted`, and needs no cursor of
  its own.
- A service timer exists only while its machine does: a machine-less rule
  cannot hold one, and no service timer fires inside the terminating window.
- `no_answer` 0 no longer arms an immediate fire, and a sub-second deadline
  is honoured as stated.
- The replicated call body carries the attempts and the timer generation; a
  copy restored on another node keeps both, so a fire scheduled before the
  move is screened like any other.
