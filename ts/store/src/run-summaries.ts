/**
 * The SUMMARY a citation kept of a run (`citations.ts`, `run_summaries` and
 * `cell_summaries`), read back: what a cited run said once its row is gone —
 * its line, every cell it held with its verdict, step and main error, and the
 * records citing it. A summary keeps no bundle path, bless class, accepted
 * count or document; a reader shows a summarised run as what it is.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { MainError, Provenance } from "./model.js"

/** The run's own line, as its summary kept it. */
export interface RunSummary {
  readonly runId: number
  readonly capture: string
  readonly lane: string
  readonly setKind: string
  readonly status: string
  readonly detail: string | null
  readonly cases: number
  readonly failed: number
  readonly env: string | null
  readonly branch: string | null
  readonly code: Provenance
  readonly sweepId: number | null
  readonly jobId: number | null
  readonly prepId: number | null
  readonly startedAt: string
  readonly finishedAt: string | null
  /** The records citing the run, in name order. */
  readonly citedBy: ReadonlyArray<string>
}

/** One cell of the run, as its summary kept it. */
export interface CellSummary {
  readonly caseId: string
  readonly capture: string
  readonly lane: string
  readonly callKey: string | null
  readonly passed: boolean
  readonly verdict: string
  readonly skipped: string | null
  readonly failedStep: string | null
  readonly mainError: MainError | null
  readonly code: Provenance
  readonly env: string | null
  readonly branch: string | null
}

export interface Summarised {
  readonly run: RunSummary
  /** In case id order. */
  readonly cells: ReadonlyArray<CellSummary>
}

export interface Interface {
  /** The kept summary of run `runId`; null where no citation kept one. */
  readonly of: (runId: number) => Effect.Effect<Summarised | null, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/RunSummaries") {}

const iso = (at: Date | null): string | null => (at === null ? null : at.toISOString())

const id = (value: string | null): number | null => (value === null ? null : Number(value))

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const of = Effect.fn("RunSummaries.of")(function*(runId: number) {
      const runs = yield* sql<{
        capture: string
        lane: string
        set_kind: string
        status: string
        detail: string | null
        cases: number
        failed: number
        env: string | null
        branch: string | null
        components: Provenance | null
        sweep_id: string | null
        job_id: string | null
        prep_id: string | null
        started_at: Date
        finished_at: Date | null
        cited_by: ReadonlyArray<string> | null
      }>`
        SELECT s.capture, s.lane, s.set_kind, s.status, s.detail, s.cases, s.failed, s.env, s.branch,
               s.components, s.sweep_id, s.job_id, s.prep_id, s.started_at, s.finished_at,
               (SELECT array_agg(c.cited_by ORDER BY c.cited_by) FROM citations c
                WHERE c.kind = 'run' AND c.ref = s.run_id::text) AS cited_by
        FROM run_summaries s WHERE s.run_id = ${runId}
      `
      const row = runs[0]
      if (row === undefined) return null
      const cells = yield* sql<{
        case_id: string
        capture: string
        lane: string
        call_key: string | null
        passed: boolean
        verdict: string
        skipped: string | null
        failed_step: string | null
        main_error: MainError | null
        components: Provenance | null
        env: string | null
        branch: string | null
      }>`
        SELECT case_id, capture, lane, call_key, passed, verdict, skipped, failed_step, main_error,
               components, env, branch
        FROM cell_summaries WHERE run_id = ${runId}
        ORDER BY case_id
      `
      return {
        run: {
          runId,
          capture: row.capture,
          lane: row.lane,
          setKind: row.set_kind,
          status: row.status,
          detail: row.detail,
          cases: Number(row.cases),
          failed: Number(row.failed),
          env: row.env,
          branch: row.branch,
          code: row.components ?? [],
          sweepId: id(row.sweep_id),
          jobId: id(row.job_id),
          prepId: id(row.prep_id),
          startedAt: row.started_at.toISOString(),
          finishedAt: iso(row.finished_at),
          citedBy: row.cited_by ?? []
        },
        cells: cells.map((cell): CellSummary => ({
          caseId: cell.case_id,
          capture: cell.capture,
          lane: cell.lane,
          callKey: cell.call_key,
          passed: cell.passed,
          verdict: cell.verdict,
          skipped: cell.skipped,
          failedStep: cell.failed_step,
          mainError: cell.main_error,
          code: cell.components ?? [],
          env: cell.env,
          branch: cell.branch
        }))
      } satisfies Summarised
    })

    return Service.of({ of })
  })
)
