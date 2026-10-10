# A dialog is named by its two tags and carries its own state

**Status:** accepted (2026-10-10)

**Owner:** `crates/call/src/model/dialog.rs` and `dialogs.rs`; the engine side is `crates/b2bua/src/router/process.rs`.

## Context

A back-to-back UA leg holds several dialogs at once: a leg the stack dialled
holds one per callee fork (RFC 3261 §12.1.2), the caller's leg one per tag the
stack showed her. Which dialog a message belongs to was read differently per
leg role, by position ("the first dialog"), and through side tables (a map
from caller-facing tags to callee forks, slots for dialogs the answer let go
of). A positional read picks a wrong dialog as soon as a leg holds two.

## Decision

1. **A dialog's identity is the leg's Call-ID plus `our_tag` and
   `peer_tag`** (§12). A received request names `(our_tag, peer_tag) = (To-tag,
   From-tag)`, a response the reverse. Lookup goes through one surface
   (`Dialogs`: `by_tags`, `of_request`, `of_response`, `confirmed`,
   `fronting`); no lookup depends on the leg's role and none returns "the
   first" dialog.
2. **A dialled leg's dialog exists once a tagged response creates it.** The
   INVITE the leg sent is a leg record (`OutboundInvite`: its From-tag and
   CSeq); each fork's dialog is minted from it, its own sequence starting at
   the INVITE's. A caller-facing dialog exists once the stack mints its tag.
3. **A dialog carries its state**: `Early`, `Confirmed`, `Ending` (our BYE
   in flight), `Ended(Rejected | PeerBye | OurBye)` and `Retired` — an early
   dialog the call let go of without a BYE (a losing fork, a caller-facing
   dialog the answer did not take), holding the relays still open in it and
   the ACK/BYE that released its late 2xx (§13.2.2.4). No record is removed
   before the call is: a late message is answered by what its dialog was.
4. **A caller-facing dialog states the callee dialogs it relays**
   (`fronts`): one, or several a masking policy shows the caller as one.
   Answering the caller confirms the dialog the 2xx names and retires the
   others; no dialog is renamed.
5. **The engine resolves a request's dialog before any rule** and refuses
   what names none (481, §12.2.2), so a rule reads the dialog it was handed
   (`RuleContext::dialog`) and never looks one up again.

## Consequences

- A request on a retired or ended dialog is told apart from one on a dialog
  never held, without a side table.
- Every turn can check the model whole: each dialog reachable by one pair of
  tags, at most one confirmed dialog per leg, every `fronts` naming a dialog
  the callee leg holds.
- Decision 5 lands after decisions 1–4: until then the router's foreign-dialog
  refusal reads the same records.
