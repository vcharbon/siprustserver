/**
 * The CAPTURED violations of a case: what the census decided a scripted party
 * broke in the source capture, stated as `rfc_violations` on the step that
 * carries the message the decision rests on (`PCAP2TEST_PIVOT_V3.md` §11.1).
 *
 * This is the provenance a replay needs before it may excuse a scripted party.
 * The run's RFC audit names a violation by the party that commits it, and it
 * cancels a scripted party's finding only where the document states that same
 * violation against that party on that same transaction — so the statement is
 * read off the wire the capture carried (`sipflow --rfc-census`, the rule bodies
 * the live audit runs too), never off the shape of the flow.
 *
 * Only a SCRIPTED party is stated. A hit charging the system under test's side
 * says what the source platform broke; the replay's own system answers for
 * itself in the live audit, and a stated SUT entry would refuse the run as a
 * claim nothing verifies.
 */
import { Violation, type Census, type Flows } from "@sip/contracts"
import type { DeclarationInput } from "./policy.js"

/** The index, in its leg's messages, of the message a hit's decision rests on. */
export const anchorOf = (hit: Census.CensusHit): number => {
  switch (hit.rule) {
    case "no-200-after-cancel":
      return hit.response_msg
    case "unacked-reliable-provisional":
      return hit.provisional_msg
    case "no-ack-to-dialog-creating-2xx":
      return hit.final_msg
    case "no-cancel-after-final":
      return hit.cancel_msg
    case "second-answer-repeats-the-first":
      return hit.second_answer_msg
    case "unacked-invite-non-2xx-final":
      return hit.reject_msg
  }
}

/**
 * The violations `hits` charge a scripted party with at this case's vantage,
 * one per rule and step.
 *
 * A hit is stated where a step's own coordinate IS its anchor message and the
 * charged endpoint is that step's party: the message's sender on a `send`, its
 * receiver on an `expect`. A hit whose anchor no step carries, or carries from
 * the far side of the vantage, states nothing — the audit then has no
 * statement to cancel against, and the scripted party's finding gates.
 */
export const capturedWith =
  (hits: ReadonlyArray<Census.CensusHit>) =>
  (input: DeclarationInput): ReadonlyArray<Violation.RfcViolation> => {
    const wanted = new Set(input.callIds)
    const stepById = new Map(input.steps.map((s) => [s.id, s]))
    const actorOfLeg = new Map(input.layout.legs.map((l) => [l.id, l.actor]))
    const out: Array<Violation.RfcViolation> = []
    for (const hit of hits) {
      if (hit.capture !== input.capture || !wanted.has(hit.call_id)) continue
      const anchor = anchorOf(hit)
      const msg: Flows.Msg | undefined = input.flows.legs[hit.leg]?.msgs[anchor]
      if (msg === undefined) continue
      for (const src of input.sources) {
        if (src.mirrored === true || src.origLeg !== hit.leg || src.msgIdx !== anchor) continue
        const step = stepById.get(src.id)
        if (step === undefined) continue
        const charged =
          (step.op === "send" && msg.src === hit.emitter) ||
          (step.op === "expect" && msg.dst === hit.emitter)
        const actor = actorOfLeg.get(step.leg)
        if (!charged || actor === undefined) continue
        if (!out.some((v) => v.rule === hit.rule && v.step === step.id)) {
          out.push({ rule: hit.rule, step: step.id, emitter: actor })
        }
      }
    }
    return out
  }
