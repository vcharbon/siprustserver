/**
 * The cells a scope's collectors read, and where what they answered of each
 * lands (`@sip/rig` `CollectorRun`): the cells that ran to a bundle described
 * as `cell.v1` off their bundles (`@sip/rig` `CellOfBundle`), and what the
 * collectors answered of each merged into the landed cell once its run row
 * closed (`@sip/store` `CellAnswers`). A bundle no description can be read
 * from is said and left out of what the collectors read, each collector
 * answering it `not checkable`; an answer the store refuses is said, never
 * the run's failure.
 */
import { type Cell, CellOfBundle, type CollectorRun } from "@sip/rig"
import { CellAnswers } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"

/** Where a capture replay keeps its collectors' directories and its batches' attachments, under the session tree. */
export const COLLECTORS_DIR = "_collectors"

/**
 * A cell that ran to a bundle: its id among its scope's, its case, its lane,
 * its bundle, the run row it lands under, and which of the cell's replays on
 * the lane it is (1 where absent).
 */
export interface LandedCell {
  readonly id: string
  readonly caseId: string
  readonly lane: string
  readonly dir: string
  readonly runId: number
  readonly attempt?: number
}

/** What the collectors answered of one landed cell, merged once its run row closed. */
export interface Pending {
  readonly runId: number
  readonly caseId: string
  readonly answers: CellAnswers.Answers
}

/** A cell as a scope's collectors read it, its run row aside: its attempt 1 where it states none. */
export type Described = Omit<LandedCell, "runId">

/**
 * `cells` described off their bundles as cells of `batch`; one whose bundle
 * states no description is said and left unread, which every collector of its
 * lane answers `not checkable`.
 */
export const describe = (
  cells: ReadonlyArray<Described>,
  batch: string
): Effect.Effect<CollectorRun.Over, never, FileSystem.FileSystem> =>
  Effect.map(
    Effect.forEach(cells, (cell) =>
      CellOfBundle.ofBundle(cell.dir, { id: cell.id, case: cell.caseId, lane: cell.lane, batch, attempt: cell.attempt ?? 1 }).pipe(
        Effect.map((described): Result.Result<Cell.Cell, CollectorRun.Unread> => Result.succeed(described)),
        Effect.catch((unreadable) =>
          Effect.as(
            Console.error(`[collector] ${cell.id} not described, no collector reads it: ${unreadable.message}`),
            Result.fail({ id: cell.id, lane: cell.lane, why: unreadable.reason })
          )
        )
      )),
    (read) => ({
      cells: read.flatMap((one) => (Result.isSuccess(one) ? [one.success] : [])),
      unread: read.flatMap((one) => (Result.isFailure(one) ? [one.failure] : []))
    })
  )

/** What `collected` answered of each of `cells`, where it answered anything, in their order. */
export const pendingOf = (
  collected: ReadonlyArray<CollectorRun.Collected>,
  cells: ReadonlyArray<LandedCell>
): ReadonlyArray<Pending> =>
  cells.flatMap((cell) => {
    const verifications = collected.flatMap((one) => one.cells.get(cell.id)?.verifications ?? [])
    const attachments = collected.flatMap((one) => one.cells.get(cell.id)?.attachments ?? [])
    return verifications.length === 0 && attachments.length === 0
      ? []
      : [{ runId: cell.runId, caseId: cell.caseId, answers: { verifications, held: { dir: cell.dir, attachments } } }]
  })

/** `pending` merged into their landed cells, each refusal said. */
export const merge = (pending: ReadonlyArray<Pending>): Effect.Effect<void, never, CellAnswers.Service> =>
  Effect.flatMap(CellAnswers.Service, (store) =>
    Effect.forEach(pending, (one) =>
      store.merge(one.runId, one.caseId, one.answers).pipe(
        Effect.catchCause(() => Console.error(`[collector] run ${one.runId}: the store refused the answers of ${one.caseId}`))
      ), { discard: true }))
