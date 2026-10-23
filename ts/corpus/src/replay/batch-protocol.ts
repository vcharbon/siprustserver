/**
 * The BATCH PROTOCOL: how a replay talks to a lane whose cells run inside one
 * long-lived interpreter process per batch, and to the SUT that batch drives.
 *
 * A cell reaches the batch process through the interpreter seam the driver
 * already runs a cell through (`ReplayCli.run`): its run-spec's path goes in,
 * the cell's report comes back, in the `replay <run-spec>` exit vocabulary, so
 * a cell the batch ran reads exactly as one the driver exec'd. Ending the
 * batch resolves on the batch's own cleanup gate: what the SUT still held once
 * every cell ended, and the call contexts it read before the first cell and
 * after the last.
 *
 * The deployment stands both (`LaneHosting`); the replay drives them through
 * these shapes and nothing else. What crosses the wire between a batch
 * process and its client is `@sip/contracts` `Batch`.
 */
import type { Batch } from "@sip/contracts"
import type { ReplayCli } from "@sip/toolchain"
import type * as Effect from "effect/Effect"

/** A call context the SUT held after the batch and not before it. */
export type HeldContext = Batch.HeldContext

/** What the batch's gate read of the SUT's call contexts. */
export type ContextsReport = Batch.ContextsReport

/** What a batch process is stood with. */
export interface BatchRequest {
  /** Where the batch's scratch goes. */
  readonly under: string
  /** The one directory holding a side row for every case the batch's cells run, where the cases carry any. */
  readonly rowsDir?: string
  /** The memory box of ONE cell; the batch's is this times `cells`. */
  readonly memoryMax: string
  /** How many cells the batch runs at once. */
  readonly cells: number
}

/** A batch process, as the batch holds it. */
export interface BatchHost {
  /** Runs one cell's run-spec in the process. */
  readonly run: (specPath: string) => Effect.Effect<ReplayCli.RunReport>
  /** How the process died, if it has; `undefined` while it lives. */
  readonly died: () => string | undefined
  /** Closes the process's input once every cell ended, and resolves on what the SUT still held (`undefined` clean). */
  readonly end: Effect.Effect<string | undefined>
  /** What the gate read of the SUT's call contexts once `end` resolved; `undefined` before, and where the lane reads none. */
  readonly contexts: () => ContextsReport | undefined
  /** Ends the process now, its cells in flight with it, and resolves once it exited; a process already gone is left. */
  readonly kill: Effect.Effect<void>
}

/** The lane's SUT, as the batch holds it. */
export interface SutHandle {
  /** Where it publishes Prometheus text, where it publishes any. */
  readonly metrics: string | undefined
  /** How a managed SUT died, if it has; `undefined` while it is alive. */
  readonly died: () => string | undefined
  /**
   * Stops a managed SUT and resolves once its process exited, so the next
   * batch finds its addresses free; a standing one is left exactly as it was
   * found.
   */
  readonly stop: Effect.Effect<void>
}
