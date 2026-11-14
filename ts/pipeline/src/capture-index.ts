/**
 * What the cut reads of a capture ONCE, however many cases it yields: the
 * Call-ID correlation (`./cut.ts`) and the number → written-forms table
 * (`./forms.ts`). Each is one linear pass over the document; a consumer
 * that rebuilt either per case would make the cut quadratic in calls.
 *
 * An entry that cuts a capture builds its index once, before the cut
 * (`CaseAssembler.Interface.index`), and hands the same value to the cut
 * (`cutCalls`, its `correlation`) and to the case set (`decideCases`), so
 * both join the capture's legs by one reading.
 */
import type { Flows } from "@sip/contracts"
import { correlate, type Correlation } from "./cut.js"
import type { CallIdDerivation } from "./derivation.js"
import type { Plan } from "./plan.js"
import { formsTable, type FormsTable } from "./forms.js"

export interface CaptureIndex {
  readonly correlation: Correlation
  readonly forms: FormsTable
}

/** The index of `flows`, its legs correlated by `derives` and its numbers read under `plan`. */
export const captureIndex = (
  flows: Flows.FlowsDoc,
  derives: CallIdDerivation,
  plan: Plan
): CaptureIndex => ({ correlation: correlate(flows, derives), forms: formsTable(flows, plan) })
