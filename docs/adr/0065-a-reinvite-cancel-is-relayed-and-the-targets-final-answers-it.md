# A re-INVITE's CANCEL is relayed, and the target's final answers its sender

**Status:** accepted

**Owner:** `crates/sip-txn/src/layer/server_cancel.rs` and `held_cancel.rs`
(`consumer_ends_cancelled_reinvite`, `CancelledInvite`), `crates/sip-txn/src/layer/client_give_up.rs`
(`bound_after_cancel`); `crates/b2bua/src/router/commit/held.rs` (`HeldCancel::release`);
`crates/b2bua-core/src/rules/actions/reinvite_cancel.rs` and the `handle-reinvite-cancel` rule;
`crates/call/src/model/request_record.rs` (`cancel_relayed`); `crates/rfc-rules/src/rules/cancel/relayed_answer.rs`.

## Context

A party CANCELs a re-INVITE this stack relayed. RFC 3261 §9.2 has a UAS "immediately respond to
the INVITE with a 487". The transaction layer did, before the CANCEL reached the target. But the
target's 2xx may already be on its way (§9.2: a CANCEL reaching a UAS after its final has no
effect). The sender then holds a 487 and the old session, while the target holds a 2xx and the
new one. When the re-INVITE carried no offer, that 2xx makes one, and its ACK MUST carry the
answer (§13.2.2.4), which only the sender can give.

Weighed: answering the target from the session it already holds (new state per dialog, the two
views still apart), rejecting every stream and ending the call (literal §13.2.2.4, a call lost to
a race), and letting the target's final decide the sender's, as a proxy does (§16.10).

## Decision

### §1 The target's final answers the sender

A CANCEL of an in-dialog INVITE is answered 200 by the transaction layer, which leaves the INVITE
open: the consumer asked for it (`consumer_ends_cancelled_reinvite`). The B2BUA relays the CANCEL
onto the relayed re-INVITE (`cancel_relayed`) and relays the target's final, a 487 or a 2xx that
crossed the CANCEL, with the provisionals and PRACKs before it, as for any relayed re-INVITE. On a
2xx the sender ACKs with her own answer. Both sides end in one session, as end to end. This
departs from the §9.2 SHOULD for this one case; the initial INVITE's CANCEL keeps the immediate
487.

### §2 The layer ends what the consumer does not keep

The layer holds the CANCEL's 200 and the 487 it would answer, at the write path's cancel bound or
the §17.2.1 bound where that is zero. The turn that took the CANCEL releases them: `Keep` when its
record holds the relay of that INVITE marked `cancel_relayed`, `End` otherwise (no relay pending,
the CANCEL not built, no turn took the event). The bound ends the INVITE with the 487 too, so a
CANCEL no consumer acts on still draws today's answer. A copy of the CANCEL draws its 200 again and
never reaches the consumer.

### §3 A target that never answers

A CANCEL leaving on an INVITE client transaction in Proceeding brings its give-up within 64·T1
(§9.1). The relayed re-INVITE's timeout then ends the target's dialog (§12.2.1.2) and the call:
the sender takes the teardown's 487 (§15.1.2).

### §4 A teardown meanwhile

A BYE between the relayed CANCEL and the target's final answers the sender's re-INVITE 487
(§15.1.2) without a second CANCEL. The target's late 2xx then resolves locally: ACKed, an offer it
made answered with every stream rejected (RFC 3264 §6).

### §5 The audit

`no-200-after-cancel` judges a UAS that took a CANCEL. An endpoint that relayed the INVITE and the
CANCEL onward, or held that CANCEL because its far UAS answered before any provisional (§9.1), and
answers with the far 2xx it took, is compliant: it relays. The far UAS is judged on its own
occasion.

## Consequences

- The sender of a CANCELled re-INVITE waits for the target's final, at most 64·T1.
- A 2xx crossing the CANCEL renegotiates the session on both sides.
