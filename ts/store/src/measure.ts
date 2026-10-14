/**
 * The MEASUREMENT of a case row's main error after the fact: which runs the
 * queue reads still carry a failing row nobody measured, and the re-measure
 * that fills a run's error columns from a fresh reading of its bundle and
 * touches nothing else the run said. The one writer of the error columns
 * outside the run's own write (`Runs.writeCases`); `Calls` only reads them.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { CaseResult } from "./model.js"
import * as Sessions from "./sessions.js"

/** A run the queue reads whose failing rows carry no measure, and the bundle to re-read. */
export interface Unmeasured {
  readonly id: number
  readonly bundlePath: string
}

export interface Interface {
  /**
   * Runs the queue reads — the current runs of each call (`current_cells`),
   * on the lane or on every lane — with a current failing row nobody
   * measured, newest first.
   */
  readonly unmeasured: (lane?: string) => Effect.Effect<ReadonlyArray<Unmeasured>, SqlError>
  /**
   * Writes the error columns of a run's cases from a fresh reading of its
   * bundle, and nothing else: `passed` and the verdict are what the run said
   * and stay so. Answers the rows updated.
   */
  readonly remeasure: (runId: number, cases: ReadonlyArray<CaseResult>) => Effect.Effect<number, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Measure") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    const unmeasured = Effect.fn("Measure.unmeasured")(function*(lane?: string) {
      const rows = yield* sql<{ id: string; bundle_path: string }>`
        SELECT DISTINCT c.run_id AS id, c.bundle_path
        FROM current_cells c
        WHERE ${lane === undefined ? sql`true` : sql`c.lane = ${lane}`}
          AND c.bundle_path <> ''
          AND NOT c.passed AND c.skipped IS NULL AND c.error_kind IS NULL
        ORDER BY c.run_id DESC
      `
      return rows.map((row) => ({
        id: Number(row.id),
        bundlePath: Sessions.fromStored(root, row.bundle_path)
      } satisfies Unmeasured))
    })

    const remeasure = Effect.fn("Measure.remeasure")(function*(runId: number, cases: ReadonlyArray<CaseResult>) {
      let updated = 0
      yield* sql.withTransaction(Effect.gen(function*() {
        for (const entry of cases) {
          const error = entry.mainError
          const rows = yield* sql<{ case_id: string }>`
            UPDATE case_results
            SET error_kind = ${error?.kind ?? null},
                error_rule = ${error?.rule ?? null},
                error_rfc = ${error?.rfc ?? null},
                error_step = ${error?.step ?? null},
                error_expected = ${error?.expected ?? null},
                error_detail = ${error?.detail ?? null},
                error_source = ${error?.source === undefined ? null : JSON.stringify(error.source)}::jsonb,
                body_failing = ${entry.bodyFailing ?? 0}
            WHERE run_id = ${runId} AND case_id = ${entry.caseId}
            RETURNING case_id
          `
          updated += rows.length
        }
      }))
      return updated
    })

    return Service.of({ unmeasured, remeasure })
  })
)
