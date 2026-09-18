# The b-leg CANCEL waits for the first provisional — bounded by a grace window, then goes regardless

**Status:** accepted (2026-08-09, amended 2026-08-10: the hold is bounded by
default — a grace expiry sends the CANCEL unconditionally; the literal §9.1
wait remains available as a configuration; amended 2026-08-26: an on-wire
CANCEL retransmits on a Timer-E ladder, X4)

## Context

RFC 3261 §9.1: *"If no provisional response has been received, the CANCEL
request MUST NOT be sent; rather, the client MUST wait for the arrival of a
provisional response before sending the request."* The wait is what makes the
CANCEL matchable: a UAS that has not yet responded may not have built the
INVITE server transaction — the early CANCEL draws a 481 while Timer-A INVITE
retransmits keep ringing a call that no longer exists — the orphaned ring.

The B2BUA's rules emit `cancel_to_leg` / `cancel_pending_reinvite` eagerly
(caller CANCEL, NoAnswer, service watchdogs) — the rule layer cannot know the
branch's response history. The transaction layer can.

Separately, the LB proxy absorbs a callee's `100 Trying` (RFC 3261 §16.7) and
— transaction-less BY DESIGN (ADR-0022 X4) — never emits its own. So in the
production topology (every worker b-leg egresses via the LB), a callee that
sends only a bare 100 leaves the worker's b-leg branch genuinely
**response-less** until the first 18x.

An *unbounded* §9.1 wait therefore creates a class of callee that is never
CANCELed at all: behind the LB, every abandoned setup whose callee sent only a
100 kept ringing (Timer-A retransmits included) until the callee's own give-up
or the worker's terminating backstop. Under sustained INVITE+abandon churn the
callee side accumulates ringing calls faster than they resolve — cancellation
running structurally slower than call setup never catches up. The literal RFC
wait is judged wrong for this B2BUA: **every CANCEL the rules emit for a live
b-leg MUST reach the wire, without exception.** The wait survives only as a
short courtesy window, never as a veto.

## Decision

### X1 — The client transaction owns CANCEL wire timing; the hold is bounded

`sip-txn`'s `send_request` CANCEL path is the seam. A CANCEL whose branch
matches a Client/Invite txn:

- **still in `Trying` (zero responses)** — is HELD on the txn and flushed on
  the branch's first provisional (100-absorb and 1xx>100 paths both flush).
  The hold arms a **grace timer** (`TransactionConfig::cancel_hold_grace_ms`,
  default `CANCEL_HOLD_GRACE` = 2·T1 = 1 s): if the window expires with the
  branch still response-less, the CANCEL is sent anyway. The grace-sent
  datagram stays armed for **one** re-send on a late first provisional — a UAS
  that 481'd the pre-1xx copy (it had not built the server txn) has it by
  then, so the re-send is the matchable one and the orphaned-ring defect
  stays fixed even when the original INVITE was lost.
- **in `Completed` (final already taken)** — is SUPPRESSED: §9.1/§9.2, a
  CANCEL has no effect on an answered request. Not an exception to the
  always-send rule: a final response resolves the leg on its own (487/486
  reject path, or the crossing-2xx reap), so no ring can persist.
- whose txn **dies holding it**:
  - **call evict** (`cancel_txns_for_call`) and **client timeout** (Timer B /
    the transaction bound) — a never-sent held CANCEL is flushed to the wire
    *first*: neither path may swallow a CANCEL still inside its grace window
    (a tight custom config can let the bound outrun the grace). The evict
    orphans the txn rather than deleting it (ADR-0034), so the flushed CANCEL
    rides its Timer-E ladder from there.
  - **final** — cleared unsent; the callee answered, cancellation is moot
    (§9.2, and the crossing-2xx reap owns the late answer).
  - Residual unsent-death paths, all pathological and counted in
    `held_cancels_dropped`: a same-branch txn displacement and the safety-net
    sweep (both indicate a bug elsewhere). The timeout death send is a single
    raw datagram (the txn is deleted in the same turn, so no ladder can ride
    it): if that one datagram is lost, the callee still rides the terminating
    backstop.

A CANCEL matching **no txn** is still sent raw: an absent txn is not proof the
INVITE ended — a takeover-restored call (ADR-0014) CANCELs a b-leg whose
INVITE client txn lived on the failed peer, and dropping it would orphan a
protected ringing callee. Counters: `cancels_held`, `held_cancels_flushed`
(first-provisional flush), `held_cancels_flushed_pre1xx` (grace/evict send),
`held_cancels_reflushed` (the one post-grace re-send, informational),
`held_cancels_dropped` (never reached the wire — final beat the grace, or the
txn died inside it); held == flushed + flushed_pre1xx + dropped.

**The policy is configurable** (`TransactionConfig::cancel_hold_grace_ms`:
`Some(ms)` bounded / `None` strict; B2BUA knob
`cancel_strict_rfc3261_wait` / env `B2BUA_CANCEL_STRICT_RFC_WAIT`). The
default is the bounded hold. Strict mode is the literal §9.1 wait — the
pre-amendment behavior, kept selectable for RFC-conformance-first deployments
and so both behavior families stay test-covered: the CANCEL is held until a
provisional arrives, dropped with a dying txn (evict included), and a
100-only/silent callee is never CANCELed (it rides the terminating backstop —
the accepted cost the original decision documented).

### X2 — Behind the LB, a 100-only b-leg is CANCELed at grace expiry

The LB still forwards no 100 (ADR-0022 X4 stands; no LB-synthesized or relayed
provisional is introduced), so a worker CANCEL for a b-leg whose callee sent
only a bare 100 is held the full grace window — then sent. The callee's server
transaction exists (it answered the INVITE with that 100), so the CANCEL
matches and draws 200 + 487: the abandoned callee stops ringing and the slot
frees within ~grace + RTT instead of the terminating backstop. A **late
answer** crossing the held/grace-sent CANCEL still resolves the `Cancelling`
leg and is reaped with ACK + immediate BYE; a **fully silent** callee (never
answers even the CANCEL) is still bounded by the worker's own dead-call
detection (terminating backstop / setup timers).

### X3 — `cancel-after-1xx` is informational, never gating

With the bounded hold sanctioned as SUT policy, a literal §9.1 breach is no
longer a defect class: the audit rule is **advisory on every lane**
(`{Proxy}`-declared lanes stay exempt — a relay forwards the upstream's CANCEL
at the upstream's timing). Severity split by timing:

- a pre-1xx CANCEL ≥ `CANCEL_GRACE_FLOOR_US` (900 ms, just under the 1 s
  grace) after the lane's first INVITE on the branch is the deliberate
  grace-expiry send — **no finding at all**;
- an under-floor pre-1xx CANCEL (an eager-CANCEL regression toward a UAS that
  may not have built its server txn — the orphaned ring — or a
  scripted fixture race) surfaces as an **informational advisory**, not a
  gating non-compliance.

The floor and the grace default must move together.

### X4 — An on-wire CANCEL retransmits on a Timer-E ladder

A CANCEL is a non-INVITE request (RFC 3261 §9.1) and owes §17.1.2.2
retransmission over UDP, but it reuses its INVITE's branch and the txns map is
branch-keyed, so it deliberately builds no client transaction of its own (a map
entry would displace the live INVITE client txn). Instead the ladder rides the
INVITE client txn as a sub-state of the parked datagram: every send that leaves
a live txn behind it — the direct pass-through, the grace expiry, the
first-provisional flush — arms a `CancelRetransmit` timer paced T1 → doubling →
capped at T2. The ladder stops on the first response whose CSeq method is
CANCEL, on the INVITE txn taking a final, on txn death (evict / timeout /
displacement), and at its own 64·T1 ceiling — and the ceiling gives up on the
CANCEL only: the INVITE client txn continues under its own bound and still owes
a final. A superseding CANCEL replaces the parked datagram, so the ladder
always replays the newest copy; a CANCEL matching no txn stays a raw single
send (nothing to hang a ladder on without re-keying the map). ACK is exempt —
a 2xx ACK is TU-owned and rides no timer (§13.2.2.4). Counter:
`cancel_retransmits`.

## Pinned by

`sip-txn/tests/cancel_hold.rs`, `sip-txn/tests/cancel_retransmit.rs` (X4),
`b2bua-harness/tests/cancel_before_provisional.rs`,
`b2bua-harness/tests/no_answer_cancelled_call.rs`,
`failover-harness/tests/silent_callee_no_answer_via_lb.rs` (via-LB shape, X2).
