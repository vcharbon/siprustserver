/**
 * What the cut reads of a capture ONCE, however many cases it yields: the
 * Call-ID correlation (`./cut.ts`) and the number → written-forms table
 * (`./forms.ts`). Each is one linear pass over the document; a consumer
 * that rebuilt either per case would make the cut quadratic in calls.
 */
import type { Flows } from "@sip/contracts"
import { correlate, type Families } from "./cut.js"
import type { CallIdDerivation } from "./derivation.js"
import type { Plan } from "./plan.js"
import { formsTable, type FormsTable } from "./forms.js"

export interface CaptureIndex {
  readonly families: Families
  readonly forms: FormsTable
}

export const captureIndex = (
  flows: Flows.FlowsDoc,
  derives: CallIdDerivation,
  plan: Plan
): CaptureIndex => ({ families: correlate(flows, derives), forms: formsTable(flows, plan) })
