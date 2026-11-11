/**
 * The BATCH: the cells one lane runs between a stand and a drain, as a row of
 * its own under the job that drove it, or under its sweep where the replay is
 * no job. Its window opens when the batch starts and closes once what the
 * lane stood for it (its SUT, its batch process) is given back; a row still
 * open has not ended, unless its owner was closed as gone, which closes it
 * as abandoned. A retry of failed cells is a batch of its own, its `attempt`
 * above 1. Its `detail` is what its end said of the batch rather than of a
 * cell: a SUT left dirty, a dead batch process, an end gate that wrote no end
 * line, cells a stop interrupted. Its verification lines (`Verifications`,
 * scope `batch`) are written with its close.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as Verifications from "./verifications.js"

/** Whose batch it is: its job, its sweep, or both; at least one. */
export type Owner =
  | { readonly jobId: number; readonly sweepId?: number }
  | { readonly jobId?: number; readonly sweepId: number }

export type OpenRequest = Owner & {
  readonly lane: string
  /** 1 for a lane's first batch of the cells, n for their n-th attempt. */
  readonly attempt?: number
  /** The batch's cells by name, in the order they are driven. */
  readonly cells: ReadonlyArray<string>
}

export interface Row {
  readonly id: number
  readonly jobId: number | null
  readonly sweepId: number | null
  readonly lane: string
  readonly attempt: number
  readonly cells: ReadonlyArray<string>
  readonly startedAt: string
  /** Null while the batch has not ended. */
  readonly finishedAt: string | null
  /** What its end said of the batch; null on a clean end. */
  readonly detail: string | null
}

export interface Interface {
  /** A batch started now: its row, open. */
  readonly open: (request: OpenRequest) => Effect.Effect<number, SqlError>
  /** The batch ended now, with what its end said of it (`null` clean) and the lines its gates answered, in one transaction. */
  readonly close: (
    id: number,
    detail: string | null,
    lines?: ReadonlyArray<Verifications.Line>
  ) => Effect.Effect<void, SqlError>
  /** Every batch of `owner`'s job, or of its sweep, still open, closed now with `detail`; the rows closed. */
  readonly abandon: (owner: Owner, detail: string) => Effect.Effect<ReadonlyArray<number>, SqlError>
  /** A job's batches, in the order they started. */
  readonly ofJob: (jobId: number) => Effect.Effect<ReadonlyArray<Row>, SqlError>
  /** A sweep's batches, in the order they started. */
  readonly ofSweep: (sweepId: number) => Effect.Effect<ReadonlyArray<Row>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Batches") {}

interface BatchRow {
  readonly id: string
  readonly job_id: string | null
  readonly sweep_id: string | null
  readonly lane: string
  readonly attempt: number
  readonly cells: ReadonlyArray<string>
  readonly started_at: Date
  readonly finished_at: Date | null
  readonly detail: string | null
}

const rowOf = (row: BatchRow): Row => ({
  id: Number(row.id),
  jobId: row.job_id === null ? null : Number(row.job_id),
  sweepId: row.sweep_id === null ? null : Number(row.sweep_id),
  lane: row.lane,
  attempt: row.attempt,
  cells: row.cells,
  startedAt: row.started_at.toISOString(),
  finishedAt: row.finished_at === null ? null : row.finished_at.toISOString(),
  detail: row.detail
})

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const open = Effect.fn("Batches.open")(function*(request: OpenRequest) {
      const rows = yield* sql<{ id: string }>`
        INSERT INTO batches (job_id, sweep_id, lane, attempt, cells)
        VALUES (${request.jobId ?? null}, ${request.sweepId ?? null}, ${request.lane}, ${request.attempt ?? 1},
                ARRAY(SELECT jsonb_array_elements_text(${JSON.stringify(request.cells)}::jsonb)))
        RETURNING id
      `
      return Number(rows[0]!.id)
    })

    const close = Effect.fn("Batches.close")(
      function*(id: number, detail: string | null, lines: ReadonlyArray<Verifications.Line> = []) {
        yield* sql.withTransaction(Effect.gen(function*() {
          yield* sql`UPDATE batches SET finished_at = now(), detail = ${detail} WHERE id = ${id}`
          yield* Verifications.replace({ scope: "batch", batchId: id }, lines)
        }))
      },
      Effect.provideService(SqlClient, sql)
    )

    const abandon = Effect.fn("Batches.abandon")(function*(owner: Owner, detail: string) {
      const rows = yield* sql<{ id: string }>`
        UPDATE batches SET finished_at = now(), detail = ${detail}
        WHERE finished_at IS NULL
          AND (job_id = ${owner.jobId ?? null} OR sweep_id = ${owner.sweepId ?? null})
        RETURNING id
      `
      return rows.map((row) => Number(row.id)).sort((a, b) => a - b)
    })

    const ofJob = Effect.fn("Batches.ofJob")(function*(jobId: number) {
      const rows = yield* sql<BatchRow>`SELECT * FROM batches WHERE job_id = ${jobId} ORDER BY id`
      return rows.map(rowOf)
    })

    const ofSweep = Effect.fn("Batches.ofSweep")(function*(sweepId: number) {
      const rows = yield* sql<BatchRow>`SELECT * FROM batches WHERE sweep_id = ${sweepId} ORDER BY id`
      return rows.map(rowOf)
    })

    return Service.of({ open, close, abandon, ofJob, ofSweep })
  })
)
