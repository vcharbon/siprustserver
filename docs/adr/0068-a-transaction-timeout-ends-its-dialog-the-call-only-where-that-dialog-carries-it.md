# A transaction timeout ends its dialog; the call only where that dialog carries it

**Status:** accepted

**Owner:** `crates/b2bua-core/src/rules/defaults/request_timeouts.rs` (`handle-timeout`),
`crates/call/src/helpers/peering.rs` (`carries_call`).

## Context

RFC 3261 §12.2.1.2: a 408, a 481 or no response at all to a request within a dialog means the
UAC SHOULD terminate the dialog; §14.1 says the same of a re-INVITE. RFC 5057 §5 narrows the
effect to the usage the request belongs to: a NOTIFY's failure ends its subscription, not the
INVITE dialog. These rules are stated for one dialog. A back-to-back call holds several: the
caller's, the one it is bridged to, and the dialogs services open beside them (a media
server's, a transfer target's before it is bridged). A default that ends the call on any
dialog's timeout ends a working conversation because a side dialog went silent.

## Decision

### §1 The legs a call is carried on

`call::helpers::carries_call` names them from the replicated record alone: the originator's
leg, and the leg it is bridged to now (its live `active_peer`). While the originator's leg is
bridged to no live leg (the setup, or the bridged peer went away), the adopted b-legs dialled
for it carry the call. A media leg, a leg dialled beside the bridge before its merge, and a leg
going away carry nothing.

### §2 The default reads the timeout by method, then by leg

A timeout terminates what its request belongs to: a PRACK's transaction; a NOTIFY's,
SUBSCRIBE's or REFER's subscription usage; any other request's dialog. A dialog that carries
the call ends the call (BYE on every leg). Any other dialog is ended alone (`DestroyLeg`: BYE,
or CANCEL while early), and the call stands. The service that owns that leg reads the timeout
first, its rule outranking the default, or hears the leg's `LegEnded` after. A pending leg's
dialling INVITE keeps the setup's failure reading. A leg already going away is resolved where
it stands. The full table is the module doc of `request_timeouts.rs`.

### §3 A relayed request reads the same

A relayed request that times out is read by the same table as one this stack sent: the
B2BUA's leg is the UAC that got no answer. The originator is sent no 408 (RFC 4320 §4.2); on a
carrying dialog it is BYE'd with the rest of the call.

### §4 Keeping the call after a carrying re-INVITE's timeout is a stated departure

A service rule may claim the Timer B of a re-INVITE it sent (`RuleContext::timed_out_record`)
and keep the call. On a carrying leg that departs from §14.1, and the service's doc states its
ground. No built-in service does it.

## Consequences

- A media server, an unbridged transfer target or any service leg that stops answering costs
  that leg, not the call. A service that relied on the default ending its call on such a
  timeout claims the timeout itself (the announcement's play INFO, the REFER transfer's
  c-realign re-INVITE).
- A relayed in-dialog request a carrying peer never answers ends the call at the relay's
  Timer F, about when the originator's own Timer F would make it BYE.
