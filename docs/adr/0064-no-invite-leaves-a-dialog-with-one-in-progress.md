# No INVITE leaves a dialog with one in progress; a rule asking for one is a defect

**Status:** accepted

**Owner:** `crates/b2bua-core/src/rules/actions/mod.rs` (`invite_may_leave`), its callers in
`originate.rs` (`send_reinvite`, `send_request_to_leg`) and `relay_request.rs`;
`crates/call/src/helpers/dialog.rs` (`invite_in_progress`).

## Context

RFC 3261 §14.1: *"A UAC MUST NOT initiate a new INVITE transaction within a dialog while
another INVITE transaction is in progress in either direction."* Inbound, the core rule
`reinvite-glare` refuses a peer's INVITE that would be relayed onto such a dialog (§14.2). The
executor itself sent whatever INVITE an action asked for. Two state machines that each ask
for a re-INVITE on one leg in the same turn put both on the wire at once; a peer processing
one of them strands the other's offer and its 2xx un-ACKed.

Two answers were weighed: queue the second INVITE until the first transaction ends, or
refuse it.

## Decision

### §1 What is in progress

An INVITE transaction is in progress on a dialog while any of these holds
(`call::helpers::invite_in_progress`):

- an INVITE this side sent on it, own or relayed, has no 2xx taken: it awaits its final, or
  the ACK of its 2xx is not composed;
- a 2xx this side took has its ACK owed and not sent (RFC 6026 *Accepted*);
- a 2xx this side sent awaits the peer's ACK;
- the peer's INVITE on it has no final from this side: relayed and unanswered, or the
  originator's dialog-creating INVITE.

A non-2xx final, or a timeout, closes this side's INVITE (`close_rejected_invite_round`).

### §2 The INVITE is refused, never queued

A re-INVITE action, an in-dialog INVITE request, or the relay of a peer's INVITE onto such a
dialog is not sent. The turn reports the defect `one-invite-in-progress-per-dialog`
(ADR-0052 §8): its detail names the rule refused, the leg, the dialog, and what holds it,
with the CSeq and the rule that sent this side's INVITE. Production logs it with the call's
dump and counts it in `b2bua_model_defects_total{invariant}`; a harness run fails on it unless
the scenario takes it. A refused relay answers its sender as glare: 491, or 500 where the
sender's own INVITE is unanswered (§14.2).

Queuing lost: an offer built for the dialog as it stood may be stale once the first
transaction ends, the queue would reorder offer/answer behind the rule's back, and it would
hide the rule's error. A rule that needs a second renegotiation waits for the first's
final and ACK, and asks then.

### §3 Every producer waits on its own

The transfer realign and the release reroute ACK the answer that calls for their re-INVITE
earlier in the same action list. The early-media promotion's resync is called for by the
callee's 200, which may overtake the caller's ACK of the promoted 200: the resync then waits
in the promotion's state (`PendingResync`) and leaves on that ACK (`promote-resync-on-a-ack`).
A refusal met later is a rule defect, ticketed on its own.

The transfer realign does not wait for a re-INVITE of the originator's own that is still
relayed and unanswered when its realign is due: that realign is refused, and the transfer
rolls back when its re-INVITE watchdog fires.

## Consequences

- No call is left with two INVITEs crossing on one dialog. A refused re-INVITE leaves its
  treatment where it stood: the transfer and the release reroute end by their watchdogs.
- `send_request_to_leg` with `INVITE` takes the same refusal.
