/**
 * Which value a replay's lane-defaulted knobs take — cells at once, cells per
 * batch, starts per second: the value stated, else the request row's where the
 * path reads one, else the lane's own default (`LaneHosting.LaneDefaults`),
 * else the generic default (`knobs.ts`). Each knob resolves on its own, and
 * "stated" means stated: a value equal to a default is still the caller's.
 */
import * as Effect from "effect/Effect"
import { LaneHosting } from "../deployment/index.js"
import { DEFAULT_BATCH, DEFAULT_CONCURRENCY, DEFAULT_STARTS_PER_SECOND } from "../knobs.js"

/** The knobs a lane may default. */
export interface Knobs {
  readonly concurrency: number
  readonly batchSize: number
  readonly startsPerSecond: number
}

/** The generic defaults: what a knob takes where nothing, the lane included, states it. */
export const GENERIC: Knobs = {
  concurrency: DEFAULT_CONCURRENCY,
  batchSize: DEFAULT_BATCH,
  startsPerSecond: DEFAULT_STARTS_PER_SECOND
}

/** What an unstated knob takes on `lane`: its own default, else the generic one. */
export const ofLane = (lane: LaneHosting.Lane): Knobs => ({
  concurrency: lane.defaults?.concurrency ?? GENERIC.concurrency,
  batchSize: lane.defaults?.batchSize ?? GENERIC.batchSize,
  startsPerSecond: lane.defaults?.startsPerSecond ?? GENERIC.startsPerSecond
})

/** Each knob as stated, else as the request row carries it, else as `defaults` (`ofLane`) has it. */
export const resolve = (
  stated: Partial<Knobs>,
  requested: Partial<Knobs> | undefined,
  defaults: Knobs
): Knobs => ({
  concurrency: stated.concurrency ?? requested?.concurrency ?? defaults.concurrency,
  batchSize: stated.batchSize ?? requested?.batchSize ?? defaults.batchSize,
  startsPerSecond: stated.startsPerSecond ?? requested?.startsPerSecond ?? defaults.startsPerSecond
})

/**
 * `lane`'s defaults as the deployment states them. A lane it does not state
 * takes the generic ones: refusing that lane is the command's own refusal,
 * asked before any work.
 */
export const forLane = (lane: string): Effect.Effect<Knobs, never, LaneHosting.Service> =>
  Effect.flatMap(LaneHosting.Service, (hosting) => hosting.lane(lane)).pipe(
    Effect.map(ofLane),
    Effect.catchTag("Corpus.LaneUnstated", () => Effect.succeed(GENERIC))
  )
