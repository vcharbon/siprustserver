# Appendix: A released call orphans its client transactions; each runs out its own timers

The decision part: [ADR-0034](../0034-orphaned-client-transactions-outlive-the-call.md).

## Context

A deleted INVITE client transaction shows up on the wire. A relayed re-INVITE is pending
at the callee when the caller BYEs; the B2BUA answers the BYE, 487s the
caller's INVITE (RFC 3261 §15.1.2), CANCELs the relayed INVITE and relays the
BYE; the callee answers both and the call is reaped. The callee's own final to
the re-INVITE — a 487 for the CANCEL, or a 200 that beat it — arrives on a
call that no longer exists and a branch no transaction holds. Nothing ACKs
it. RFC 3261 §17.2.1 then keeps the callee's INVITE server transaction in
Completed repeating the final on Timer G until Timer H (64·T1); for a 2xx,
§13.3.1.4 repeats it to 64·T1 and §14.2 has the callee BYE a dialog that is
already gone.

## An orphan's 2xx final

Accepted for Timer M from the first 2xx, and each dialog's
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
