/**
 * Where each attempt of a lane's cells lives: a lane's first batch is its
 * attempt 1, under the run's `<lane>/`, each cell's bundle at `<cell id>/`; a
 * retry (`run.ts`) is attempt n, under `<lane>/attempt-<n>/`, each bundle at
 * `<cell id>~<n>/`. A cell id holds no `~`, so a bundle name reads back as one
 * cell's attempt. What the rig's collectors of a campaign job's batch attach
 * to it lives under the job's directory in the session tree.
 */
import * as CollectorIo from "@sip/corpus/replay/collector-io"
import * as path from "node:path"
import type { Cell, Lane } from "./cell.js"

/** The directory of `lane`'s batch of attempt `attempt` under the run's `root`. */
export const batchDirOf = (root: string, lane: Lane, attempt: number): string =>
  attempt === 1 ? path.join(root, lane) : path.join(root, lane, `attempt-${attempt}`)

/** The name of `lane`'s batch of attempt `attempt`, unique within its run. */
export const batchNameOf = (lane: Lane, attempt: number): string => `${lane}-${attempt}`

/** Where a run's batches keep what their collectors attach: the run's directory, the session tree, the run's campaign job. */
export interface Holding {
  readonly root: string
  readonly sessions: string
  readonly job: number | undefined
}

/**
 * The directory holding what the rig's collectors of `lane`'s batch of
 * attempt `attempt` attach to the batch, theirs under it: the job's batch
 * `<run>/<name>` in the session tree (`CollectorIo.batchHolderOf`), `<run>`
 * the name of the run's directory, so each run of one job and each attempt
 * holds its own; the batch's own directory under the run's for a run that is
 * no job, whose batch no row holds.
 */
export const batchHolderOf = (holding: Holding, lane: Lane, attempt: number): string =>
  holding.job === undefined
    ? batchDirOf(holding.root, lane, attempt)
    : CollectorIo.batchHolderOf(holding.sessions, holding.job, path.join(path.basename(holding.root), batchNameOf(lane, attempt)))

/** The bundle directory name of `cell`'s attempt `attempt`. */
export const bundleNameOf = (cell: Pick<Cell, "id">, attempt: number): string =>
  attempt === 1 ? cell.id : `${cell.id}~${attempt}`

/** The cell id and the attempt a bundle directory name states. */
export const attemptOf = (name: string): { readonly id: string; readonly attempt: number } => {
  const match = /^(.*)~([0-9]+)$/.exec(name)
  return match === null ? { id: name, attempt: 1 } : { id: match[1]!, attempt: Number(match[2]) }
}
