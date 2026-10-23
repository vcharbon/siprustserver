/**
 * One call's history on EVERY lane: its latest run per lane, and its recent
 * runs whatever the lane, newest first, with how the case ended in each. What
 * a call study reads to show the call "on the other lanes" and to switch to
 * one of them.
 *
 * Both statements walk the capture's runs by `runs_capture_lane_id_idx`
 * newest first and probe `case_results` by its primary key, stopping at the
 * first hit per lane or at `limit`: the cost is the capture's runs, never the store's.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as Aside from "./aside.js"
import type { SetKind } from "./model.js"

/** One run of the call. */
export interface Entry {
  readonly lane: string
  readonly runId: number
  readonly setKind: SetKind
  readonly jobId: number | null
  readonly finishedAt: string | null
  readonly passed: boolean
  /** The lane verdict the case was blocked on; null where it ran. */
  readonly skipped: string | null
  readonly failedStep: string | null
  /** The main error's rule or failure tag; null where it passed or was never measured. */
  readonly errorRule: string | null
  readonly errorExpected: string | null
  /** The features of the document the run ran (`case_documents.features`). */
  readonly features: ReadonlyArray<string>
}

export interface History {
  /** The call's latest run on each lane asked that ran it, in the order asked. */
  readonly latest: ReadonlyArray<Entry>
  /** The call's last `limit` runs across every lane, newest first. */
  readonly recent: ReadonlyArray<Entry>
}

export interface Interface {
  readonly ofCall: (
    capture: string,
    caseId: string,
    lanes: ReadonlyArray<string>,
    limit: number
  ) => Effect.Effect<History, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/CallHistory") {}

interface Row {
  readonly lane: string
  readonly run_id: number | string
  readonly set_kind: SetKind
  readonly job_id: number | string | null
  readonly finished_at: string | Date | null
  readonly passed: boolean
  readonly skipped: string | null
  readonly failed_step: string | null
  readonly error_rule: string | null
  readonly error_kind: string | null
  readonly error_expected: string | null
  readonly features: ReadonlyArray<string> | null
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    /** The runs of the call on the capture, newest first, the lane narrowed or not; never an aside run. */
    const runsOf = (capture: string, caseId: string, lane: string | undefined, limit: number) =>
      sql<Row>`
        SELECT r.lane, r.id AS run_id, r.set_kind, r.job_id, r.finished_at,
               cr.passed, cr.skipped, cr.failed_step, cr.error_kind, cr.error_rule, cr.error_expected,
               d.features
        FROM runs r
        JOIN case_results cr ON cr.run_id = r.id AND cr.case_id = ${caseId}
        LEFT JOIN case_documents d ON d.hash = cr.document_hash
        WHERE r.capture = ${capture} AND ${lane === undefined ? sql`true` : sql`r.lane = ${lane}`}
          AND ${Aside.excluded(sql, "r")}
        ORDER BY r.id DESC
        LIMIT ${limit}
      `

    const ofCall = Effect.fn("CallHistory.ofCall")(function*(
      capture: string,
      caseId: string,
      lanes: ReadonlyArray<string>,
      limit: number
    ) {
      const latest = yield* Effect.forEach(lanes, (lane) => runsOf(capture, caseId, lane, 1))
      const recent = yield* runsOf(capture, caseId, undefined, limit)
      return { latest: latest.flat().map(entryOf), recent: recent.map(entryOf) } satisfies History
    })

    return Service.of({ ofCall })
  })
)

const entryOf = (row: Row): Entry => ({
  lane: row.lane,
  runId: Number(row.run_id),
  setKind: row.set_kind,
  jobId: row.job_id === null ? null : Number(row.job_id),
  finishedAt: row.finished_at === null ? null : new Date(row.finished_at).toISOString(),
  passed: row.passed,
  skipped: row.skipped,
  failedStep: row.failed_step,
  errorRule: row.error_rule ?? row.error_kind,
  errorExpected: row.error_expected,
  features: row.features ?? []
})
