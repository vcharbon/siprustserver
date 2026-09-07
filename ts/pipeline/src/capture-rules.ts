/**
 * The CAPTURE tier: a refusal decided on the call family alone.
 *
 * The vantages are deliberately absent from {@link CaptureInput}. The cut
 * PRODUCES them, so a rule that reads one is asking a question the capture alone
 * cannot answer and belongs a tier later (`./case-rules.ts`). That one word is
 * the whole boundary, and it is drawn by the input type rather than by a
 * convention, so a rule cannot reach past it.
 *
 * A roster of these is read in ON-DISK order, not in match order: refusals are
 * ALL-THAT-APPLY, and every rule that fires is recorded. Nothing here is
 * first-match-wins — that is the tolerance family's contract
 * (the downstream tolerance registry) and copying it here would silently
 * delete the second reason a case was refused.
 */
import type { Flows } from "@sip/contracts"
import type { Finding, RuleIdentity } from "./refusal-rule.js"
import type { SutSet } from "./sut.js"

/**
 * What a CAPTURE-TIER refusal is decided from: the call family, and nothing the
 * cut has not decided yet.
 */
export interface CaptureInput {
  readonly flows: Flows.FlowsDoc
  readonly capture: string
  readonly sut: SutSet
  /** The captured legs of the correlated family, ascending. */
  readonly legs: ReadonlyArray<number>
  /** The family's own correlated Call-IDs. */
  readonly callIds: ReadonlyArray<string>
  /** The id the refusal is recorded under, for the rule's own finding line. */
  readonly caseId: string
}

/** One rule of the capture tier. */
export interface CaptureRule extends RuleIdentity {
  readonly refuses: (input: CaptureInput) => Finding | undefined
}
