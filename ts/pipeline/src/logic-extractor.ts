/**
 * `LogicExtractor` — the seam a deployment substitutes at to say what its own
 * platform does.
 *
 * It holds one value, a {@link CasePolicy}, and that is the point: every reading
 * that needs to know whose platform produced a capture lives in one record, so
 * a composition either supplies them all or runs the neutral ones. There is no
 * second way in — no registry, no environment lookup, no per-call override.
 *
 * The default layer states nothing, which is what an unconfigured run is
 * entitled to say about a system it was never told about.
 */
import * as Context from "effect/Context"
import * as Layer from "effect/Layer"
import { neutralPolicy, type CasePolicy } from "./policy.js"

export interface Interface {
  readonly policy: CasePolicy
}

export class Service extends Context.Service<Service, Interface>()(
  "@sip/pipeline/LogicExtractor"
) {}

/** The neutral extractor: nothing derives, nothing is caused, nothing is refused. */
export const layer = Layer.succeed(Service, Service.of({ policy: neutralPolicy }))

/** A deployment's own readings. */
export const layerWith = (policy: CasePolicy): Layer.Layer<Service> =>
  Layer.succeed(Service, Service.of({ policy }))

export * as LogicExtractor from "./logic-extractor.js"
