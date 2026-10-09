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
import { Violation, type Case, type Census, type Flows } from "@sip/contracts"
import type { DeclarationInput } from "./policy.js"

/**
 * The index, in its leg's messages, of the message a hit's decision rests on:
 * the head's `anchor_msg`, or — on a report taken before every hit stated it —
 * the modelled rule's own evidence field. `undefined` where neither says.
 */
export const anchorOf = (hit: Census.CensusHit): number | undefined => {
  if (hit.anchor_msg !== undefined) return hit.anchor_msg
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
    default:
      return undefined
  }
}

/** What the census states about one case: the violations, and the flags saying where it could not. */
export interface Statements {
  readonly violations: ReadonlyArray<Violation.RfcViolation>
  readonly flags: ReadonlyArray<Case.Flag>
}

/** The flag a case of a capture the census did not cover carries. */
export const UNCOVERED = "census-uncovered"

/**
 * The violations `hits` charge a scripted party with at this case's vantage,
 * one per rule and step, and a flag where the census never covered the case's
 * capture — an empty list there is no reading at all, and a scripted party's
 * finding has nothing to be cancelled by. `covered` is the report's coverage;
 * absent, coverage is not judged.
 *
 * A hit is stated where a step's own coordinate IS its anchor message and the
 * charged endpoint is that step's party: the message's sender on a `send`, its
 * receiver on an `expect`. A hit whose anchor no step carries, or carries from
 * the far side of the vantage, states nothing — the audit then has no
 * statement to cancel against, and the scripted party's finding gates.
 */
export const capturedWith =
  (hits: ReadonlyArray<Census.CensusHit>, covered?: ReadonlySet<string>) =>
  (input: DeclarationInput): Statements => {
    const flags: Array<Case.Flag> =
      covered === undefined || covered.has(input.capture)
        ? []
        : [
            {
              kind: UNCOVERED,
              detail:
                `the census report covers no capture '${input.capture}': nothing states what ` +
                `its scripted parties broke, so every finding against one gates`
            }
          ]
    const wanted = new Set(input.callIds)
    const stepById = new Map(input.steps.map((s) => [s.id, s]))
    const actorOfLeg = new Map(input.layout.legs.map((l) => [l.id, l.actor]))
    const violations: Array<Violation.RfcViolation> = []
    for (const hit of hits) {
      if (hit.capture !== input.capture || !wanted.has(hit.call_id)) continue
      const anchor = anchorOf(hit)
      if (anchor === undefined) continue
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
        if (!violations.some((v) => v.rule === hit.rule && v.step === step.id)) {
          violations.push({ rule: hit.rule, step: step.id, emitter: actor })
        }
      }
    }
    return { violations, flags }
  }
