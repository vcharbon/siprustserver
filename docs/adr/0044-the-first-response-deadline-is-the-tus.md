# The first-response deadline is the TU's

**Status:** accepted (2026-10-11). Replaces the first-response amendment of
ADR-0032.

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
   range 2..=32 s, default 32 — which arms nothing), beside Timer B. Fired
   with the branch still in `Trying`, it reports `Timeout { kind: Response }`
   to the TU once; the `call_failure` consult carries `timeout_kind:
   "response"`. The transaction keeps Timer A to Timer B and gives up there
   without a second report.
2. **The abandoned INVITE stays a transaction.** The TU's CANCEL on it is held
   (ADR-0043 X1); a late provisional sends it, a late final is ACKed, and no
   later timeout of the transaction is reported. A provisional after the
   report leaves Timer B armed: the long INVITE bound is not swapped in.
3. **Only the initial INVITE reads it.** An in-dialog INVITE and every
   non-INVITE keep Timer B / Timer F, and a provisional before the deadline
   swaps in the long `invite_txn_timeout_sec` bound as before.

## Consequences

- A dead hop costs the INVITE's full Timer A ladder (seven sends to 31.5 s)
  after the TU has moved on.
- The 2 s floor keeps two Timer A re-sends ahead of the deadline.

## Pinned by

`sip-txn/tests/it/fsm.rs`
(`the_first_response_deadline_tells_the_tu_while_timer_a_runs_to_timer_b`),
`sip-txn/tests/it/invite_give_up.rs`,
`b2bua-harness/tests/it/decision_context.rs`
(`b_leg_that_draws_nothing_fails_at_the_first_response_bound_as_response`),
`b2bua-harness/tests/it/timed_out_invite_cancel.rs`.
