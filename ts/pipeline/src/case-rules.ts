/**
 * The CASE tier: a refusal decided on the capture PLUS the vantages the cut
 * produced, and still before any document exists.
 *
 * The one word that separates this tier from the one below it is VANTAGES: a
 * rule here reads which leg and hop each simulated peer binds at, which is what
 * lets it judge the caller's dialog apart from the called ones. It still holds
 * no step ids, no `op` and no `auto` — those exist only once the document is
 * assembled (`./document-rules.ts`).
 *
 * A roster of these is read in ON-DISK order, not in match order: refusals are
 * ALL-THAT-APPLY, and every rule that fires is recorded.
 */
import type { CaseSpec } from "./case-spec.js"
import type { CaptureInput } from "./capture-rules.js"
import type { Finding, RuleIdentity } from "./refusal-rule.js"

/** What a CASE-TIER refusal is decided from: the capture, plus the cut's vantages. */
export interface RefusalInput extends CaptureInput {
  readonly spec: CaseSpec
}

/** One rule of the case tier. */
export interface CaseRule extends RuleIdentity {
  readonly refuses: (input: RefusalInput) => Finding | undefined
}
