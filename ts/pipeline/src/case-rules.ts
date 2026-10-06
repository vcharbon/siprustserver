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
 * ALL-THAT-APPLY, and every rule that fires is recorded. The pipeline's own
 * ({@link CASE_RULES}) run ahead of a deployment's
 * ({@link Policy.CasePolicy.refuse}).
 */
import type { CaseSpec } from "./case-spec.js"
import type { CaptureInput } from "./capture-rules.js"
import { checkRefusalRoster } from "./refusal-roster.js"
import type { Finding, RuleIdentity } from "./refusal-rule.js"
import { IDENTITY_REOFFERED, reofferClause, reofferLine, reoffers } from "./reoffer.js"

/** What a CASE-TIER refusal is decided from: the capture, plus the cut's vantages. */
export interface RefusalInput extends CaptureInput {
  readonly spec: CaseSpec
}

/** One rule of the case tier. */
export interface CaseRule extends RuleIdentity {
  readonly refuses: (input: RefusalInput) => Finding | undefined
}

/**
 * Every case-tier refusal the pipeline states on its own account: what no
 * document of a vantage can state, whatever the deployment.
 */
export const CASE_RULES: ReadonlyArray<CaseRule> = [
  {
    id: IDENTITY_REOFFERED,
    subject: "scope",
    disposition: "refuses",
    refuses: (input) => {
      const charged = reoffers(input.flows, [input.spec.uac, ...input.spec.uas])
      if (charged.length === 0) return undefined
      const legs = [...new Set(charged.map((r) => r.leg))]
      return {
        legs,
        callIds: [...new Set(charged.map((r) => r.callId))],
        evidence: charged.map((r) => ({
          leg: r.leg,
          hop: r.hop,
          msg: r.again.msg,
          callId: r.callId,
          detail: reofferClause(r)
        })),
        line: reofferLine(input.capture, input.caseId, charged)
      }
    }
  }
]

checkRefusalRoster(CASE_RULES, { where: "@sip/pipeline case rules", deploymentFree: true })
