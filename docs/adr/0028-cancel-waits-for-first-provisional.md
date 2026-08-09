# The b-leg CANCEL waits for the branch's first provisional (RFC 3261 §9.1)

**Status:** accepted (2026-08-09)

## Context

RFC 3261 §9.1: *"If no provisional response has been received, the CANCEL
request MUST NOT be sent; rather, the client MUST wait for the arrival of a
provisional response before sending the request."* The wait is what makes the
CANCEL matchable: a UAS that has not yet responded may not have built the
INVITE server transaction — the early CANCEL draws a 481 while Timer-A INVITE
retransmits keep ringing a call that no longer exists (the orphaned ring,
upstreamneed-070).

The B2BUA's rules emit `cancel_to_leg` / `cancel_pending_reinvite` eagerly
(caller CANCEL, NoAnswer, service watchdogs) — the rule layer cannot know the
branch's response history. The transaction layer can.

Separately, the LB proxy absorbs a callee's `100 Trying` (RFC 3261 §16.7) and
— transaction-less BY DESIGN (ADR-0022 X4) — never emits its own. So in the
production topology (every worker b-leg egresses via the LB), a callee that
sends only a bare 100 leaves the worker's b-leg branch genuinely
**response-less** until the first 18x.

## Decision

### X1 — The client transaction owns CANCEL wire timing

`sip-txn`'s `send_request` CANCEL path is the seam. A CANCEL whose branch
matches a Client/Invite txn:

- **still in `Trying` (zero responses)** — is HELD on the txn and flushed on
  the branch's first provisional (100-absorb and 1xx>100 paths both flush).
- **in `Completed` (final already taken)** — is SUPPRESSED: §9.1/§9.2, a
  CANCEL has no effect on an answered request, and sending it would put a
  pre-1xx CANCEL on the wire when the final raced the caller's decision.
- whose txn **dies holding it** (Timer B, 2xx final, call evict, sweep) — is
  dropped: nothing is owed to a dead transaction.

A CANCEL matching **no txn** is still sent raw: an absent txn is not proof the
INVITE ended — a takeover-restored call (ADR-0014) CANCELs a b-leg whose
INVITE client txn lived on the failed peer, and dropping it would orphan a
protected ringing callee. Counters: `cancels_held`, `held_cancels_flushed`,
`held_cancels_dropped` (held == flushed + dropped), `cancels_suppressed_on_final`.

### X2 — Behind the LB, a 100-only b-leg is never CANCELed — accepted

Because the LB forwards no 100 (X4 of ADR-0022 stands; no LB-synthesized or
relayed provisional is introduced), a worker CANCEL for a b-leg whose callee
sent only a bare 100 is held forever and dies with the transaction. The
abandoned callee resolves without a CANCEL:

- a **late answer** crosses the held CANCEL — the 2xx resolves the
  `Cancelling` leg and is reaped after the fact with ACK + immediate BYE (the
  brief one-sided dialog is the §9.1-compliant shape of the old crossing);
- a **silent callee** is reaped by the worker's own dead-call detection (the
  terminating backstop / setup timers), with Timer-A INVITE retransmits
  running until the leg dies.

Accepted cost: a pre-provisional-CANCELed call occupies its slot (plus INVITE
retransmits) for up to the terminating backstop instead of milliseconds after
a 487. This is bounded, traffic-proportional to *abandoned* setups only, and
judged cheaper than giving the LB a provisional-emitting duty it must not have
(transaction-less, ADR-0022 X4).

### X3 — `rfc3261.cancelAfter1xx` gates forwarder lanes

With the wait implemented, the audit rule gates INVITE-forwarding (B2BUA/AS)
lanes; pure-originator fixture lanes stay advisory (a scripted caller
abandoning pre-1xx is a legitimate race); `{Proxy}`-declared lanes are exempt
(a relay forwards the upstream's CANCEL at the upstream's timing).

## Pinned by

`sip-txn/tests/cancel_hold.rs`, `b2bua-harness/tests/cancel_before_provisional.rs`,
`failover-harness/tests/silent_callee_no_answer_via_lb.rs` (via-LB shape, X2).
