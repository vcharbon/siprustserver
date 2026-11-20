/**
 * One batch of a lane's cells through the corpus batch driver (`@sip/corpus`
 * `replay/lane-batch`): its row filed under the run's campaign job with the
 * batch's attempt (1 for the lane's first, n for the n-th replay of its
 * failed cells) and its cells by id. A run that is no job (a quick run)
 * files no row.
 */
import * as LaneBatch from "@sip/corpus/replay/lane-batch"
import type { Cell } from "./cell.js"

/** Whose batch it is: the run's campaign job, none for a quick run, and the attempt on the lane. */
export interface Owner {
  readonly job: number | undefined
  readonly attempt: number
}

/** `cells` driven as one batch of `request.lane`, its row filed under `owner`. */
export const inBatch = <A, E, R, EC = never, RO = never, RC = never>(
  owner: Owner,
  cells: ReadonlyArray<Cell>,
  request: Omit<LaneBatch.Request<A, E, R, EC, RO, RC>, "row">
) =>
  LaneBatch.run({
    ...request,
    ...(owner.job === undefined
      ? {}
      : { row: { jobId: owner.job, lane: request.lane, attempt: owner.attempt, cells: cells.map((cell) => cell.id) } })
  })

/** Why the batch leaves every cell not judged, as each cell states it; `undefined` where its end judged them. */
export const unjudgedOf = (ended: LaneBatch.Ended<unknown>): string | undefined =>
  ended.judged ? undefined : LaneBatch.detailOf(ended) ?? LaneBatch.NO_END_LINE
