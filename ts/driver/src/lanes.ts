/**
 * `LanePresets` — which real systems a lane NAME means.
 *
 * A campaign names its lanes in the deployment's own vocabulary, and the
 * interpreter's `LaneSpec` says what each one binds, spawns or dials. Only the
 * deployment can map one to the other, so this is a seam and its default refuses
 * every lane by name.
 *
 * Refusing is the whole point. A guessed lane block would run a case against
 * whatever process happened to be listening, and report the result as if the
 * campaign had asked for it.
 */
import type { Campaign } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"
import type { LaneBlock } from "./run-spec.js"

/** The lane the campaign named is not one this composition can run. */
export class LaneUnknown extends Schema.TaggedError<LaneUnknown>()("Driver.LaneUnknown", {
  lane: Schema.String,
  detail: Schema.String
}) {}

export interface Interface {
  /** The interpreter lane block this cell runs on. */
  readonly laneOf: (
    cell: Campaign.PivotReplayCell
  ) => Effect.Effect<LaneBlock, LaneUnknown>
}

export class Service extends Context.Service<Service, Interface>()("@sip/driver/LanePresets") {}

/** The default: no lane is known, so no cell runs until a deployment says how. */
export const layer = Layer.succeed(
  Service,
  Service.of({
    laneOf: (cell) =>
      Effect.fail(
        new LaneUnknown({
          lane: cell.lane,
          detail:
            "no lane presets are composed — substitute LanePresets.layerWith to say what this " +
            "lane binds, spawns or dials"
        })
      )
  })
)

/** A deployment's own presets. */
export const layerWith = (
  laneOf: (cell: Campaign.PivotReplayCell) => Effect.Effect<LaneBlock, LaneUnknown>
): Layer.Layer<Service> => Layer.succeed(Service, Service.of({ laneOf }))

export * as LanePresets from "./lanes.js"
