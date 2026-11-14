/**
 * `CaseAssembler` — one capture in, every case it yields out.
 *
 * The service exists so a caller never carries the policy around: it is bound
 * once, at composition, and every case a run assembles is assembled under the
 * same readings. A caller that could pass a policy per call could generate two
 * cases of one capture under two different platforms' rules.
 *
 * The capture's index comes from here too, built under the bound policy's
 * Call-ID derivation: an entry asks for it once per capture and hands it to
 * the cut and to `cases`, so the capture's legs are correlated once and the
 * cut and its cases read the same correlation.
 */
import type { AllowedErrors, Flows } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { captureIndex, type CaptureIndex } from "./capture-index.js"
import type { CaseSpec } from "./case-spec.js"
import { decideCases, type CaptureCases } from "./caseset.js"
import { LogicExtractor } from "./logic-extractor.js"
import type { PartsIndex } from "./parts.js"
import type { Plan } from "./plan.js"
import type { SutSet } from "./sut.js"

/** One capture, cut and ready to assemble. */
export interface CaptureInput {
  readonly flows: Flows.FlowsDoc
  /** The source capture's file name — what every ledger records against. */
  readonly capture: string
  readonly specs: ReadonlyArray<CaseSpec>
  readonly sut: SutSet
  readonly plan: Plan
  /**
   * The index {@link Interface.index} built of this very `flows` under this
   * very `plan`, the one the cut read; one of another document or plan is a
   * defect of {@link Interface.cases}.
   */
  readonly index: CaptureIndex
  readonly parts?: PartsIndex
  readonly allowed?: AllowedErrors.AllowedErrors
  /**
   * Assemble refused cases too, so a caller can replay what a refusal deleted
   * and falsify the rule that wrote it. Never changes which cases are generated.
   */
  readonly quarantine?: boolean
}

export interface Interface {
  /** The index of one capture under the bound policy's derivation: its one correlation and forms table. */
  readonly index: (flows: Flows.FlowsDoc, plan: Plan) => CaptureIndex
  /** Every case this capture yields, generated or refused; a defect where `input.index` is not of `input.flows` and `input.plan`. */
  readonly cases: (input: CaptureInput) => Effect.Effect<CaptureCases>
}

export class Service extends Context.Service<Service, Interface>()(
  "@sip/pipeline/CaseAssembler"
) {}

export const layer: Layer.Layer<Service, never, LogicExtractor.Service> = Layer.effect(
  Service,
  Effect.gen(function* () {
    const { policy } = yield* LogicExtractor.Service

    const index = (flows: Flows.FlowsDoc, plan: Plan): CaptureIndex => captureIndex(flows, policy.derives, plan)

    const cases = Effect.fn("CaseAssembler.cases")(function* (input: CaptureInput) {
      return decideCases({ ...input, policy })
    })

    return Service.of({ index, cases })
  })
)

export * as CaseAssembler from "./case-assembler.js"
