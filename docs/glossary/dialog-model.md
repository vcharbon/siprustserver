# Dialog model vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Dialog model vocabulary".

How a leg keeps its dialogs (ADR-0041). The `call` crate's `Dialog` /
`Dialogs`; the engine and the rules read them, never by position.

**Dialog identity**:
The leg's Call-ID plus the dialog's **our tag** (this stack's) and **peer
tag**. A received request names it by `(To-tag, From-tag)`, a response by
`(From-tag, To-tag)`, on every leg alike (RFC 3261 §12).
_Avoid_: "identity tag" (one tag never names a dialog of a leg that holds
several), "local/remote tag" for the fields (the RFC terms stay for the RFC).

**Dialog state**:
`Early`, `Confirmed`, `Ending` (our BYE, or our CANCEL of its INVITE, in
flight: the dialog still exists), `Ended` (rejected by the INVITE's final, the
peer's BYE, our BYE's final or timeout) or **retired**.
A dialog record stays until its call is released.

**Retired dialog**:
An early dialog the call let go of without a BYE: a losing callee fork, a
caller-facing dialog the answer did not take, an early dialog of a leg ended
before its INVITE's final. It keeps the relays still open
in it and, for a fork whose 2xx came late, the ACK and BYE that released it;
while that BYE is in flight it reads as ending.
_Avoid_: "pruned"/"dropped" dialog (nothing is removed).

**Fronting**:
A caller-facing dialog **fronts** the callee dialogs it relays (`fronts`); a
dialog the stack answers itself fronts none.
_Avoid_: "tag map".

**Outbound INVITE**:
The dialog-creating INVITE a leg sent (its From-tag, CSeq and client
transaction, the final it took, whether this stack CANCELled it): each
dialog a response to it creates is minted from it. A dialled leg holds no
dialog before such a response.

**Leg state**:
Read off the leg's dialogs and its INVITE's facts, never stored
(`Leg::state`): a leg whose BYE or CANCEL is in flight reads `Terminated`.
_Avoid_: "set the leg state" (`TerminateLeg` records an end, `LegEnd`).

**Dialog gate**:
The engine step that resolves a message to the dialog it names, before any
rule reads it, and refuses a request its dialog does not take (481, or 500
when out of order). An ending dialog answers a request crossing its end
itself (200, or 487 for what it no longer acts on). Rules read what it
resolved (`RuleContext::dialog`).
_Avoid_: "foreign-dialog check" (the gate refuses by state, not only by
absence).

**Not now**:
A relayed request no dialog can carry yet while its own dialog lives on:
500 with `Retry-After`, never silence and never 481.
