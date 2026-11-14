/**
 * What the cut reads of a capture ONCE, however many cases it yields: the
 * leg correlation (`./cut.ts`) and the number → written-forms table
 * (`./forms.ts`). Each is one linear pass over the document; a consumer
 * that rebuilt either per case would make the cut quadratic in calls.
 *
 * An entry that cuts a capture builds its index once, before the cut
 * (`CaseAssembler.Interface.index`), and hands the same value to the cut
 * (`cutCalls`, its `correlation`) and to the case set (`decideCases`), so
 * both join the capture's legs by one reading.
 */
import type { Flows, Rules } from "@sip/contracts"
import { correlate, type Correlation } from "./cut.js"
import type { CallIdDerivation } from "./derivation.js"
import type { Plan } from "./plan.js"
import { formsTable, type FormsTable } from "./forms.js"

export interface CaptureIndex {
  /** The flows document the index was built of, by identity. */
  readonly flows: Flows.FlowsDoc
  /** The plan its forms table was read under, by identity. */
  readonly plan: Plan
  readonly correlation: Correlation
  readonly forms: FormsTable
}

/**
 * The index of `flows`, its legs correlated by `derives` and by the
 * `header-pair` `rules`, its numbers read under `plan`. Throws where `flows`
 * does not project a header a rule names.
 */
export const captureIndex = (
  flows: Flows.FlowsDoc,
  derives: CallIdDerivation,
  plan: Plan,
  rules: ReadonlyArray<Rules.HeaderPairRule> = []
): CaptureIndex => ({ flows, plan, correlation: correlate(flows, derives, rules), forms: formsTable(flows, plan) })

/**
 * `index`, where it was built of this very `flows` document under this very
 * `plan` (identity, not content); a defect otherwise, since an index of
 * another capture or plan yields wrong cases silently.
 */
export const indexOf = (index: CaptureIndex, flows: Flows.FlowsDoc, plan: Plan): CaptureIndex => {
  if (index.flows !== flows) throw new Error("the capture index was built of another flows document")
  if (index.plan !== plan) throw new Error("the capture index was built under another plan")
  return index
}
