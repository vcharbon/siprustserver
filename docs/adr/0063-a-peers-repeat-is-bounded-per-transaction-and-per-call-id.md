# A peer's repeat is answered on the RFC schedule, and every Call-ID has a message budget

**Status:** accepted

**Owner:** `crates/sip-txn` (the per-transaction bound, the Call-ID budget and its mute),
`crates/b2bua-core` (the call layer's re-ACK of a copy), `crates/b2bua` (the router's stateless
answers and the ingress brake's reply, counted on the budget), `crates/sip-proxy` (its refusal
memo). The inventory of reactive emissions behind this decision is in the
[appendix](appendix/0063-a-peers-repeat-is-bounded-per-transaction-and-per-call-id.md).

Amends ADR-0057: §10 is extended from 2xx copies to every reactive re-send (§1 below), and the
Call-ID budget is a peer cap whose trip mutes, where §4's trip fails a turn.

## Context

A reactive emission is a message the node sends because a peer's message arrived: the last
response replayed for a request copy, a hop ACK for a non-2xx final copy, the 200 to a CANCEL
copy, a stateless 481. Each step is compliant on its own. Against a peer that reacts to the
emission with its trigger again, the pair loops at wire speed.

The inventory finds that loop in the transaction layer, the call layer and the router:

- **Transaction layer.** No transaction limits how often it answers a copy. Only its lifetime
  timer (32 s for H, J, L, D and M; up to 193 s for a ringing INVITE) ends the loop. An
  orphaned client transaction re-ACKs every 2xx copy, outside §10's bound, because §10's count
  is on the call record.
- **Router.** A stray CANCEL draws a stateless 481 per copy. A final for a call this node does
  not hold draws an ACK, and for a 2xx also a new BYE transaction, per copy. Nothing bounds
  either, and both outlive the call.
- **Call layer.** The re-ACK of a non-2xx copy has no per-path bound.

The existing limits do not stop these loops:

- The per-turn fuel bounds one turn.
- The per-interval message cap and the lifetime cap count the events the call layer sees. They
  miss what the transaction layer absorbs and what the node sends.
- Both caps tear the call down and stop nothing: the rules that answer copies run on a
  `Terminated` leg (RFC 5407), and the stateless answers need no call.

## Decision

### 1. A transaction answers a peer's copy on the RFC schedule

Every reactive re-send of a transaction follows §10's rule. Copies up to the class's re-sends
(`Class::resends()`) are answered at any pace. A later copy is answered only when it arrives at
least T1 after the last answered one, and the copies between are absorbed. The rule covers:

- the last response replayed for a request copy, in every server state, RFC 6026 Accepted
  included;
- the 200 to a CANCEL copy;
- the hop ACK to a non-2xx final copy (RFC 3261 §17.1.1.3);
- an orphaned transaction's re-ACK of a 2xx copy;
- the refusal replayed for an INVITE copy, in the node's and the proxy's refusal memos.

A stray CANCEL is answered through its own non-INVITE server transaction, so its copies fall
under the same rule.

RFC 3261 §17.2.1 and §17.2.2 ask a server transaction to re-send its response on each
retransmission of the request. A compliant client spaces its retransmissions at least T1 apart
(Timers A and E start at T1), so it is always served. A copy past the count that arrives faster
is not a retransmission on that schedule, as §10 reads §13.2.2.4. The count and the last
answered instant live on the transaction, in memory, as the layer's whole state does.

### 2. The call layer re-ACKs a copy through one bound

The action that re-sends a retained ACK applies §10 itself. This covers every rule that answers
a final copy (the 2xx and the non-2xx alike), and any rule added later. A final for a call this
node does not hold sends its BYE once per dialog: the router keeps a book of those dialogs for
64·T1, as it does for late PRACKs.

### 3. Every message counts against its Call-ID's budget

The transaction layer's owner counts every datagram it receives and every one it sends under
that message's Call-ID. Retransmissions, absorbed copies, stateless answers, the call layer's
sends and the ingress brake's reply all count. A raw send carries its Call-ID.

- **Key.** The key is the Call-ID, not the call reference. Every message carries a Call-ID, and
  it outlives the call's removal, so the stateless phase counts on the same entry. A dialled
  leg has its own Call-ID (ADR-0059), so the budget is per leg dialog: a looping leg trips
  alone, and the call's other legs still carry its teardown.
- **Default.** `B2BUA_CAP_MESSAGES_PER_CALL_ID` is 200 000. A compliant 24-hour call exchanges
  a few thousand messages. Startup refuses 0, and refuses a budget under twice
  `max_messages_per_call_lifetime`, so a busy compliant call meets the lifetime cap's teardown
  first.
- **Entry lifetime.** An entry lives while a transaction of its Call-ID lives, and for 64·T1
  after its last message. The map is bounded. At the bound, the entry idle longest is evicted,
  and its count starts again.

### 4. A trip mutes the Call-ID; it never tears the call down

Past the budget, the owner sends nothing for that Call-ID and drops what arrives for it before
dispatch. Its transactions stay resident and their timers run: deleting them would let an
INVITE copy be judged a new INVITE. The mute lasts until 64·T1 after the last dropped message.

The trip ends nothing by itself. The call's own timers end it:

- an unanswered keepalive toward the muted leg ends the call through Timer F;
- during setup, the setup deadline ends it;
- a terminating call ends at `TerminatingTimeout`.

The call writes its CDR and releases the limiter as usual. Its BYE toward the muted leg never
leaves: that peer's traffic crossed the budget.

### 5. The count is not replicated

The count and the mute live in the node's transaction layer, which replicates nothing
(ADR-0055). A takeover node counts from zero. A looping peer therefore gets at most the budget
times (takeovers + 1), two or three budgets in practice, before it is muted everywhere it
loops.

### 6. Detection

- `b2bua_cap_trips_total{cap="messages_per_call_id", kind="peer"}` counts trips, and a peer cap
  raises no alert (ADR-0057 §7). The trip logs the Call-ID once.
- `sip_txn_copies_past_schedule_total{kind}` counts the copies §1 absorbs.
- `sip_txn_muted_messages_total{direction}` counts what a mute drops.

## Consequences

- No loop outlives one budget per node. The per-path bounds (§1, §2) slow every known loop to
  one answer per T1 well before the budget is reached.
- A loop that has no per-path bound, or one added later, still stops at the budget, after the
  call's removal included.
- The owner holds one entry per live Call-ID, plus a 64·T1 tail.
- A misbehaving peer that is muted keeps its call until the call's timers end it, billed as
  usual.

## Considered and rejected

- **Replicating the count:** a backstop that tolerates two or three budgets needs no merge rule.
- **Tearing the call down on a trip:** the teardown itself sends toward the looping peer, and
  the lifetime cap already tears a busy call down first.
- **One counter per call, across its legs:** the transaction layer does not know the call for
  every message (the caller's initial INVITE transaction has no call reference, and a detached
  transaction loses it). Muting per leg keeps the healthy party's teardown.
- **Counting in the call record:** it never sees absorbed copies, and it is gone in the
  stateless phase.
- **Muting by deleting the Call-ID's transactions:** INVITE copies would be judged as new
  INVITEs.

## Out of scope

A peer answering each refusal with a fresh request is a 1:1 flood, not an amplification. The
ingress brake, the parse-refusal bucket and the out-of-dialog OPTIONS lane bound it. Every such
request still counts on its Call-ID's budget.
