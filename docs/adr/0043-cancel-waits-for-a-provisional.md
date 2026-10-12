# The CANCEL waits for a provisional, without bound

**Status:** accepted (2026-10-11). Supersedes ADR-0028. Amended (2026-10-12):
the front proxy relays the callee's 100 (Context, X1, Consequences).

**Owner:** `crates/sip-txn/src/layer/client.rs` (the CANCEL gate, the
provisional flush, the Timer-E ladder), `crates/sip-txn/src/layer/seed.rs`
(the seeded client INVITE's state), `crates/rfc-rules/src/rules/cancel.rs`
(`cancel-after-1xx`).

## Context

RFC 3261 §9.1: *"If no provisional response has been received, the CANCEL
request MUST NOT be sent; rather, the client MUST wait for the arrival of a
provisional response before sending the request."* A UAS that has not answered
may not have built the INVITE server transaction: an early CANCEL draws a 481
while Timer-A retransmits keep ringing a call nobody can cancel.

The rules emit `cancel_to_leg` / `cancel_pending_reinvite` whenever the call
decides to abandon a leg (caller CANCEL, no-answer, service watchdogs); only
the transaction layer knows the branch's response history. ADR-0028 bounded
the wait with a grace window after which the CANCEL went regardless. That
bound sent the CANCEL §9.1 forbids, and is withdrawn.

The front proxy is stateless (§16.11, ADR-0022 X4) and relays a callee's
`100 Trying` to the worker like any response, so a worker's b-leg branch sees
the callee's first provisional on every path, through the proxy or direct.

## Decision

### X1 — The client transaction holds a CANCEL until the first provisional

`sip-txn`'s `send_request` is the seam. A CANCEL whose branch matches a client
INVITE transaction:

- **in `Trying` (no response)** is held on the transaction and flushed on its
  first provisional (a 100 included), however late — on a live transaction,
  an orphaned one (ADR-0034) and one that gave up alike. A branch that never
  draws a provisional never sees it: the INVITE ends at Timer B, is held 64·T1
  past its `Timeout`, and the held CANCEL dies with it.
- **in `Proceeding`** goes on the wire at once.
- **in `Completed` or `Accepted`, or matching no transaction** (a 2xx deletes
  it) is suppressed: the final already resolved the request (§9.2).

A newer CANCEL replaces a held one. Counters: `cancels_held`,
`held_cancels_flushed`, `held_cancels_dropped` (never on the wire: a final came
first, or the transaction ended with no provisional); held == flushed +
dropped.

A client INVITE rebuilt from a replicated record (ADR-0014) opens `Proceeding`
only when the record holds a provisional for it — a dialog on the initial
INVITE's leg — and `Trying` otherwise, so the survivor's CANCEL obeys the same
wait. A CANCEL the record says was sent with no provisional on record is still
held when the node that sent it dies: the record keeps the headers it stated
(`OutboundInvite::cancel_headers`), and the seed carries it rebuilt, held on
the `Trying` transaction until the callee's first 18x reaches the survivor.

### X2 — The caller is answered at once

A caller's CANCEL of its own INVITE is answered by the server transaction in
the turn it arrives (200 to the CANCEL, 487 to the INVITE); the b-leg CANCEL
the call emits then follows X1.

### X3 — `cancel-after-1xx` gates

The rule is a hard gate on every lane but a `{Proxy}`-only one (a transparent
relay forwards its upstream's CANCEL at the upstream's timing). Only a 1xx the
emitter took on the branch before the CANCEL discharges it; elapsed time does
not.

### X4 — An on-wire CANCEL retransmits on a Timer-E ladder

A CANCEL is a non-INVITE request (§9.1) and owes §17.1.2.2 retransmission over
UDP. It reuses its INVITE's branch and builds no client transaction of its own
(a map entry would displace the INVITE's), so the ladder rides the INVITE
client transaction as a sub-state of the parked datagram: every send that
leaves a live transaction behind it arms a `CancelRetransmit` timer paced T1 →
doubling → capped at T2. The ladder stops on the first response whose CSeq
method is CANCEL, on the INVITE taking a final, on transaction death, and at its
own 64·T1 ceiling, which gives up on the CANCEL only. A superseding CANCEL
replaces the parked datagram. A CANCEL matching no transaction stays a raw
single send. Counter: `cancel_retransmits`.

## Consequences

- A callee that has sent nothing is not CANCELed: its INVITE resolves on the
  callee's own final or the worker's Timer B, and the call that abandoned it
  rides its terminating backstop until that leg resolves
  (`silent_callee_abandoned_via_lb__reject__delayed_crossing_200_still_reaped`).
  A 2xx it sends after the call was released is ACKed and the dialog it
  created BYEd by the orphaned transaction (ADR-0034).
- A callee that has sent a 100 alone is CANCELed on the wire when the call
  abandons it, behind the front proxy as on a direct path
  (`trying_only_callee_abandoned_via_lb__reject__is_cancelled`).
- After a takeover only the callee's 18x or final can release a CANCEL held at
  the kill: a 100 the callee sends to an initial INVITE afterwards carries no
  cookie the front proxy can fail it over on (no Record-Route, and the proxy's
  Via on an initial INVITE carries none), and a 100 matching no transaction is
  absorbed by the layer without materialising the call. The CANCEL stays held
  past the callee's 100 and goes on its 18x
  (`a_cancel_held_at_the_kill_is_sent_by_the_survivor_on_the_callees_18x_not_its_100`).
- A re-INVITE round the survivor of a takeover rebuilt with no provisional on
  record is never CANCELed by it.

## Pinned by

`sip-txn/tests/it/cancel_hold.rs`, `sip-txn/tests/it/cancel_retransmit.rs`
(X4), `sip-txn/tests/it/seed.rs`
(`a_cancel_on_a_seed_with_no_provisional_waits_for_one`),
`b2bua-harness/tests/it/cancel_before_provisional.rs`,
`failover-harness/tests/it/abandoned_callee_via_lb.rs`,
`failover-harness/tests/it/inflight_resend.rs`,
`failover-harness/tests/it/held_cancel_takeover.rs`,
`b2bua/src/router/materialise/seeds/tests.rs`,
`rfc-rules/src/rules/cancel/tests.rs`
(`a_pre_1xx_cancel_is_violated_however_late`).
