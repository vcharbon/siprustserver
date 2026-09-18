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

`cancel_txns_for_call` **orphans** every client transaction of the released
call and deletes none. An orphan keeps every timer it was running and closes
its own obligations with no consumer:

- **Non-2xx final** — the hop ACK on the INVITE's branch (§17.1.1.3), then the
  Completed hold for **Timer D** (§17.1.1.2, 64·T1) re-ACKing each repeat; the
  final is surfaced to nobody.
- **2xx final** — the layer sends the bare ACK itself, on a fresh branch
  (§17.1.1.3: the 2xx ACK is its own transaction), built from the INVITE and
  the 2xx alone (`generate_ack_for_2xx_from_invite`), and holds the
  transaction in **Accepted** for **Timer M** (RFC 6026 §7.2, 64·T1),
  re-passing the same ACK to each repeat (§13.2.2.4). The ACK carries no
  body: the session it would describe was released by the BYE, so no answer
  is owed to anything living (RFC 3264 §4 places the answer in the ACK of an
  offerless INVITE; an orphaned offerless INVITE gets the bare ACK all the
  same, which closes the transaction and leaves the callee nothing to repeat).
  A seeded transaction retains no request and is deleted at its 2xx, ACK-less.
- **Nothing** — Timer A keeps repeating an INVITE in Calling and the held
  CANCEL keeps its §9.1 wait or its Timer E ladder; **Timer B** (or the
  configured INVITE bound past a provisional) purges the transaction with a
  `Timeout` naming no call, which the router drops.
- **Non-INVITE** — Timer E to its final or Timer F, as before.

Server transactions are not touched, as before: their Timer H / J / L holds
are the retransmit-absorption windows of §17.2.1 / §17.2.2 and the UAS-side
mirror of this decision (a final the B2BUA sent to a re-INVITE it received is
repeated on Timer G and held to Timer H whatever became of the call).

The call attribution is dropped at once: `has_txns_for`, `ActiveTxnCount` and
the ADR-0014 `CallQuiesced` timing see the call as quiesced. A never-sent held
CANCEL is put on the wire at the release under the bounded policy (ADR-0028:
the courtesy wait ends with the call) and rides its ladder from there; under
the strict policy it stays parked on the orphan and leaves on the branch's
first provisional, as for a live call.

The metrics name the state: `txn_orphaned_on_call_evict` counts orphans made,
`orphaned_transactions` gauges those resident.

## Retention

The earliest moment an orphan may be purged is its own RFC timer, measured
from the event that arms it; the layer purges at exactly that moment and the
safety-net sweep (35 s) sits just above each window:

| Orphan's fate | Held until | From |
|---|---|---|
| non-2xx final taken | Timer D = 64·T1 (≥ 32 s on UDP, §17.1.1.2) | the first final |
| 2xx final taken | Timer M = 64·T1 (RFC 6026 §7.2, §8.4; matches the sender's §13.3.1.4 repeat bound) | the first 2xx |
| no final, no provisional | Timer B = 64·T1 (§17.1.1.2) | the INVITE |
| no final, a provisional | the configured INVITE bound (`invite_initial_timeout_ms`; §17.1.1.2 has no client timer in Proceeding — the bound is deployment policy) | the first provisional |
| non-INVITE, no final | Timer F = 64·T1 (§17.1.2.2) | the request |
| CANCEL that draws nothing | its INVITE transaction's bound above; the CANCEL's own Timer E ladder stops at 64·T1 (§17.1.2.2) | the CANCEL |

## Consequences

- A call torn down mid-transaction leaves no peer laddering a final to Timer
  H, and no dead-dialog BYE from a callee whose 2xx was never ACKed.
- An orphaned INVITE in Calling keeps repeating on Timer A after the call is
  gone: the request may not have reached the peer, and a peer that gets it
  late answers it (a 481 for a dead dialog, which the orphan ACKs). This is the
  §17.1.1.2 ladder, bounded by Timer B.
- A `Timeout` for an orphan names no call; the router's per-peer failure
  accounting still sees the destination, as it does for a detached non-INVITE.
- Orphan finals never reach the consumer, so a rule can no longer observe
  them; the layer's ACK is the whole response. The consumer-side rules for a
  crossing 2xx on a resident call (`resolve-cancelled-reinvite-response`) are
  unchanged.
- The retention windows above are the invariant the paused-clock tests pin
  (`crates/b2bua-harness/tests/reinvite_final_after_teardown_is_acked.rs`,
  `crates/sip-txn/tests/cancel_on_evict.rs`).
