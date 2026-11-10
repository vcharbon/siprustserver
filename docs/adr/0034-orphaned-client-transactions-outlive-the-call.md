# A released call orphans its client transactions; each runs out its own timers

**Status:** accepted (2026-09-18)

The wire scenario behind the decision, the long form of the orphaned 2xx, and the tests:
[appendix](appendix/0034-orphaned-client-transactions-outlive-the-call.md).

## Context

`cancel_txns_for_call` runs from the one teardown executor (`release_call`) when a call's
per-node state is freed. A client transaction deleted there shows up on the wire: a final
that arrives on a branch no transaction holds draws no ACK, so the peer repeats it to Timer H,
and a callee whose 2xx was never ACKed BYEs a dialog that is already gone. Detail:
[appendix, context](appendix/0034-orphaned-client-transactions-outlive-the-call.md#context).

The obligation belongs to the transaction, not the dialog. §17.1.1.3 has the
client transaction ACK every non-2xx final on the INVITE's branch; §13.2.2.4
has the UAC core ACK every 2xx, "including a 2xx that beat a CANCEL" (§9.1);
RFC 5407 §2 and Appendix D keep the invite usage past the BYE for exactly the
ACK. The B2BUA already keeps a Mortal leg for the relayed 2xx ACK while the
call is resident; this decision covers the transaction once the call is not.

## Decision

Every INVITE client transaction that takes a 2xx is held in **Accepted** for
**Timer M** (RFC 6026 §7.2, 64·T1 from the first 2xx), its call or not. Each
2xx's ACK has one owner (RFC 3261 §13.2.2.4):

- **While the call holds the transaction**, the TU. The first 2xx is the
  transaction's final; every later 2xx on the branch, a repeat or a fork's,
  reaches the TU as a repeat matched to no transaction, as it would with
  none, and the layer sends nothing for it; a 1xx or a non-2xx on the
  branch is absorbed (RFC 6026 §8.4). The TU's ACK leaves through the layer
  (`send_request`, raw), which records it and its destination on the dialog
  it names (the INVITE's Call-ID, From-tag and CSeq number, the ACK's
  To-tag); a BYE the TU sends on a dialog marks that dialog ended.
- **Once the call releases it**, the layer, as for any orphan below.

An Accepted transaction is off its call's books: the 2xx met its obligation
to the call, so `has_txns_for`, `ActiveTxnCount` and the ADR-0014
`CallQuiesced` timing see it as gone, while `cancel_txns_for_call` orphans it
and `relinquish_call` deletes it. A release that does not end the call (a
takeover copy shed at quiescence while the call lives on at its primary)
relinquishes the call's Accepted transactions first
(`relinquish_accepted_of_call`): their dialogs are the primary's, so none
may draw this layer's ACK or BYE; a later repeat is a new arrival.

`cancel_txns_for_call` **orphans** every client transaction of the released
call and deletes none. An orphan keeps every timer it was running and closes
its own obligations with no consumer:

- **Non-2xx final**: the hop ACK on the INVITE's branch (§17.1.1.3), then the Completed hold
  for **Timer D** (§17.1.1.2, 64·T1) re-ACKing each repeat. The final is surfaced to nobody.
- **2xx final**: Accepted for Timer M from the first 2xx, and each dialog's ACK sent by the
  layer, the same datagram for every repeat (§13.2.2.4). A dialog whose ACK the TU sent gets
  that ACK again and nothing else. A dialog no ACK left for gets an ACK built from the INVITE
  and the 2xx alone (`generate_ack_for_2xx_from_invite`). A 2xx to an initial INVITE created a
  dialog nothing holds: unless a BYE already left on it, the layer sends its BYE
  (`generate_bye_for_2xx_from_invite`). A transaction remembers 16 dialogs; a new one past
  them is ACKed each time and not BYEd. Detail:
  [appendix, 2xx final](appendix/0034-orphaned-client-transactions-outlive-the-call.md#an-orphans-2xx-final).

- **A 2xx on no branch this node sent** matches no transaction and draws
  nothing.
- **Nothing**: Timer A keeps repeating an INVITE in Calling and the held
  CANCEL keeps its §9.1 wait or its Timer E ladder; **Timer B** (or the
  configured INVITE bound past a provisional) gives the transaction up with a
  `Timeout` naming no call, which the router drops, and 64·T1 later (the
  §9.1 wait for the final its CANCEL provokes, ADR-0043) it is purged.
- **Non-INVITE**: Timer E to its final or Timer F.

Server transactions are not touched: their Timer H / J / L holds are the
retransmit-absorption windows of §17.2.1 / §17.2.2 and the UAS-side mirror of this decision
(a final the B2BUA sent to a re-INVITE it received is repeated on Timer G and held to Timer H
whatever became of the call).

The call attribution is dropped at once: `has_txns_for`, `ActiveTxnCount` and
the ADR-0014 `CallQuiesced` timing see the call as quiesced. A never-sent held
CANCEL stays parked on the orphan and leaves on the branch's first
provisional, as for a live call (ADR-0043).

The metrics name the state: `txn_orphaned_on_call_evict` counts orphans made,
`orphaned_transactions` gauges those resident.

## Retention

The earliest moment an orphan may be purged is its own RFC timer, measured
from the event that arms it; the layer purges at exactly that moment, the
transaction's one lifetime deadline (`sip-txn` `layer::lifetime`), which the
safety-net sweep only re-reads:

| Orphan's fate | Held until | From |
|---|---|---|
| non-2xx final taken | Timer D = 64·T1 (≥ 32 s on UDP, §17.1.1.2) | the first final |
| 2xx final taken, before or after the release | Timer M = 64·T1 (RFC 6026 §7.2, §8.4; matches the sender's §13.3.1.4 repeat bound) | the first 2xx |
| no final, no provisional | Timer B = 64·T1 (§17.1.1.2), then 64·T1 (§9.1) | the INVITE |
| no final, a provisional | the configured INVITE bound (`invite_initial_timeout_ms`; §17.1.1.2 has no client timer in Proceeding, so the bound is deployment policy; armed at the first provisional), then 64·T1 (§9.1) | the INVITE |
| non-INVITE, no final | Timer F = 64·T1 (§17.1.2.2) | the request |
| CANCEL that draws nothing | its INVITE transaction's bound above; the CANCEL's own Timer E ladder stops at 64·T1 (§17.1.2.2) | the CANCEL |

## Consequences

- A call torn down mid-transaction leaves no peer laddering a final to Timer H, and no
  dead-dialog BYE from a callee whose 2xx was never ACKed.
- Every answered INVITE holds a client transaction for 64·T1 past its 2xx, counted by the
  transaction table and a transaction ceiling (ADR-0037).
- A takeover (ADR-0014) seeds no Accepted client transaction.
- An orphaned INVITE in Calling keeps repeating on Timer A after the call is gone, stopped at
  Timer B.
- A `Timeout` for an orphan names no call. It counts on
  `b2bua_unroutable_internal_total{event="timeout"}`.
- Orphan finals never reach the consumer: the layer's ACK is the whole response.

Detail and tests: [appendix, consequences](appendix/0034-orphaned-client-transactions-outlive-the-call.md#consequences).
