/**
 * The OPEN PREPS: cuts opened and never finished, either running now or left
 * behind by a process that is gone. What tells the two apart — the cut's job,
 * when it last recorded a reading — is read here; whether that process lives
 * is the deployment's to judge before it {@link Interface.close}s one.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

export interface OpenPrep {
  readonly prepId: number
  readonly startedAt: string
  /** The job the cut ran as; null where it ran as none. */
  readonly jobId: number | null
  /** When it last recorded a reading; null where it recorded none. */
  readonly lastCutAt: string | null
}

export interface Interface {
  /** Every prep opened and not finished, oldest first. */
  readonly all: Effect.Effect<ReadonlyArray<OpenPrep>, SqlError>
  /** Finishes an open prep as it stands; answers whether it was open. */
  readonly close: (prepId: number) => Effect.Effect<boolean, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/OpenPreps") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const all = Effect.gen(function*() {
      const rows = yield* sql<{ id: string; started_at: Date; job_id: string | null; last_cut_at: Date | null }>`
        SELECT p.id, p.started_at, p.job_id,
               (SELECT max(cut_at) FROM capture_cuts c WHERE c.prep_id = p.id) AS last_cut_at
        FROM preps p WHERE p.finished_at IS NULL ORDER BY p.id
      `
      return rows.map((row): OpenPrep => ({
        prepId: Number(row.id),
        startedAt: row.started_at.toISOString(),
        jobId: row.job_id === null ? null : Number(row.job_id),
        lastCutAt: row.last_cut_at === null ? null : row.last_cut_at.toISOString()
      }))
    })

    const close = Effect.fn("OpenPreps.close")(function*(prepId: number) {
      const rows = yield* sql<{ id: string }>`
        UPDATE preps SET finished_at = now() WHERE id = ${prepId} AND finished_at IS NULL RETURNING id
      `
      return rows.length > 0
    })

    return Service.of({ all, close })
  })
)
