/**
 * What made a run besides its code: the environment and branch it was run
 * from, the lane it ran on as a hash of the knobs that change how a cell
 * behaves, and the load it ran under.
 *
 * Which (env, branch) is CURRENT — the one population whose runs say what a
 * call stands at — is the store's own setting (`current.ts`), never a literal
 * here: a run from any other environment is recorded under its own name and
 * never moves what a call currently stands at.
 */
import * as crypto from "node:crypto"

/**
 * The load a run ran under, as the replay phase applied it: cells in flight
 * and cell starts per second after the lane's own ceilings, captures per
 * batch, the memory scope, and the span bound the selection stated.
 */
export interface Load {
  readonly concurrency: number
  readonly batchSize: number
  /** Cell starts per second; 0 is "as fast as slots free". */
  readonly startsPerSecond: number
  /** The `--max-span` bound in seconds; null where the selection stated none. */
  readonly maxSpanSeconds: number | null
  readonly memoryMax: string
}

/** The run's identity beyond its provenance; every field null where it could not be taken. */
export interface RunIdentity {
  readonly env: string | null
  readonly branch: string | null
  readonly laneHash: string | null
  readonly load: Load | null
}

export const UNKNOWN_IDENTITY: RunIdentity = { env: null, branch: null, laneHash: null, load: null }

/** The knobs that change how a cell behaves on a lane; concurrency and the span bound do not. */
export interface LaneKnobs {
  readonly startsPerSecond: number
  readonly batchSize: number
  readonly memoryMax: string
}

/**
 * The lane as a behaviour: its name and the knobs that change what a cell
 * meets on it. Two runs with one lane hash ran their cells under the same
 * conditions, whatever their concurrency or selection.
 */
export const laneHash = (lane: string, knobs: LaneKnobs): string =>
  crypto
    .createHash("sha256")
    .update(JSON.stringify([lane, knobs.startsPerSecond, knobs.batchSize, knobs.memoryMax]))
    .digest("hex")
    .slice(0, 16)
