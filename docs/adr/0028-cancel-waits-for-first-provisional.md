# The b-leg CANCEL waits for the first provisional, bounded by a grace window, then goes regardless

**Status:** superseded by [ADR-0043](0043-cancel-waits-for-a-provisional.md) (2026-10-11), which
holds the CANCEL without bound. Accepted (2026-08-09).

The long form of each section, its counters and the tests:
[appendix](appendix/0028-cancel-waits-for-first-provisional.md).

## Context

RFC 3261 §9.1: *"If no provisional response has been received, the CANCEL
request MUST NOT be sent; rather, the client MUST wait for the arrival of a
provisional response before sending the request."* A UAS that has not yet responded may not
have built the INVITE server transaction, so an early CANCEL draws a 481 while Timer-A INVITE
retransmits keep ringing a call that no longer exists: the orphaned ring. The rules emit
`cancel_to_leg` / `cancel_pending_reinvite` eagerly and cannot know the branch's response
history; the transaction layer can. With an LB that absorbs a callee's `100 Trying`, a callee
that sends only a bare 100 leaves the b-leg branch response-less until the first 18x, and an
unbounded wait never CANCELs it. This ADR judges the literal wait wrong for the B2BUA: every
CANCEL the rules emit for a live b-leg must reach the wire. Detail:
[appendix, context](appendix/0028-cancel-waits-for-first-provisional.md#context).

## Decision

### X1: The client transaction owns CANCEL wire timing; the hold is bounded

`sip-txn`'s `send_request` CANCEL path is the seam. A CANCEL on a branch still in `Trying` is
held and flushed on the first provisional, under a grace timer
(`TransactionConfig::cancel_hold_grace_ms`, default 2·T1 = 1 s) after which it is sent anyway.
A CANCEL on a `Completed` branch is suppressed. A call evict or client timeout flushes a
never-sent held CANCEL first. A CANCEL matching no transaction is sent raw. The policy is
configurable: `None` is the strict §9.1 wait (`cancel_strict_rfc3261_wait`,
`B2BUA_CANCEL_STRICT_RFC_WAIT`). Detail:
[appendix X1](appendix/0028-cancel-waits-for-first-provisional.md#x1-the-client-transaction-owns-cancel-wire-timing-the-hold-is-bounded).

### X2: Behind the LB, a 100-only b-leg is CANCELed at grace expiry

The callee's server transaction exists, since it answered with that 100, so the grace-sent
CANCEL matches and draws 200 + 487. Detail:
[appendix X2](appendix/0028-cancel-waits-for-first-provisional.md#x2-behind-the-lb-a-100-only-b-leg-is-canceled-at-grace-expiry).

### X3: `cancel-after-1xx` is informational, never gating

The audit rule is advisory on every lane. A pre-1xx CANCEL at or past `CANCEL_GRACE_FLOOR_US`
(900 ms) is no finding; one under the floor is an informational advisory. The floor and the
grace default move together. Detail:
[appendix X3](appendix/0028-cancel-waits-for-first-provisional.md#x3-cancel-after-1xx-is-informational-never-gating).

### X4: An on-wire CANCEL retransmits on a Timer-E ladder

The ladder rides the INVITE client transaction as a sub-state of the parked datagram, paced
T1 → doubling → capped at T2, stopped by a CANCEL response, the INVITE's final, transaction
death or its own 64·T1 ceiling. Detail:
[appendix X4](appendix/0028-cancel-waits-for-first-provisional.md#x4-an-on-wire-cancel-retransmits-on-a-timer-e-ladder).

## Pinned by

[Appendix, pinned by](appendix/0028-cancel-waits-for-first-provisional.md#pinned-by).
