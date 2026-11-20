/**
 * The VERIFICATION LINES of a cell, a batch and a job (`@sip/contracts`
 * `Verification`, ADR-0060 appendix §8), as the `verifications` table holds
 * them: each holder's lines in the order written, a write replacing every
 * line the holder held. A cell's lines go with its case result (`runs.ts`
 * writes them beside it), a batch's with its `batches` row, a job's with its
 * `jobs` row.
 *
 * A cell's final status is read off its lines (`Verification.finalStatusOf`,
 * the `cell_statuses` view), never stored apart from them.
 */
import { Verification } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

export type Line = Verification.Verification

/** Who holds a set of lines: a cell (its case result), a batch, a job. */
export type Holder =
  | { readonly scope: "cell"; readonly runId: number; readonly caseId: string }
  | { readonly scope: "batch"; readonly batchId: number }
  | { readonly scope: "job"; readonly jobId: number }

/** Lines written per statement: a capture replay writes a few per cell for thousands of cells. */
const PAGE = 500

/** A `verifications` row as a select of its columns reads it. */
export interface Row {
  readonly run_id: string | null
  readonly case_id: string | null
  readonly batch_id: string | null
  readonly job_id: string | null
  readonly name: string
  readonly source: string
  readonly answer: Verification.Answer
  readonly message: string | null
  readonly detail: Verification.Detail | null
  readonly informative: boolean
  readonly waived: boolean
}

/** The line a row states. */
export const lineOf = (row: Pick<Row, "name" | "source" | "answer" | "message" | "detail" | "informative" | "waived">): Line => ({
  name: row.name,
  source: row.source,
  answer: row.answer,
  ...(row.message === null ? {} : { message: row.message }),
  ...(row.detail === null ? {} : { detail: row.detail }),
  ...(row.informative ? { informative: true } : {}),
  ...(row.waived ? { waived: true } : {})
})

const keyOf = (holder: Holder) =>
  holder.scope === "cell"
    ? { run_id: holder.runId, case_id: holder.caseId, batch_id: null, job_id: null }
    : holder.scope === "batch"
    ? { run_id: null, case_id: null, batch_id: holder.batchId, job_id: null }
    : { run_id: null, case_id: null, batch_id: null, job_id: holder.jobId }

/**
 * Replaces every line `holder` holds with `lines`, in their order. Runs in the
 * caller's transaction, so a case result and its lines land together.
 */
export const replace = Effect.fn("Verifications.replace")(function*(holder: Holder, lines: ReadonlyArray<Line>) {
  const sql = yield* SqlClient
  const key = keyOf(holder)
  yield* holder.scope === "cell"
    ? sql`DELETE FROM verifications WHERE scope = 'cell' AND run_id = ${holder.runId} AND case_id = ${holder.caseId}`
    : holder.scope === "batch"
    ? sql`DELETE FROM verifications WHERE scope = 'batch' AND batch_id = ${holder.batchId}`
    : sql`DELETE FROM verifications WHERE scope = 'job' AND job_id = ${holder.jobId}`
  const rows = lines.map((line, ordinal) => ({
    scope: holder.scope,
    ...key,
    ordinal,
    name: line.name,
    source: line.source,
    answer: line.answer,
    message: line.message ?? null,
    detail: line.detail === undefined ? null : JSON.stringify(line.detail),
    informative: line.informative === true,
    waived: line.waived === true
  }))
  for (let at = 0; at < rows.length; at += PAGE) {
    yield* sql`INSERT INTO verifications ${sql.insert(rows.slice(at, at + PAGE))}`
  }
})

/** The key a cell's lines are found under in {@link Interface.cells}. */
export const cellKey = (runId: number, caseId: string): string => `${runId}\u0000${caseId}`

export interface Interface {
  /** Replaces a batch's lines. */
  readonly writeBatch: (batchId: number, lines: ReadonlyArray<Line>) => Effect.Effect<void, SqlError>
  /** Replaces a job's lines. */
  readonly writeJob: (jobId: number, lines: ReadonlyArray<Line>) => Effect.Effect<void, SqlError>
  /** The lines of every cell of the runs, by {@link cellKey}, each in its written order; a cell with none is absent. */
  readonly cells: (runIds: ReadonlyArray<number>) => Effect.Effect<ReadonlyMap<string, ReadonlyArray<Line>>, SqlError>
  /** The lines `holder` holds, in their written order. */
  readonly of: (holder: Holder) => Effect.Effect<ReadonlyArray<Line>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Verifications") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const provided = <A, E>(effect: Effect.Effect<A, E, SqlClient>) => Effect.provideService(effect, SqlClient, sql)

    const writeBatch = (batchId: number, lines: ReadonlyArray<Line>) =>
      provided(sql.withTransaction(replace({ scope: "batch", batchId }, lines)))

    const writeJob = (jobId: number, lines: ReadonlyArray<Line>) =>
      provided(sql.withTransaction(replace({ scope: "job", jobId }, lines)))

    const cells = Effect.fn("Verifications.cells")(function*(runIds: ReadonlyArray<number>) {
      const out = new Map<string, Array<Line>>()
      if (runIds.length === 0) return out
      const rows = yield* sql<Row>`
        SELECT run_id, case_id, batch_id, job_id, name, source, answer, message, detail, informative, waived
        FROM verifications
        WHERE scope = 'cell' AND run_id = ANY(${runIds.map(String)}::bigint[])
        ORDER BY run_id, case_id, ordinal
      `
      for (const row of rows) {
        const key = cellKey(Number(row.run_id), row.case_id!)
        const list = out.get(key) ?? []
        list.push(lineOf(row))
        out.set(key, list)
      }
      return out
    })

    const of = Effect.fn("Verifications.of")(function*(holder: Holder) {
      const rows = yield* holder.scope === "cell"
        ? sql<Row>`
            SELECT * FROM verifications
            WHERE scope = 'cell' AND run_id = ${holder.runId} AND case_id = ${holder.caseId} ORDER BY ordinal
          `
        : holder.scope === "batch"
        ? sql<Row>`SELECT * FROM verifications WHERE scope = 'batch' AND batch_id = ${holder.batchId} ORDER BY ordinal`
        : sql<Row>`SELECT * FROM verifications WHERE scope = 'job' AND job_id = ${holder.jobId} ORDER BY ordinal`
      return rows.map(lineOf)
    })

    return Service.of({ writeBatch, writeJob, cells, of })
  })
)
