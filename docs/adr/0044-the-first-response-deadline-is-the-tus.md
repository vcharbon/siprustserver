# The first-response deadline is the TU's

**Status:** accepted (2026-10-11). Replaces the first-response amendment of
ADR-0032. Amended (2026-10-29): default 5 s, the alive report, the 480.

**Owner:** `crates/sip-txn/src/layer/client.rs` (`fire_first_response`),
`B2buaConfig::invite_first_response_timeout_sec`.

## Context

A b-leg initial INVITE that draws nothing — not even a `100 Trying` — is
talking to a hop that is down. RFC 3261's Timer B (64·T1 = 32 s) is how long
the client transaction waits before reporting that, and a caller cannot be
left in silence that long before the reroute to an alternate hop. The bound
was tightened inside the transaction, which made it a Timer B deviation.

Only the transaction layer sees a 100: it absorbs it and never hands it to the
TU, so a TU timer of its own could not tell "nothing at all" from "a 100 and
silence".

## Decision

1. **The TU gives up on the hop; the transaction keeps RFC timing.** On an
   initial INVITE (no To-tag) the transaction layer arms the TU's deadline,
   `invite_first_response_timeout_sec` (`B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC`,
   range 2..=32 s, default 5), beside Timer B. Fired with the branch still in
   `Trying`, it reports `Timeout { kind: Response }` to the TU once; the
   `call_failure` consult carries `timeout_kind: "response"`. The transaction
   keeps Timer A to Timer B and gives up there without a second report. The
   default gives up fast because a silent next hop on an initial INVITE is
   better routed elsewhere (another hop, another site) than waited on; a
   silence in a live dialog is a short network cut, so in-dialog requests
   keep the RFC timers.
2. **A hop a `100 Trying` alone answered is reported alive.** The deadline
   fired with the branch in `Proceeding` and no provisional above 100 tells
   the TU `HopAlive` once, the call's `hop-alive` internal event: the TU learns
   at the deadline which of "nothing at all" and "a 100, then silence" it
   holds, the one fact the absorbed 100 otherwise hides from it. Nothing else
   is reported; a lost notice leaves the TU on its own deadlines.
3. **A silent hop let stand is a 480.** A failure of `timeout_kind:
   "response"` the decision lets stand, or one raised with no decision
   context, answers the caller `480 Temporarily Unavailable`, as a callee that
   never answered does; `"transaction"` (it answered, then drew no final)
   keeps `408`.
4. **The abandoned INVITE stays a transaction.** The TU's CANCEL on it is held
   (ADR-0043 X1); a late provisional sends it, a late final is ACKed, and no
   later timeout of the transaction is reported. A provisional after the
   report leaves Timer B armed: the long INVITE bound is not swapped in.
5. **Only the initial INVITE reads it.** An in-dialog INVITE and every
   non-INVITE keep Timer B / Timer F, and a provisional before the deadline
   swaps in the long `invite_txn_timeout_sec` bound as before.

## Consequences

- A dead hop costs the INVITE's full Timer A ladder (seven sends to 31.5 s)
  after the TU has moved on.
- The 2 s floor keeps two Timer A re-sends ahead of the deadline; the 5 s
  default keeps three (0.5, 1.5, 3.5 s).
- A call whose callee has not progressed past a 100 at the deadline takes
  one more turn, the alive report.

## Pinned by

`sip-txn/tests/it/fsm.rs`
(`the_first_response_deadline_tells_the_tu_while_timer_a_runs_to_timer_b`),
`sip-txn/tests/it/invite_give_up.rs`,
`b2bua-harness/tests/it/decision_context.rs`
(`b_leg_that_draws_nothing_fails_at_the_first_response_bound_as_response`),
`b2bua-harness/tests/it/timed_out_invite_cancel.rs`
(`a_silent_hop_let_stand_answers_the_caller_480`),
`sip-txn/tests/it/hop_alive.rs`.
