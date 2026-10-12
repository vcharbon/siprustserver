/**
 * What OUTSIDE RECORDS cite of the store — runs, jobs, sweeps, preps and case
 * ids, each with the record citing it and the day it did — and one summary row
 * per cell of every run a citation names (`cell_summaries`): the case, its
 * call, its verdict, failed step and main error, the code and environment that
 * ran it. A summary is written once and outlives the run it was read from.
 *
 * A citation dated before the store's first cut names an id of another store:
 * it is recorded as cited and summarises nothing.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

export type Kind = "run" | "job" | "sweep" | "prep" | "case"

export interface Citation {
  readonly kind: Kind
  /** The id as the store names it: a run, job, sweep or prep number, or a case id. */
  readonly ref: string
  /** The record that cites it. */
  readonly citedBy: string
  /** The day it was cited, `YYYY-MM-DD`; null where the record does not say. */
  readonly citedOn: string | null
}

export interface Interface {
  /** Records citations, keeping every one already recorded; answers how many were new. */
  readonly record: (citations: ReadonlyArray<Citation>) => Effect.Effect<number, SqlError>
  /** Every recorded citation. */
  readonly all: Effect.Effect<ReadonlyArray<Citation>, SqlError>
  /** Summarises every cell of every cited run, job and sweep not summarised yet; answers the rows written. */
  readonly summarise: Effect.Effect<number, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Citations") {}

interface Row {
  readonly kind: Kind
  readonly ref: string
  readonly cited_by: string
  readonly cited_on: Date | null
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const record = Effect.fn("Citations.record")(function*(citations: ReadonlyArray<Citation>) {
      if (citations.length === 0) return 0
      const rows = yield* sql<{ n: number }>`
        WITH added AS (
          INSERT INTO citations (kind, ref, cited_by, cited_on)
          SELECT c.kind, c.ref, c."citedBy", c."citedOn"::date
          FROM jsonb_to_recordset(${JSON.stringify(citations)}::jsonb)
            AS c(kind text, ref text, "citedBy" text, "citedOn" text)
          ON CONFLICT DO NOTHING
          RETURNING 1
        )
        SELECT count(*)::int AS n FROM added
      `
      return Number(rows[0]!.n)
    })

    const all = Effect.gen(function*() {
      const rows = yield* sql<Row>`SELECT kind, ref, cited_by, cited_on FROM citations ORDER BY kind, ref, cited_by`
      return rows.map((row): Citation => ({
        kind: row.kind,
        ref: row.ref,
        citedBy: row.cited_by,
        citedOn: row.cited_on === null ? null : row.cited_on.toISOString().slice(0, 10)
      }))
    })

    const summarise = Effect.gen(function*() {
      const rows = yield* sql<{ n: number }>`
        WITH since AS (
          SELECT least(
            (SELECT min(cut_at) FROM capture_cuts),
            (SELECT min(cut_at) FROM case_maps)
          )::date AS day
        ),
        current AS (
          SELECT c.kind, c.ref::bigint AS id FROM citations c, since
          WHERE c.kind IN ('run', 'job', 'sweep') AND c.ref ~ '^[0-9]{1,18}$'
            AND (c.cited_on IS NULL OR since.day IS NULL OR c.cited_on >= since.day)
        ),
        cited AS (
          SELECT r.id FROM runs r JOIN current c ON c.kind = 'run' AND c.id = r.id
          UNION SELECT r.id FROM runs r JOIN current c ON c.kind = 'job' AND c.id = r.job_id
          UNION SELECT r.id FROM runs r JOIN current c ON c.kind = 'sweep' AND c.id = r.sweep_id
        ),
        written AS (
          INSERT INTO cell_summaries (
            run_id, case_id, capture, lane, call_key, passed, verdict, skipped, failed_step, main_error,
            components, env, branch, sweep_id, job_id, prep_id, started_at
          )
          SELECT r.id, cr.case_id, r.capture, r.lane,
                 (SELECT m.call_key FROM case_maps m
                  WHERE m.capture = r.capture AND m.case_id = cr.case_id
                    AND (r.prep_id IS NULL OR m.prep_id <= r.prep_id)
                  ORDER BY m.prep_id DESC LIMIT 1),
                 cr.passed, cr.verdict, cr.skipped, cr.failed_step,
                 CASE WHEN cr.error_kind IS NULL THEN NULL ELSE jsonb_build_object(
                   'kind', cr.error_kind, 'rule', cr.error_rule, 'rfc', cr.error_rfc, 'step', cr.error_step,
                   'expected', cr.error_expected, 'detail', cr.error_detail
                 ) END,
                 r.components, r.env, r.branch, r.sweep_id, r.job_id, r.prep_id, r.started_at
          FROM cited JOIN runs r ON r.id = cited.id JOIN case_results cr ON cr.run_id = r.id
          ON CONFLICT DO NOTHING
          RETURNING 1
        )
        SELECT count(*)::int AS n FROM written
      `
      return Number(rows[0]!.n)
    })

    return Service.of({ record, all, summarise })
  })
)
