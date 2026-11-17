# A peer's repeat is answered on the RFC schedule, and every Call-ID has a message budget

**Status:** accepted

**Owner:** `crates/sip-txn` (the per-trigger bound, the Call-ID budget and its mute),
`crates/b2bua-core` (the re-ACK of a copy, the budget's verdict), `crates/b2bua` (the router's
answers to unheld finals), `crates/sip-proxy` (its refusal memo). The inventory, the RFC
reading and the sizing are in the
[appendix](appendix/0063-a-peers-repeat-is-bounded-per-transaction-and-per-call-id.md).

Amends ADR-0057: §10's rule covers every reactive re-send (§1, §2), and the budget's trip
mutes before it ends the call (§4).

## Context

A reactive emission is a message the node sends because a peer's message arrived. Each step is
compliant on its own, but against a peer that answers the emission with its trigger again, the
pair loops at wire speed. The inventory finds such loops in every layer:

- **Transaction layer.** A transaction answers every copy until its lifetime timer ends it (32
  s, or up to 193 s for a ringing INVITE).
- **Router.** A stray CANCEL and an unheld final are answered per copy, with no bound, after
  the call's removal included.
- **Call layer.** The re-ACK of a non-2xx copy has no bound.

The per-turn fuel bounds one turn. The per-interval and lifetime caps count only what the call
layer sees, and they tear the call down without stopping the rules that answer copies on a
`Terminated` leg (RFC 5407).

## Decision

### 1. A transaction answers a peer's copy on the RFC schedule

Each reactive re-send follows ADR-0057 §10's rule: copies up to the class's re-sends
(`Class::resends()`) are answered at any pace, and a later copy only at least T1 after the last
answered one. The copies in between are absorbed. The count is per answered trigger:

- per method on a server transaction (the request copy, the CANCEL copy);
- per dialog on a client transaction.

The rule covers:

- the last response replayed for a request copy;
- the 200 to a CANCEL copy;
- the hop ACK to a non-2xx final copy (§17.1.1.3);
- an orphan's re-ACK of a 2xx copy;
- the refusals replayed by the node's and the proxy's memos, the proxy's preflight rejects
  included.

A stray CANCEL is answered through its own non-INVITE server transaction. An Accepted INVITE
absorbs copies (RFC 6026 §7.1), except on a seeded transaction whose 2xx has not yet left. A
compliant client spaces its retransmissions at least T1 apart, so it is always served. The
departures from RFC 3261 §17.1.1.3 and §17.2.2 are read in the appendix.

### 2. One book bounds every re-ACK the call layer sends

The action that re-sends a retained ACK applies §10 itself. A final that matches no transaction
and no leg the call holds (an unheld final, an unknown leg's 2xx) is answered through one router
book. The book is keyed by Call-ID, To-tag and CSeq, and kept for 64·T1 after the entry's last
copy. Its ACK follows §10, and a 2xx's BYE leaves once.

### 3. Every message counts against its Call-ID's budget

The transaction layer's owner counts, under the message's Call-ID:

- every datagram it receives that matches a transaction or draws an answer;
- every datagram it sends, retransmissions and the call layer's sends included. A raw send
  carries its Call-ID.

The out-of-dialog OPTIONS lane, the parse-refusal bucket and the ingress brake's memo have
their own bounds and do not count.

- **Key.** A Call-ID outlives the call, so the stateless phase counts on the same entry. A
  dialled leg has its own Call-ID (ADR-0059), so a looping leg trips alone.
- **Default.** `B2BUA_CAP_MESSAGES_PER_CALL_ID` is 5 000 000. Startup refuses a budget under 44
  times `max_messages_per_call_lifetime`, so a compliant call reaches the lifetime cap first
  (appendix).
- **Entry lifetime.** An entry lives while a transaction of its Call-ID lives, and for 64·T1
  after its last message. A muted entry is never evicted. When the bound is full of mutes, a
  new Call-ID goes uncounted, and `sip_txn_message_budget_uncounted_total` counts it.

### 4. A trip mutes the Call-ID and ends its call

Past the budget, the owner sends nothing for that Call-ID and drops whatever arrives for it
before dispatch. Its transactions stay resident and their timers run: deleting them would let
an INVITE copy be judged a new INVITE.

The owner reports the trip to the call holding the Call-ID. That call ends as the message
cap's call does:

- a `503` to an unanswered caller;
- BYE or CANCEL on its other legs;
- nothing on the muted leg;
- its CDR, with the `MessageBudget` cause.

The mute lasts until that call is released, and in every case until 64·T1 after the last
dropped message. A trip after the call's removal only mutes.

### 5. The count is not replicated

The count and the mute live in the transaction layer, which is in-memory only
(`sip-txn/src/lib.rs`). Every ownership move (a takeover, a reclaim, a restart) counts from
zero. A looping peer therefore gets at most the budget times (moves + 1), two or three budgets
in practice.

### 6. Detection

- `sip_txn_message_budget_trips_total` counts trips, and the trip logs its Call-ID.
- `b2bua_cap_trips_total{cap="messages_per_call_id", kind="peer"}` counts the calls ended. A
  peer cap is a counter, not an alert (ADR-0057 §7).
- `sip_txn_copies_past_schedule_total{kind}` counts what §1 absorbs.
- `sip_txn_muted_messages_total{direction}` counts what a mute drops.

## Consequences

- The per-path bounds (§1, §2) bring every loop in the inventory down to one answer per T1.
- A loop with no per-path bound, or one added later, stops at the budget, after the call's
  removal included.
- A party that knows a Call-ID can spend its budget with in-dialog requests, as it could end
  the call with a forged BYE.

## Considered and rejected

- **Replicating the count:** a backstop that tolerates two or three budgets needs no merge rule.
- **Muting without ending the call:** the mute lifts before a keepalive fails, and the loop
  restarts with a fresh budget.
- **One counter across a call's legs:** the transaction layer does not know the call for every
  transaction. A per-leg mute keeps the healthy party's teardown.
- **Counting in the call record:** it never sees absorbed copies, and it is gone in the
  stateless phase.
- **Muting by deleting the Call-ID's transactions:** INVITE copies would be judged as new.
