/**
 * `LaneHosting` — what a lane NAME means to the orchestrator, beside what it
 * means to one cell (`@sip/driver` `LanePresets`, the interpreter's lane
 * block): whether its expectations are wall-clock deadlines, whether its SUT
 * is one box-wide singleton, how many cells it takes at once and how fast they
 * may start, the knobs a replay on it takes where its caller states none, how
 * its batch process and its SUT are stood, and what must hold on the box
 * before it is driven.
 *
 * A real-clock lane holds the box quiet while it runs — a neighbour's load
 * would time its cells out — and its runs cost a lab and their calls' wall
 * time to remake, so their bundles are kept. An exclusive lane is driven by
 * one invocation at a time. A lane with a batch process runs every cell of a
 * batch inside it; one without runs each cell as its own interpreter process.
 *
 * The clock a lane runs on is stated here and nowhere else in the
 * orchestration: the session it takes, the settle after a build, the bundles
 * retention keeps and the names a run settles all read it off this seam.
 *
 * The default declares no lane, and refuses by name every lane a replay
 * reaches for: a guessed lane would drive a SUT the deployment never named.
 * {@link NEUTRAL_LANE} is what a deployment states for a lane whose cells
 * build their SUT in-process.
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

/**
 * The replay knobs a lane defaults where neither the caller nor the request
 * states one; a knob left out is the generic default (`knobs.ts`). A ceiling
 * still clamps a default, as it clamps a stated value.
 */
export interface LaneDefaults {
  /** Cells at once. */
  readonly concurrency?: number
  /** Cells per batch. */
  readonly batchSize?: number
  /** Cell starts per second; `0` is as fast as concurrency slots free. */
  readonly startsPerSecond?: number
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
  /** The knobs a replay on it takes where nobody states them. */
  readonly defaults?: LaneDefaults
  /** Whether a SUT outside the cell is stood up for each batch (`managed`) or found running (`standing`). */
  readonly sut?: "managed" | "standing"
}

/** A lane whose cells build their SUT in-process: paused clock, shared, no ceiling. */
export const NEUTRAL_LANE: Lane = { clock: "paused", exclusive: false }

/** No lane hosting is composed; the message names the lane asked for and how to state it. */
export class LaneUnstated extends Schema.TaggedError<LaneUnstated>()("Corpus.LaneUnstated", {
  lane: Schema.String,
  /** The lanes the deployment does state, where one is composed. */
  known: Schema.optional(Schema.Array(Schema.String))
}) {
  override get message(): string {
    return this.known === undefined
      ? `no lane hosting is composed — substitute LaneHosting.layerWith to say what lane '${this.lane}' is`
      : `there is no lane '${this.lane}': the lanes are ${this.known.join(", ")}`
  }
}

export interface Interface {
  /** Every lane id the deployment declares. */
  readonly lanes: ReadonlyArray<string>
  /** What `lane` states. */
  readonly lane: (lane: string) => Effect.Effect<Lane, LaneUnstated>
  /** Stands the lane's batch process; `undefined` where its cells run on their own. */
  readonly standBatch: (
    lane: string,
    request: BatchRequest
  ) => Effect.Effect<BatchHost | undefined, LaneUnready | LaneUnstated>
  /**
   * Stands the lane's SUT for a batch, logging under `under`; `undefined` where
   * the cell builds its own. A managed SUT's own tool is current by then
   * (`Tools.ready("sut")`).
   */
  readonly standSut: (lane: string, under: string) => Effect.Effect<SutHandle | undefined, LaneUnready | LaneUnstated>
  /** Why the lane cannot be driven on this box now, naming what fixes it; `null` where it can. */
  readonly preflight: (lane: string) => Effect.Effect<string | null, LaneUnstated>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/LaneHosting") {}

/** The lanes the deployment declares whose clock is real. */
export const realClock = (hosting: Interface): Effect.Effect<ReadonlyArray<string>, LaneUnstated> =>
  Effect.map(Effect.forEach(hosting.lanes, (lane) => hosting.lane(lane)), (states) =>
    hosting.lanes.filter((_, at) => states[at]!.clock === "real"))

/** The default: no lane declared, so every lane a replay names is refused by name. */
export const layer = Layer.succeed(
  Service,
  Service.of({
    lanes: [],
    lane: (lane) => Effect.fail(new LaneUnstated({ lane })),
    standBatch: (lane) => Effect.fail(new LaneUnstated({ lane })),
    standSut: (lane) => Effect.fail(new LaneUnstated({ lane })),
    preflight: (lane) => Effect.fail(new LaneUnstated({ lane }))
  })
)

/** A deployment's own lanes. */
export const layerWith = (hosting: Interface): Layer.Layer<Service> => Layer.succeed(Service, Service.of(hosting))
