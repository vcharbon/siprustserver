# An initial INVITE's 100 leaves once its call starts

**Status:** accepted.

**Owner:** the switch and the arming in `crates/sip-txn/src/layer/trying.rs`, the anchor and
the start bound in `crates/b2bua/src/router/initial_turn.rs`, the bound's reading in
`crates/b2bua/src/setup_budget.rs`, the re-ask's in `crates/b2bua/src/router/undecided.rs`.

Amends ADR-0022 (context, X1) and ADR-0047 (context, D3, D4).

## Context

A 100 Trying tells the previous hop that its request is being worked on: the hop stops
retransmitting and runs no Timer B (RFC 3261 §17.1.1.2). The transaction layer sent it 200 ms
after an INVITE arrived whatever the router did. A worker whose router was wedged, or whose
turns waited for their permits, told the previous hop it held a call it had not started. The
hop then waited on that worker, when a node that cannot start the call should stay silent so
that the hop's own timer (Timer B, or an SBC's reroute timer) sends the call elsewhere.

## Decision

1. **The switch.** `TransactionConfig::consumer_arms_initial_trying`, off by default: the
   §17.2.1 bound of an initial INVITE (no To-tag) is not armed as its transaction opens. Until
   the consumer arms it (`TransactionLayer::arm_trying`) or releases the 100
   (`release_trying`), the transaction sends nothing and absorbs the INVITE's copies, as a
   `Trying` transaction with nothing to repeat does. An in-dialog INVITE keeps its bound from
   its opening. The B2BUA turns the switch on.
2. **The anchor.** The initial INVITE's turn creates the call, offers its creation write to
   its backup when it has one, queues the 100 behind that write (the existing rider), and
   arms the bound in the poll that sends the decision request. The anchor sits as close to the
   HTTP request as the code allows: the arming is a queued command with no reply, so no await
   of the owner's turn sits between it and the request. The 100 then leaves at the earliest of
   the rider's release (the backup applied the write, the sync cap passed, or at once with no
   backup) and 200 ms after the arming. A backup that never answers, an outbox that does not
   drain and an unbacked call each still get their 100.
3. **The start bound.** A new call whose turn starts `B2BUA_CALL_START_BOUND_MS` (4000) or
   more after its INVITE's receipt creates no call, asks nothing and answers the bare 503 of an
   unavailable decision (ADR-0022 X3), with no 100 before it: its call is born only to write its
   record and CDR (ADR-0050 §9), counted `start_bound` on `b2bua_new_calls_total`. An
   emergency INVITE is held to it too: the hop has failed it over, or does on the 503, and
   proceeding would ring the PSAP twice. The order is: cancelled, cordoned, the start bound,
   the setup budget. A turn waiting for its permits stops waiting at its start bound.
4. **The re-ask.** A call taken over before its first decision (ADR-0047 D4) is held to the
   same start bound, counted from the INVITE's receipt its record states (`Call::received_at`),
   never from the takeover. The node asking again cannot tell whether the caller heard a 100.
   Inside the bound, it asks the decision again with a budget of its own and serves the call.
   Past it, it asks nothing, answers the bare 503 through the transaction the takeover seeded,
   and dials no one: a caller that heard no 100 may have been failed over already, and a dial
   would ring the callee twice.
5. **Bounds.** The start bound is positive and below the setup budget
   (`B2BUA_CALL_CONTROL_TIMEOUT_MS`); with replication, start bound + sync cap is below the
   setup budget. Both are refused at startup. Start bound + sync cap must sit under the time
   the previous hop retransmits an INVITE with no 100 before it fails the call over: a 100
   that reaches the hop after it gave up draws its CANCEL (§9.1). The default is 4 s, for a
   hop that retransmits for at least 5 s: 900 ms under it with the 100 ms sync cap, eight
   times the worst 100 delay measured on a worker at three times its capacity (502 ms), and
   above the proxy's dead verdict (2.6 to 3.6 s), so a call taken over can still be asked
   again.
6. **The sync cap** keeps its bounds, below T1 and below the §17.2.1 bound: a re-INVITE's 100
   is still released behind its write, and the ACK and CANCEL bounds stay on that bound.

## RFC 3261

- **§17.2.1, departed from**: "The server transaction MUST generate a 100 (Trying) response
  unless it knows that the TU will generate a provisional or final response within 200 ms".
  With the switch on, an initial INVITE whose turn has not reached its consult draws no 100 at
  200 ms. The departure is limited to that window, and it is the only MUST departed from. A
  node that cannot start the work does not say it is working on it, so the previous hop's own
  timers fail the request over.
- §17.2.1 copies: a copy received with nothing sent draws nothing; a copy after the 100
  draws the 100 again. §8.2.6.1: a late 100 states its delay in its `Timestamp`. §9.2: a
  CANCEL before any 100 still draws 200 and 487.
- §9.1 is the client's: a caller with no provisional does not CANCEL; its give-up is silent.
  The start bound keeps a late turn from ringing anyone.
- The RFC audit's `proxy-100-within-grace` rule is advisory and charged to the front proxy;
  no rule of the SUT's own output fires on the withheld 100.

## Consequences

- At nominal load the 100 leaves well under 200 ms (p99 1 to 3 ms on one worker at
  1000 calls/s), so nothing changes on the wire.
- A stuck worker is silent; the previous hop retransmits and reroutes on its timer. A
  retransmission follows the proxy's memo to the same worker while the proxy reads it alive or
  slow (ADR-0069): the probe is answered on the router loop, so a wedged loop reads dead after
  the probe's threshold.
- Retransmissions of a new INVITE become ordinary under load: a call slower than T1 to its
  creation write is retransmitted. A copy rewrites its own entry in the proxy's retransmission
  memo and adds none. A memo sized for the call rate (`calls_per_second` of its bounds) keeps
  the entry to Timer B. Past that rate the memo forgets its oldest writes first: the defaults,
  sized for 109 calls/s, keep an entry about 4.4 s after its last write at 1000 calls/s. A copy
  after a longer Timer A gap (the copies at 15.5 s and 31.5 s) is then placed as a new call,
  which may ring a callee the previous hop already rerouted to.
- The copies of a call whose primary died before its turn go to the cookie's backup, which
  holds no record and drops each one unanswered.
- A CANCEL that meets a turn waiting for its permit comes from a caller that broke §9.1.
- Residuals: a call ended with `Caller::Leave` (the stuck-poll discharge, an unrecoverable
  record, an ended body, a reclaim's ending); an unbacked call whose worker dies after its
  100; a consult that blocks its poll after the 100. An SBC retry on the same Call-ID meets the
  merged-copy 482 (same CSeq) or the identity-in-use 500 with a `Retry-After` (next CSeq).

## Rejected

- **No fallback 100.** A call with no backup, or a backup that never answers, would draw no
  100 at all from a worker that serves it.
- **The fallback at the call's creation.** It would arm before the creation write is offered
  and before the decision request, so a worker that never reaches the request would still say
  it works on the call.
- **A start bound counted from the turn's start.** The caller's silence runs from the
  INVITE's receipt, and so does the previous hop's timer.
