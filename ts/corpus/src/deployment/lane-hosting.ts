/**
 * `LaneHosting` — what a lane NAME means to the orchestrator, beside what it
 * means to one cell (`@sip/driver` `LanePresets`, the interpreter's lane
 * block): whether its expectations are wall-clock deadlines, whether its SUT
 * is one box-wide singleton, how many cells it takes at once and how fast they
 * may start, how its batch process and its SUT are stood, and what must hold
 * on the box before it is driven.
 *
 * A real-clock lane holds the box quiet while it runs — a neighbour's load
 * would time its cells out — and its runs cost a lab and their calls' wall
 * time to remake, so their bundles are kept. An exclusive lane is driven by
 * one invocation at a time. A lane with a batch process runs every cell of a
 * batch inside it; one without runs each cell as its own interpreter process.
 *
 * The default declares no lane: every lane reads as a paused-clock lane with
 * no ceiling, no batch process, no SUT of its own and nothing to check, which
 * is what a lane whose SUT the cell builds in-process is.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Schema from "effect/Schema"
import type { BatchHost, BatchRequest, SutHandle } from "../replay/batch-protocol.js"

/** The lane's batch process or its SUT could not be stood; `detail` says why. */
export class LaneUnready extends Schema.TaggedError<LaneUnready>()("Corpus.LaneUnready", {
  lane: Schema.String,
  detail: Schema.String
}) {
  override get message(): string {
    return `lane '${this.lane}': ${this.detail}`
  }
}

/** What a lane states about itself. */
export interface Lane {
  /** `real`: wall-clock deadlines, the box held quiet, bundles kept. `paused`: none of that. */
  readonly clock: "real" | "paused"
  /** Whether one invocation at a time may drive it. */
  readonly exclusive: boolean
  /** How many cells it takes at once, where it takes fewer than a caller asks for. */
  readonly cellCeiling?: number
  /** How many cells may start per second, where the lane bounds it. */
  readonly startCeiling?: number
  /** Whether a SUT outside the cell is stood up for each batch (`managed`) or found running (`standing`). */
  readonly sut?: "managed" | "standing"
}

/** A lane the deployment does not declare. */
export const NEUTRAL_LANE: Lane = { clock: "paused", exclusive: false }

export interface Interface {
  /** Every lane id the deployment declares. */
  readonly lanes: ReadonlyArray<string>
  /** What `lane` states; {@link NEUTRAL_LANE} where the deployment declares it not. */
  readonly lane: (lane: string) => Lane
  /** Stands the lane's batch process; `undefined` where its cells run on their own. */
  readonly standBatch: (lane: string, request: BatchRequest) => Effect.Effect<BatchHost | undefined, LaneUnready>
  /** Stands the lane's SUT for a batch, logging under `under`; `undefined` where the cell builds its own. */
  readonly standSut: (lane: string, under: string) => Effect.Effect<SutHandle | undefined, LaneUnready>
  /** Why the lane cannot be driven on this box now, naming what fixes it; `null` where it can. */
  readonly preflight: (lane: string) => Effect.Effect<string | null>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/LaneHosting") {}

/** The lanes among `lanes` whose clock is real. */
export const realClock = (hosting: Interface): ReadonlyArray<string> =>
  hosting.lanes.filter((lane) => hosting.lane(lane).clock === "real")

/** The default: no lane declared, every lane the neutral one. */
export const layer = Layer.succeed(
  Service,
  Service.of({
    lanes: [],
    lane: () => NEUTRAL_LANE,
    standBatch: () => Effect.succeed(undefined),
    standSut: () => Effect.succeed(undefined),
    preflight: () => Effect.succeed(null)
  })
)

/** A deployment's own lanes. */
export const layerWith = (hosting: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(hosting))
