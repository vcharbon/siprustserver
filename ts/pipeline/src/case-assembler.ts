/**
 * `CaseAssembler` — one capture in, every case it yields out.
 *
 * The service exists so a caller never carries the policy around: it is bound
 * once, at composition, and every case a run assembles is assembled under the
 * same readings. A caller that could pass a policy per call could generate two
 * cases of one capture under two different platforms' rules.
 */
import type { AllowedErrors, Flows } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
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
  readonly parts?: PartsIndex
  readonly allowed?: AllowedErrors.AllowedErrors
  /**
   * Assemble refused cases too, so a caller can replay what a refusal deleted
   * and falsify the rule that wrote it. Never changes which cases are generated.
   */
  readonly quarantine?: boolean
}

export interface Interface {
  /** Every case this capture yields, generated or refused. */
  readonly cases: (input: CaptureInput) => Effect.Effect<CaptureCases>
}

export class Service extends Context.Service<Service, Interface>()(
  "@sip/pipeline/CaseAssembler"
) {}

export const layer: Layer.Layer<Service, never, LogicExtractor.Service> = Layer.effect(
  Service,
  Effect.gen(function* () {
    const { policy } = yield* LogicExtractor.Service

    const cases = Effect.fn("CaseAssembler.cases")(function* (input: CaptureInput) {
      return decideCases({ ...input, policy })
    })

    return Service.of({ cases })
  })
)

export * as CaseAssembler from "./case-assembler.js"
