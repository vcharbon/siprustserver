/**
 * `Reclassifier` — the POST-RUN escape hatch: a deployment's last word on what a
 * finished run's verdict means to it.
 *
 * The interpreter's verdict is structural and stays authoritative: a run that
 * did not settle failed, whatever anyone says afterwards. What this seam exists
 * for is the opposite direction — a deployment that KNOWS a particular failure
 * is its own open defect can restate the cell as expected-red, and must say so
 * out loud in {@link Reclassified.reason}, so nothing is ever quietly greened.
 *
 * The default reclassifies nothing. A campaign that substitutes no layer reports
 * exactly what the interpreter decided.
 */
import type { Bundle, CellHits } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"

/** One finished cell, as the driver read it off the bundle. */
export interface RunOutcome {
  /** The case the cell ran. */
  readonly caseId: string
  /** The lane it ran on — deployment vocabulary, so an open token. */
  readonly lane: string
  readonly verdict: Bundle.RunVerdict
  /**
   * Absolute path of the finished cell's directory — the run bundle plus the
   * driver's post-run records — for a reclassifier that re-reads the evidence.
   */
  readonly dir: string
  /** What the run decided so far, before the deployment had a say. */
  readonly passed: boolean
}

/** What the cell counts as, once the deployment has spoken. */
export interface Reclassified {
  readonly passed: boolean
  /** Why it was restated. Required whenever `passed` differs from the verdict's. */
  readonly reason?: string
  /**
   * The cell's counts under the deployment's own rule families, taken off the
   * same reading that decided `passed`. The driver writes them beside the cell
   * so nothing has to parse it a second time to count it; absent writes nothing.
   */
  readonly hits?: CellHits.Hits
}

export interface Interface {
  readonly reclassify: (outcome: RunOutcome) => Effect.Effect<Reclassified>
}

export class Service extends Context.Service<Service, Interface>()(
  "@sip/pipeline/Reclassifier"
) {}

/** The neutral reclassifier: the interpreter's verdict, unchanged. */
export const layer = Layer.succeed(
  Service,
  Service.of({ reclassify: (outcome) => Effect.succeed({ passed: outcome.passed }) })
)

/** A deployment's own reading of a finished cell. */
export const layerWith = (
  reclassify: (outcome: RunOutcome) => Effect.Effect<Reclassified>
): Layer.Layer<Service> => Layer.succeed(Service, Service.of({ reclassify }))

export * as Reclassifier from "./reclassifier.js"
