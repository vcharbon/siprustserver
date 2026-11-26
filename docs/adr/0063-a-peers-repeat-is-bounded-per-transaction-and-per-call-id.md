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
answered one, and none later than 64·T1 after the trigger's first copy: a compliant client
stops retransmitting by then. The copies in between are absorbed. The count is per trigger,
keyed by the transaction's identity:

- on a server transaction, its top-Via branch and the copy's method (the request, the CANCEL);
- on a client transaction, its branch and the final's To-tag, with the dialogs past its dialog
  bound sharing one count.

A trigger's record lives in its Call-ID's entry (§3) until 64·T1 after its last copy, absorbed
copies included. A copy that re-opens a transaction after its timer continues the count.

The rule covers:

- the last response replayed for a request copy;
- the 200 to a CANCEL copy;
- the hop ACK to a non-2xx final copy (§17.1.1.3);
- an orphan's re-ACK of a 2xx copy;
- the refusals replayed by the node's and the proxy's memos, the proxy's preflight rejects
  included.

A request naming no call, a stray CANCEL included, is outside this rule and answered as a
stateless UAS answers it (RFC 3261 §8.2.7): the rule needs state per trigger, and each copy draws
one answer, never re-sent. An Accepted INVITE
absorbs copies (RFC 6026 §7.1), except on a seeded transaction whose 2xx has not yet left. A
compliant client spaces its retransmissions at least T1 apart, so it is always served. The
departures from RFC 3261 §17.1.1.3 and §17.2.2 are read in the appendix.

### 2. One book bounds every re-ACK the call layer sends

The action that re-sends a retained ACK applies §10 itself. Some finals match no transaction
and no leg the call holds: an unheld final, or an unknown leg's 2xx. Each one is answered
through one book in the shell, keyed by Call-ID, To-tag and CSeq. The router consults it, and
so does the commit that sends the core's unheld answers. Its ACK follows §1's rule, the 64·T1
window included, and a 2xx's BYE leaves once. An entry lives until 64·T1 after its last copy,
absorbed copies included. A turn's answers are booked once on the call's outbox. A full book
(65 536 finals) forgets the half idle longest. A turn the book absorbed whole is quiet.

### 3. Every message counts against its Call-ID's budget

The transaction layer's owner counts, under the message's Call-ID:

- every datagram it receives that matches a transaction or draws an answer;
- every datagram it sends, retransmissions and the call layer's sends included. A raw send
  carries its Call-ID.

The out-of-dialog OPTIONS lane, the parse-refusal bucket and the ingress brake's memo have
their own bounds and do not count.

- **Key.** A Call-ID outlives the call, so the stateless phase counts on the same entry. A
  dialled leg has its own Call-ID (ADR-0059), so a looping leg trips alone.
- **Default.** `B2BUA_CAP_MESSAGES_PER_CALL_ID` is 10 000 000. Startup refuses a budget under
  100 times `max_messages_per_call_lifetime`, so a compliant call reaches the lifetime cap first
  (appendix).
- **Entry lifetime.** An entry lives while a transaction of its Call-ID lives, and for 64·T1
  after its last message. A transaction makes its entry past the bound; a muted entry or
  one with a live transaction is never evicted. A new Call-ID on a full bound replaces the
  idle entry least recently counted, else goes uncounted (`b2bua_txn_message_budget_uncounted_total`).

### 4. A trip mutes the Call-ID and ends its call

Past the budget, the owner sends nothing for that Call-ID and drops whatever arrives for it
before dispatch. Its transactions stay resident and their timers run, so each one reaches its
terminal state. Deleting them would let an INVITE copy be judged a new INVITE.

The trip report carries the Call-ID, the From-tags of the tagless INVITEs received on it and
the call references of its matched transactions, each capped at a few. The router finds,
through its SIP index, every call holding that Call-ID: the a-leg by Call-ID and From-tag, a
b-leg by Call-ID alone. A spiral finds two. Each call ends as the message cap's call does: a `503` to an unanswered
caller, BYE or CANCEL on its legs, and a CDR with the `MessageBudget` cause. The call layer
composes its full teardown, and the owner drops what goes to the muted Call-ID.

The mute lasts until every call found is released, and in every case until 64·T1 after the
last dropped message: the mute holds while a transaction of the Call-ID lives, and each call's
teardown opens one there that outlives its `TerminatingTimeout`. A trip after the calls'
removal only mutes.

### 5. The count is not replicated

The count and the mute live in the transaction layer, which is in-memory only
(`sip-txn/src/lib.rs`). Every ownership move (a takeover, a reclaim, a restart) counts from
zero. A looping peer therefore gets at most the budget times (moves + 1), two or three budgets
in practice.

### 6. Detection

Trips count on `b2bua_txn_message_budget_trips_total`, and the calls ended on
`b2bua_cap_trips_total{cap="messages_per_call_id", kind="peer"}`, a counter, not an alert
(ADR-0057 §7). The families that count absorbed copies and muted messages are listed in the
appendix.

## Consequences

- The per-path bounds (§1, §2) bring every loop in the inventory that repeats one message down
  to one answer per T1, within 64·T1 of its first copy.
- A loop with no per-path bound, or one added later, stops at the budget, after the call's
  removal included.
- A party that knows a Call-ID can spend its budget with in-dialog requests, as it could end
  the call with a forged BYE.
- A call that reuses a muted Call-ID (a retry, ADR-0059) fails while the mute lasts. A mute
  that lifted on a new INVITE would let a peer reset its count with fresh branches.

## Considered and rejected

- **Replicating the count:** a backstop that tolerates two or three budgets needs no merge rule.
- **Muting without ending the call:** the mute lifts before a keepalive fails, and the loop
  restarts with a fresh budget.
- **One counter across a call's legs:** the transaction layer does not know the call for every
  transaction. A per-leg mute keeps the healthy party's teardown.
- **Counting in the call record:** it never sees absorbed copies, and it is gone in the
  stateless phase.
- **Muting by deleting the Call-ID's transactions:** INVITE copies would be judged as new.
