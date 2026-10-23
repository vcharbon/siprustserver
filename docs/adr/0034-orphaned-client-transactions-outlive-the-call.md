# A released call orphans its client transactions; each runs out its own timers

**Status:** accepted (2026-09-18)

## Context

`cancel_txns_for_call` runs from the one teardown executor (`release_call`)
when a call's per-node state is freed. It used to delete an INVITE client
transaction still awaiting its final, on the argument that Timer B was the
call's own failure-detection deadline and teardown meant giving up now. A
Completed INVITE (Timer D) and an active non-INVITE were already spared —
detached from the call and left to finish — because deleting them had shown
up on the wire (a rejected leg never re-ACKed, a lost NOTIFY never re-sent).

The remaining deletion shows up the same way. A relayed re-INVITE is pending
at the callee when the caller BYEs; the B2BUA answers the BYE, 487s the
caller's INVITE (RFC 3261 §15.1.2), CANCELs the relayed INVITE and relays the
BYE; the callee answers both and the call is reaped. The callee's own final to
the re-INVITE — a 487 for the CANCEL, or a 200 that beat it — arrives on a
call that no longer exists and a branch no transaction holds. Nothing ACKs
it. RFC 3261 §17.2.1 then keeps the callee's INVITE server transaction in
Completed repeating the final on Timer G until Timer H (64·T1); for a 2xx,
§13.3.1.4 repeats it to 64·T1 and §14.2 has the callee BYE a dialog that is
already gone.

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
and `relinquish_call` deletes it. A release that does not end the call — a
takeover copy shed at quiescence while the call lives on at its primary —
relinquishes the call's Accepted transactions first
(`relinquish_accepted_of_call`): their dialogs are the primary's, so none
may draw this layer's ACK or BYE; a later repeat is a new arrival.

`cancel_txns_for_call` **orphans** every client transaction of the released
call and deletes none. An orphan keeps every timer it was running and closes
its own obligations with no consumer:

- **Non-2xx final** — the hop ACK on the INVITE's branch (§17.1.1.3), then the
  Completed hold for **Timer D** (§17.1.1.2, 64·T1) re-ACKing each repeat; the
  final is surfaced to nobody.
- **2xx final** — Accepted for Timer M from the first 2xx, and each dialog's
  ACK sent by the layer, the same datagram for every repeat (§13.2.2.4). A
  dialog whose ACK the TU sent gets that ACK again, where the TU sent it, and
  nothing else: the TU held that dialog and its release ended it. A dialog
  no ACK left for (a 2xx after the release, a fork's, one the TU never
  acknowledged) gets an ACK built from the INVITE and the 2xx alone on a
  fresh branch (`generate_ack_for_2xx_from_invite`: the INVITE's route set or
  the 2xx's, this node's own Via), sent to its next hop (§8.1.2, §12.2.1.1):
  the first Route when loose, else the Request-URI, the 2xx's Contact; a hop
  naming no address draws nothing, since the layer resolves no host name.
  When the 2xx carries the offer of an offerless INVITE
  (RFC 3264 §4) and no reliable provisional of that dialog carried it (RFC
  3262 §5: its PRACK answered it), the ACK answers it rejecting every stream
  (RFC 3264 §6, port 0), the least a dialog about to be released can commit
  to. A 2xx to an initial INVITE created a dialog nothing holds: unless a
  BYE already left on it (the TU hung up before any ACK), the layer then
  sends its BYE (§15.1.1, `generate_bye_for_2xx_from_invite`), numbered
  above every request this layer sent on that dialog, to the same next hop,
  on an orphaned non-INVITE transaction of its own, whose final reaches no
  consumer. A
  transaction remembers 16 dialogs; a new one past them is ACKed each time
  and not BYEd.
- **A 2xx on no branch this node sent** matches no transaction and draws
  nothing.
- **Nothing** — Timer A keeps repeating an INVITE in Calling and the held
  CANCEL keeps its §9.1 wait or its Timer E ladder; **Timer B** (or the
  configured INVITE bound past a provisional) gives the transaction up with a
  `Timeout` naming no call, which the router drops, and 64·T1 later (the
  §9.1 wait for the final its CANCEL provokes, ADR-0043) it is purged.
- **Non-INVITE** — Timer E to its final or Timer F, as before.

Server transactions are not touched, as before: their Timer H / J / L holds
are the retransmit-absorption windows of §17.2.1 / §17.2.2 and the UAS-side
mirror of this decision (a final the B2BUA sent to a re-INVITE it received is
repeated on Timer G and held to Timer H whatever became of the call).

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
| no final, a provisional | the configured INVITE bound (`invite_initial_timeout_ms`; §17.1.1.2 has no client timer in Proceeding — the bound is deployment policy; armed at the first provisional), then 64·T1 (§9.1) | the INVITE |
| non-INVITE, no final | Timer F = 64·T1 (§17.1.2.2) | the request |
| CANCEL that draws nothing | its INVITE transaction's bound above; the CANCEL's own Timer E ladder stops at 64·T1 (§17.1.2.2) | the CANCEL |

## Consequences

- A call torn down mid-transaction leaves no peer laddering a final to Timer
  H, and no dead-dialog BYE from a callee whose 2xx was never ACKed — nor
  from one whose ACK was lost after the call ended, or whose 2xx the call
  consumed and died on.
- Every answered INVITE holds a client transaction for 64·T1 past its 2xx:
  the transaction table, and a transaction ceiling (ADR-0037), count it. It
  keeps the INVITE and the ACKs of its dialogs; the INVITE's retransmit image
  goes at the 2xx.
- A takeover (ADR-0014) seeds no Accepted client transaction: while the call
  lives, the record's `ack_branch` lets the takeover copy's rules re-ACK a
  repeat; a repeat reaching a node that holds neither the call nor the
  transaction is dropped there.
- An orphaned INVITE in Calling keeps repeating on Timer A after the call is
  gone: the request may not have reached the peer, and a peer that gets it
  late answers it (a 481 for a dead dialog, which the orphan ACKs). This is the
  §17.1.1.2 ladder, stopped at Timer B.
- A `Timeout` for an orphan names no call; the router's per-peer failure
  accounting still sees the destination, as it does for a detached non-INVITE.
  It counts on `b2bua_unroutable_internal_total{event="timeout"}`, apart from
  the wire messages dropped (`router::unroutable`).
- Orphan finals never reach the consumer, so a rule can no longer observe
  them; the layer's ACK is the whole response. The consumer-side rules for a
  crossing 2xx on a resident call (`resolve-cancelled-reinvite-response`) are
  unchanged.
- The retention windows above are the invariant the paused-clock tests pin
  (`crates/b2bua-harness/tests/it/reinvite_final_after_teardown_is_acked.rs`,
  `crates/sip-txn/tests/it/cancel_on_evict.rs`); the ACK owner rule is pinned
  by `crates/sip-txn/tests/it/accepted_client.rs` and
  `crates/b2bua-harness/tests/it/two_xx_after_release.rs`.
