/**
 * `RoutingCompiler` — how a document's routing INTENT becomes the run
 * configuration a lane can execute (`PCAP2TEST_PIVOT_V3.md` §4.3).
 *
 * The interpreter never reads `calls` to steer a run: routing reaches it only as
 * the pre-compiled directive headers, identity bindings, claim numbers and
 * endpoint rebinding this seam produces. Compiling them needs the deployment's
 * own routing vocabulary, so the seam is a service.
 *
 * The DEFAULT is not a refusal, unlike a lane's: it is the cell's own `overlay`,
 * or an overlay that states nothing. A campaign can therefore run a case whose
 * routing no compiler states yet by writing the overlay by hand in the campaign
 * document — which is the escape hatch a pilot needs, and it is visible in the
 * campaign file rather than buried in a layer.
 */
import { Bundle, type Campaign, type Pivot } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"

/** The document's routing cannot be compiled for this lane. */
export class RoutingRefused extends Schema.TaggedError<RoutingRefused>()("Driver.RoutingRefused", {
  case: Schema.String,
  lane: Schema.String,
  detail: Schema.String
}) {}

export interface Interface {
  /** The §4.3 overlay this cell runs with. */
  readonly compile: (
    cell: Campaign.PivotReplayCell,
    pivot: Pivot.PivotV3
  ) => Effect.Effect<Bundle.RunOverlay, RoutingRefused>
}

export class Service extends Context.Service<Service, Interface>()("@sip/driver/RoutingCompiler") {}

/** The neutral compiler: what the cell states, and nothing invented. */
export const layer = Layer.succeed(
  Service,
  Service.of({ compile: (cell) => Effect.succeed(cell.overlay ?? Bundle.emptyOverlay) })
)

/** A deployment's own compiler. A cell's hand-written overlay still wins. */
export const layerWith = (
  compile: (
    cell: Campaign.PivotReplayCell,
    pivot: Pivot.PivotV3
  ) => Effect.Effect<Bundle.RunOverlay, RoutingRefused>
): Layer.Layer<Service> =>
  Layer.succeed(
    Service,
    Service.of({
      compile: (cell, pivot) =>
        cell.overlay === undefined ? compile(cell, pivot) : Effect.succeed(cell.overlay)
    })
  )

export * as RoutingCompiler from "./routing.js"
