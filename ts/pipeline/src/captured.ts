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
 * A SCRIPTED party is stated. A hit charging the system under test's side says
 * what the source platform broke; the replay's own system answers for itself in
 * the live audit, and a stated SUT entry would refuse the run as a claim
 * nothing verifies — except where the census reads the platform's hit as
 * relayed onward from a party's hit this case states: that copy is stated as a
 * `sut` entry that `relays` the party's, which gates nothing and, beside the
 * party's entry, is what cancels the system under test's own relayed copy.
 *
 * A hit anchored on a hop the case does not carry is stated on the carried
 * hop's copy of its anchor message — the same message forwarded along the same
 * leg — charging the endpoint on the same side of that copy: the scripted party
 * playing the forwarding element reproduces what it forwarded.
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
    const mine = hits.filter((hit) => hit.capture === input.capture && wanted.has(hit.call_id))
    const violations: Array<Violation.RfcViolation> = []
    const state = (v: Violation.RfcViolation): void => {
      if (!violations.some((w) => w.rule === v.rule && w.step === v.step && w.emitter === v.emitter)) {
        violations.push(v)
      }
    }
    // The party's statement each origin coordinate became, for the copies.
    const statedAt = new Map<string, string>()
    for (const hit of mine) {
      for (const at of placed(input, hit)) {
        if (at.actor === undefined) continue
        state({ rule: hit.rule, step: at.step, emitter: at.actor })
        const anchor = anchorOf(hit)
        if (anchor !== undefined) statedAt.set(originKey(hit.rule, hit.leg, anchor), at.step)
      }
    }
    for (const hit of mine) {
      if (hit.relays === undefined) continue
      const origin = statedAt.get(originKey(hit.rule, hit.relays.leg, hit.relays.anchor_msg))
      if (origin === undefined) continue
      for (const at of placed(input, hit)) {
        if (at.actor !== undefined) continue
        state({ rule: hit.rule, step: at.step, emitter: Violation.SUT_EMITTER, relays: origin })
      }
    }
    return { violations, flags }
  }

/** A step a hit lands on, and the actor it charges there — `undefined` where the charged endpoint is the platform's side. */
interface Placed {
  readonly step: string
  readonly actor: string | undefined
}

const originKey = (rule: string, leg: number, msg: number): string => `${rule} ${leg}:${msg}`

/**
 * Where `hit` lands in this case: the steps whose own coordinate is its anchor
 * message — or, where no step carries that message, the carried hop's copy of
 * it — and who it charges at each. A party is charged where the step's party
 * is the charged endpoint: the message's sender on a `send`, its receiver on
 * an `expect`. The platform's side is charged where the step's actor is the
 * other end of the message. Empty where no step carries it.
 */
const placed = (input: DeclarationInput, hit: Census.CensusHit): ReadonlyArray<Placed> => {
  const anchor = anchorOf(hit)
  if (anchor === undefined) return []
  const msg: Flows.Msg | undefined = input.flows.legs[hit.leg]?.msgs[anchor]
  if (msg === undefined) return []
  const own = at(input, hit.leg, anchor, msg, hit.emitter)
  if (own.length > 0) return own
  const copy = carriedCopy(input, hit.leg, anchor, msg)
  if (copy === undefined) return []
  const sameSide = hit.emitter === msg.src ? copy.msg.src : hit.emitter === msg.dst ? copy.msg.dst : undefined
  return sameSide === undefined ? [] : at(input, hit.leg, copy.index, copy.msg, sameSide)
}

/** The steps carrying `legs[leg].msgs[index]`, and whom a hit charging `charged` names at each. */
const at = (
  input: DeclarationInput,
  leg: number,
  index: number,
  msg: Flows.Msg,
  charged: string
): ReadonlyArray<Placed> => {
  const stepById = new Map(input.steps.map((s) => [s.id, s]))
  const actorOfLeg = new Map(input.layout.legs.map((l) => [l.id, l.actor]))
  const out: Array<Placed> = []
  for (const src of input.sources) {
    if (src.mirrored === true || src.origLeg !== leg || src.msgIdx !== index) continue
    const step = stepById.get(src.id)
    if (step === undefined) continue
    const sent = msg.src === charged
    const taken = msg.dst === charged
    if ((step.op === "send" && sent) || (step.op === "expect" && taken)) {
      const actor = actorOfLeg.get(step.leg)
      if (actor !== undefined) out.push({ step: step.id, actor })
    } else if ((step.op === "expect" && sent) || (step.op === "send" && taken)) {
      out.push({ step: step.id, actor: undefined })
    }
  }
  return out
}

/**
 * The copy of `msg` a step of this case carries on another hop of the same
 * leg: the same message forwarded — the same request method or status, CSeq,
 * and dialog tags — never a retransmission, the nearest in time where several
 * qualify. `undefined` where no step carries one.
 */
const carriedCopy = (
  input: DeclarationInput,
  leg: number,
  index: number,
  msg: Flows.Msg
): { readonly index: number; readonly msg: Flows.Msg } | undefined => {
  const msgs = input.flows.legs[leg]?.msgs ?? []
  const carried = new Set(
    input.sources.filter((s) => s.mirrored !== true && s.origLeg === leg).map((s) => s.msgIdx)
  )
  let best: { readonly index: number; readonly msg: Flows.Msg } | undefined
  msgs.forEach((other, i) => {
    if (i === index || !carried.has(i) || other.hop === msg.hop || other.repeat_of !== undefined) return
    if (!sameMessage(msg, other)) return
    if (best === undefined || Math.abs(other.ts_us - msg.ts_us) < Math.abs(best.msg.ts_us - msg.ts_us)) {
      best = { index: i, msg: other }
    }
  })
  return best
}

/** Whether two captured messages are one SIP message on two hops: the same start line's kind, CSeq and dialog tags. */
const sameMessage = (a: Flows.Msg, b: Flows.Msg): boolean => {
  const x = a.summary
  const y = b.summary
  if (x.cseq.seq !== y.cseq.seq || x.cseq.method.toUpperCase() !== y.cseq.method.toUpperCase()) return false
  if (x.from.tag !== y.from.tag || x.to.tag !== y.to.tag) return false
  if (x.kind === "request" && y.kind === "request") return x.method.toUpperCase() === y.method.toUpperCase()
  if (x.kind === "response" && y.kind === "response") return x.status === y.status
  return false
}
