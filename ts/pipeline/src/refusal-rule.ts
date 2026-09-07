/**
 * What every refusal rule declares about itself, and what it reports when it
 * fires — the vocabulary the three tiers share, and the only thing they share.
 *
 * A rule states three facts about ITSELF and nothing about the corpus: the token
 * it refuses under, the party the token names, and what the refusal does to the
 * case. A rule states one fact about the CASE, a {@link Finding}, and the
 * evaluator turns it into the {@link Policy.Refusal} everything downstream
 * reads. So a rule never mints a case id, and a case id is never a rule's to
 * choose.
 *
 * There is deliberately no rule type here. A rule's INPUT is its tier
 * (`./capture-rules.ts`, `./case-rules.ts`, `./document-rules.ts`), and a shape
 * generic over the input would let a rule reach data its tier does not hold —
 * which is the whole boundary those three files exist to draw.
 */
import type { ChargedHit, Disposition, Refusal, RefusalEvidence } from "./policy.js"

/**
 * Whose behaviour the token names, made declarable so a roster cannot claim one
 * party and read as another.
 *
 * - `source` — the CAPTURE lost it. RFC-decidable, deployment-free.
 * - `egress` — the trace never held that side of the call.
 * - `sut` — the SOURCE PLATFORM did something this platform will not. Naming
 *   one is naming a deployment, so no roster inside `@sip/pipeline` may.
 * - `scope` — the pivot FORMAT cannot state the mechanism.
 */
export type RefusalSubject = "source" | "egress" | "sut" | "scope"

/** What every refusal rule declares about itself, at every tier. */
export interface RuleIdentity {
  /** The reason token, greppable in the log and on disk. Never reused. */
  readonly id: string
  readonly subject: RefusalSubject
  readonly disposition: Disposition
}

/**
 * What a rule reports when it fires: the finding, and nothing about which case
 * carried it.
 *
 * `line` is the rule's own prose because the preamble is not uniform — the cut's
 * two tokens refuse a family that never reached a case id, so no evaluator can
 * compose it for them.
 */
export interface Finding {
  /** The one-line finding, for stderr. */
  readonly line: string
  /** The captured legs it rests on, where the rule names them. */
  readonly legs?: ReadonlyArray<number>
  /** The flows groups it rests on, where the rule names them. */
  readonly groups?: ReadonlyArray<number>
  /** The Call-IDs the rule itself charged. Absent where it charges the whole case. */
  readonly callIds?: ReadonlyArray<string>
  /** What it rests on, down to the datagram. */
  readonly evidence?: ReadonlyArray<RefusalEvidence>
  /** The coordinates a declaration must anchor to withdraw it. */
  readonly charged?: ReadonlyArray<ChargedHit>
}

/** Where a refusal is recorded: the case the evaluator was deciding. */
export interface RefusalSite {
  readonly caseId: string
  readonly capture: string
}

/** One finding recorded against one case, under its rule's own declaration. */
export const refusalOf = (rule: RuleIdentity, site: RefusalSite, finding: Finding): Refusal => ({
  caseId: site.caseId,
  capture: site.capture,
  reason: rule.id,
  line: finding.line,
  disposition: rule.disposition,
  ...(finding.legs === undefined ? {} : { legs: finding.legs }),
  ...(finding.groups === undefined ? {} : { groups: finding.groups }),
  ...(finding.callIds === undefined ? {} : { callIds: finding.callIds }),
  ...(finding.evidence === undefined ? {} : { evidence: finding.evidence }),
  ...(finding.charged === undefined ? {} : { charged: finding.charged })
})
