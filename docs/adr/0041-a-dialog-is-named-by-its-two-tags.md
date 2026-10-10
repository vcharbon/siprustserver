# A dialog is named by its two tags and carries its own state

**Status:** accepted (2026-10-10)

**Owner:** `crates/call/src/model/dialog.rs` and `dialogs.rs`; the engine side is the dialog gate, `crates/b2bua/src/router/dialog_gate.rs`.

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
   INVITE the leg sent is a leg record (`OutboundInvite`: its From-tag, CSeq,
   client transaction, the final it took and whether this stack CANCELled
   it); each fork's dialog is minted from it, its own sequence starting at
   the INVITE's. A caller-facing dialog exists once the stack mints its tag.
3. **A dialog carries its state**: `Early`, `Confirmed`, `Ending` (our BYE
   of it, or our CANCEL of its INVITE, in flight; the INVITE's final ends a
   cancelled one), `Ended(Rejected | PeerBye | OurBye)` and `Retired` — an early
   dialog the call let go of without a BYE (a losing fork, a caller-facing
   dialog the answer did not take, a leg given up before its INVITE's final),
   holding the relays still open in it and
   the ACK/BYE that released its late 2xx (§13.2.2.4). No record is removed
   before the call is: a late message is answered by what its dialog was.
4. **A caller-facing dialog states the callee dialogs it relays**
   (`fronts`): one, or several a masking policy shows the caller as one.
   Answering the caller confirms the dialog the 2xx names and retires the
   others; no dialog is renamed.
5. **The engine resolves every message's dialog before any rule** (the
   dialog gate), so a rule reads what it was handed (`RuleContext::dialog`,
   `RuleContext::relayed_pending`) and never looks a dialog up again. A
   request with a To-tag, on any leg:

   | its dialog | the gate |
   |---|---|
   | none, or a caller-facing one the caller was never shown | 481 (§12.2.2) |
   | `Early`, `Confirmed` | pass |
   | `Ending` | BYE, PRACK pass; INFO, MESSAGE, NOTIFY, OPTIONS 200 here; others 487 (§21.4.25) |
   | `Ended(Rejected)` | PRACK passes (RFC 3262 §3); others 481 |
   | `Ended(PeerBye \| OurBye)` | 481, a BYE included (§15.1.2) |
   | `Retired` | 481; one whose release BYE is in flight reads as `Ending` |

   A dialog exists until the BYE or CANCEL ending it completes (§15.1.2), so
   requests crossing it are answered in it: a 200 the gate sends goes no
   further, and a 487 refuses what the dialog will no longer act on. A
   request below its
   dialog's last CSeq draws 500 (§12.2.2). An ACK passes on any dialog a 2xx
   established, ended since or not (RFC 5407 §2), and is otherwise dropped
   unanswered (§17.1.1.3); a CANCEL is matched by transaction (§9.1). A
   response resolves to the relay it answers (by dialog, method and CSeq),
   to its dialog, or to the request this stack sent; one whose From-tag is
   no tag of this stack's on the leg answers nothing and is dropped.
6. **A relay always ends its request's turn with a final.** No dialog to
   carry it now — no peer leg while the call lives, a peer leg with no
   dialog yet or several early ones and none named, a peer dialog its party
   was never shown or no longer live — draws 500 with `Retry-After` (§14.2,
   RFC 3311 §5.2): the sender may retry and keeps its dialog, which a 481
   would end (§12.2.1.2, RFC 5057 §5.2). An UPDATE with no relayable peer
   draws 491. A request from a leg its service owns that no rule takes draws
   403 (§21.4.4).
7. **Each dialog numbers its own requests.** A dialog holds `our_cseq`, the
   last CSeq this side sent there, and `peer_cseq`, the peer's last, which
   serves the §12.2.2 order check alone and is set by the gate for every
   request it passes. A relayed request takes the target dialog's next
   number, `our_cseq + 1`, never a copy of or a distance from the sender's
   (§12.2.1.1): a gap in the sender's sequence is not reproduced on the other
   face. Our sequence on a caller-facing dialog starts at 1, as on a dialled
   leg (§8.1.1.5 allows any start below 2³¹).
8. **The model states its invariants at the end of every turn**
   (`crates/b2bua/src/rules/model_invariants.rs`, on in debug builds and every
   harness): each dialog of a leg is named by exactly one pair of tags; a leg
   holds at most one confirmed or ending dialog (§12.1); every `fronts` entry and the
   leg's own tag name a dialog the call holds; a dialog stays until the call
   is released and its two sequences never decrease (§12.2.1.1, §12.2.2); no
   dialog is early once its INVITE has its final or its leg ended (§12.1),
   and none this stack is CANCELling outlives that final (§9.1);
   and every in-dialog request but an ACK ends its turn with a final sent or
   its relay recorded (§8.2.6, §8.1.3.3).

9. **A leg's state is read, not stored.** `Leg::state()` reads its dialogs
   and its INVITE's facts: `Confirmed` while one dialog is, else
   `Terminated` once a BYE ended or is ending a dialog, its INVITE was
   CANCELled or took a final, else `Early` while a dialog is early, else
   `Trying`. A leg released while its BYE or CANCEL is in flight reads
   `Terminated`; the dialog keeps the in-flight detail. `Leg::end()` and
   `Leg::disposition()` read the same facts; `TerminateLeg` states what it
   records (`LegEnd`), never a state to write.

## Consequences

- A request on a retired or ended dialog is told apart from one on a dialog
  never held, without a side table.
- Every turn checks the model whole (decision 8), so a defect shows at the
  turn that made it, not at the message that later meets it.
- Each face of a call carries a contiguous run of this stack's own numbers;
  a trace that maps a request across the call matches it by the relay
  record, not by its CSeq.
- A peer's BYE after its dialog ended — by its own BYE or by ours — draws
  481; only a BYE crossing ours while it is in flight is answered 200.
- No gate cell reads the call's state: a call's teardown shows in its
  dialogs, each `Ending` by the BYE or CANCEL this stack sent.
- No leg state can disagree with its dialogs (decision 9): the setters and
  their ordering inside a turn are gone; a rule matches the leg as the
  event finds it, the INVITE's final recorded when a rule acts on it or
  after the rules.
