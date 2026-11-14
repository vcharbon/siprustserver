# Rules read call facts

**Status:** accepted (2026-11-14).

**Owner:** `ts/pipeline/` (the call facts module; the situation reading `situation.ts` and
`pivot-reading.ts`).

## Context

A pack's cut, refusal and comparison rules each find a call's messages their own way: each
walks the flows document or the case document's steps, and each works out again whether the
caller was answered, got a final or gave up. This repository disagrees with itself on a leg's
opening INVITE: `finals.ts` takes the first, `call-completeness.ts` and `capture-edge.ts` the
last. "The system released on its own" is read with a clock skew in one rule and without one in
another. Each walk is a defect the next rule repeats, and several are quadratic in a call's
messages.

## Decision

1. **Call facts is a module of `@sip/pipeline`.** It states RFC readings only: no join kind, no
   deployment's role for a leg. Who acted is "the party" or "the system".
2. **Capture side.** The input is a position: the flows document, a leg, the hop's message
   indices and the system's address set. Every reader of a capture reduces to it: the refusal
   tiers, which run before the layout exists, the joins hook and the later hooks. One
   computation serves each position once per file. It holds indices, never copies of messages.
3. **Document side.** The input is a case document, built once when the case is written and
   passed down. It reads what the document states (an attempt's final, its cause, an abandon, a
   refusal). Its order, times and in-dialog facts mean what the replay will do, not what the
   capture showed: the cut drops keepalives and stray ACKs, folds retransmissions, adds far-side
   re-INVITE steps and reorders relayed provisionals.
4. **Two levels.** A *timeline*: each message with its leg, direction, hop, transaction, dialog
   state, time, body media types, retransmission count, whether the system sent it, and a
   pointer to its source message or step. A fixed set of *leg facts* and *call facts* derived
   from it. A rule reads content through an entry's source pointer. A deployment's rules never
   walk flows or steps themselves. A fact joins the fixed set when a second rule asks for it;
   until then the rule derives it from the timeline.
5. **Three states.** A fact is *seen*, *absent* or *not captured*.
6. **A leg is one attempt at one position.** It may hold several opening transactions (a retry
   after a 401 or 407 challenge); its outcome is the last one's final. Any method may open a
   leg, REGISTER included. Derived facts exist for INVITE-opened legs only.
7. **Starting leg facts.** The opening INVITE (time, offer present, 100rel asked); the first
   18x, a reliable provisional, a provisional with a session description; the outcome final
   (status, time); the answer time; how the leg ended (BYE or CANCEL, by the party or the system,
   time); a CANCEL that crossed the final; each accepted in-dialog request (method, body media
   types, time).
8. **Call facts need the caller's legs.** The caller of the module passes which legs form the
   call and the chain (the hunted legs in order, per branch); the module infers neither.
   Starting call facts: the caller was answered; the caller got a failure final; the caller's
   give-up, an event with its time; each attempt's ring time from its INVITE and from its first
   18x; "the system released on its own". That last one has one definition, aware of the
   capture's clock skew, the skew passed in by the pack. The situation reading's pairing takes
   no skew: on a measured corpus a skew window changed 300 of 295,327 emissions, about 5 of
   them repaired, so a window mispairs more than it repairs.
9. **`CasePolicy` does not change.** A pack's rules call the module with the inputs each hook
   already receives.
10. **No per-file work in a per-case path.** What a rule needs of the whole file (another
    call's tags, an address set) is an index built once per file.
11. **One correct reading.** Where unifying the readings changes a case document or a row a
    pack derives, the RFC-correct reading wins, and the change lists each case it changed.
12. **Linear, and proved by counting.** A test counts visits through the module's own
    interface: twice the messages cost at most 2.2 times the visits, decoy legs in the file
    change nothing, and a second request for the same position costs nothing. Merging several
    positions' timelines into one capture order reads no message. The situation
    reading (`situation.ts`) places a call's emitted messages in one forward pass, linear in the
    call's messages with a factor logarithmic in the longest run of identical responses a relay
    reads. Its counting test, which also counts every message read, states a bound per shape:
    at most 2.2 times the cost for twice the exchanges or the forked legs with the runs fixed,
    and what n log n allows when a run doubles. A differential test holds it to the rules
    applied literally.

## Considered and rejected

- **A capture-only module.** The document side has the same readers after the run, and they
  would keep their own walks.
- **A closed record of facts with no timeline.** Every new question would grow the record or
  send a rule back to walking messages.
- **One builder emitting both a capture timeline and a document.** It re-implements the cut.
- **Full parity between the capture and the document sides.** The cut changes order and drops
  messages by design. The two agree on a named subset: the opening INVITE, the first 18x, the
  outcome final's status, the answer, how the leg ended and who ended it.

## Consequences

- `calls.ts` (`finalOf`), `call-completeness.ts`, `capture-edge.ts` and the timestamp helpers of
  `topology.ts` read the capture side. The first-INVITE reading of a leg (`finals.ts`) is
  removed. `abandonOf` and `refusalOf` name a step of the case document, so they keep reading
  its steps. `Transactions` pairs the steps of a document and stays exported for the readers
  the document side does not serve yet.
- The capture side is exported as `CallFacts`, the document side as `DocumentFacts`, whose
  `parity` holds a case document against its capture on the agreed subset and says how many
  legs it compared.
- A pack's per-rule walks over flows and steps move onto the module, one rule at a time, each
  move proved by re-cutting its captures and diffing the cases.
- The situation reading is exported as `Situation` and `PivotReading`; a pack's header census
  reads it from here.
- The cut's flow synthesis reads its case's positions merged in capture order
  (`merged-timeline.ts`) and builds no message list of its own. A timeline entry lists each
  retransmission folded into it, and a retransmission whose original the position does not
  hold is an entry of its own.
