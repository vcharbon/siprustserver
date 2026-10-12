/**
 * What REMOVING takes out of the store: retiring a
 * source — every row keyed by the capture — and deleting runs — the run rows
 * and what hangs off them. Both are HARD deletes, counted first so the caller
 * can say what goes, then taken in one transaction so no reader sees a
 * capture half gone.
 *
 * A retire removes the population row, the capture's `capture_cuts` with
 * their `cut_cases`, `cut_refusals` and `cut_refusal_calls`, its runs with
 * their `case_results` and `run_rule_hits`, and the `job_cells` and
 * `request_cells` naming it. Content rows (`cases`, `case_documents`,
 * `flows_documents`, …) are content-addressed and shared, so none are
 * removed here: the cache under `_content/` is what the GC behind a removal
 * collects. A prep that cut nothing else keeps its row: it is a cut that
 * happened.
 *
 * A delete removes run rows alone; `job_cells` keep their outcomes, because
 * what a job ran is the job's own history, not the run's.
 *
 * Both are REFUSED while a running job holds what they would remove, read
 * inside the transaction: a replay whose cells name the capture, a cut or an
 * ingest whose arguments name it or take the whole population, a run whose
 * job is `running` or that is still open with no job to answer for it. A
 * running retire or delete holds nothing: the one asking is such a job.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Kind } from "./jobs.js"
import type { RunStatus } from "./model.js"
import * as Sessions from "./sessions.js"

/** The rows a retire takes, per table. */
export interface RetireRows {
  readonly population: number
  readonly captureCuts: number
  readonly cutCases: number
  readonly cutRefusals: number
  readonly cutRefusalCalls: number
  readonly runs: number
  readonly caseResults: number
  readonly runRuleHits: number
  readonly jobCells: number
  readonly requestCells: number
}

export const RETIRE_TABLES: ReadonlyArray<keyof RetireRows> = [
  "population",
  "captureCuts",
  "cutCases",
  "cutRefusals",
  "cutRefusalCalls",
  "runs",
  "caseResults",
  "runRuleHits",
  "jobCells",
  "requestCells"
]

/** A running job that names what a removal would take. */
export interface Holder {
  readonly jobId: number
  readonly kind: Kind
  /** How the job names it: its cells, its arguments, or a run of its own. */
  readonly how: string
}

/** A run as a removal sees it: enough to name its bundle and to say whether it may go. */
export interface RunEntry {
  readonly id: number
  readonly capture: string
  readonly lane: string
  readonly status: RunStatus
  /** Absolute. */
  readonly bundlePath: string
  readonly finishedAt: string | null
  readonly jobId: number | null
}

export interface RetirePlan {
  readonly capture: string
  readonly rows: RetireRows
  readonly runs: ReadonlyArray<RunEntry>
  readonly holders: ReadonlyArray<Holder>
}

/** The rows a delete takes, per table. */
export interface DeleteRows {
  readonly runs: number
  readonly caseResults: number
  readonly runRuleHits: number
}

export const DELETE_TABLES: ReadonlyArray<keyof DeleteRows> = ["runs", "caseResults", "runRuleHits"]

/** Why one run may not go now. */
export interface HeldRun {
  readonly id: number
  readonly why: string
}

export interface DeletePlan {
  readonly runs: ReadonlyArray<RunEntry>
  readonly rows: DeleteRows
  readonly held: ReadonlyArray<HeldRun>
}

/** Which runs a delete names. */
export type Selector =
  | { readonly _tag: "ids"; readonly ids: ReadonlyArray<number> }
  | { readonly _tag: "request"; readonly requestId: number }
  | { readonly _tag: "sweep"; readonly sweepId: number }
  /** Every run the job opened. */
  | { readonly _tag: "job"; readonly jobId: number }
  | { readonly _tag: "older"; readonly lane: string; readonly hours: number }

/** A removal refused because a running job holds what it names. */
export class Held extends Error {
  override readonly name = "Removal.Held"
  constructor(readonly holders: ReadonlyArray<Holder | HeldRun>, message: string) {
    super(message)
  }
}

export const isHeld = (error: unknown): error is Held => error instanceof Held

export interface Interface {
  /** What retiring each capture takes, read-only; a capture the store holds nothing of counts zero everywhere. */
  readonly retirePlan: (captures: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<RetirePlan>, SqlError>
  /** The rows of every capture, one transaction; fails `Held` — removing nothing — where a running job names one. */
  readonly retire: (captures: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<RetirePlan>, Held | SqlError>
  /** What deleting the selected runs takes, read-only, with the runs that may not go now. */
  readonly deletePlan: (selector: Selector) => Effect.Effect<DeletePlan, SqlError>
  /** The rows of these runs, one transaction; fails `Held` — removing nothing — where one may not go. */
  readonly deleteRuns: (ids: ReadonlyArray<number>) => Effect.Effect<DeletePlan, Held | SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Removal") {}

interface RunRow {
  readonly id: string
  readonly capture: string
  readonly lane: string
  readonly status: RunStatus
  readonly bundle_path: string
  readonly finished_at: Date | null
  readonly job_id: string | null
  readonly job_status: string | null
}

interface HolderRow {
  readonly job_id: string
  readonly kind: Kind
  readonly how: string
}

const n = (value: string | number | null | undefined): number => (value === null || value === undefined ? 0 : Number(value))

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    const runOf = (row: RunRow): RunEntry => ({
      id: Number(row.id),
      capture: row.capture,
      lane: row.lane,
      status: row.status,
      bundlePath: Sessions.fromStored(root, row.bundle_path),
      finishedAt: row.finished_at === null ? null : row.finished_at.toISOString(),
      jobId: row.job_id === null ? null : Number(row.job_id)
    })

    /**
     * The running jobs naming this capture: by a cell, by their arguments, or
     * by a whole-population cut. A retire or a delete holds nothing — the
     * one asking is running too, and two removals of one thing cost nothing.
     */
    const holdersOf = Effect.fn("Removal.holders")(function*(capture: string) {
      const rows = yield* sql<HolderRow>`
        SELECT j.id AS job_id, j.kind,
               CASE
                 WHEN EXISTS (SELECT 1 FROM job_cells c WHERE c.job_id = j.id AND c.capture = ${capture})
                   THEN 'a cell of it'
                 WHEN j.args->'captures' ? ${capture} THEN 'it is one of its captures'
                 WHEN EXISTS (
                   SELECT 1 FROM preps p JOIN capture_cuts cc ON cc.prep_id = p.id
                   WHERE p.job_id = j.id AND cc.capture = ${capture}
                 ) THEN 'its prep cut it'
                 ELSE 'it cuts the whole population'
               END AS how
        FROM jobs j
        WHERE j.status = 'running' AND j.kind NOT IN ('retire', 'delete') AND (
          EXISTS (SELECT 1 FROM job_cells c WHERE c.job_id = j.id AND c.capture = ${capture})
          OR j.args->'captures' ? ${capture}
          OR EXISTS (
            SELECT 1 FROM preps p JOIN capture_cuts cc ON cc.prep_id = p.id
            WHERE p.job_id = j.id AND cc.capture = ${capture}
          )
          OR (j.kind = 'cut' AND (
            coalesce((j.args->>'standing')::boolean, false)
            OR coalesce((j.args->>'all')::boolean, false)
            OR coalesce((j.args->>'known')::boolean, false)
          ))
        )
        ORDER BY j.id
      `
      return rows.map((row): Holder => ({ jobId: Number(row.job_id), kind: row.kind, how: row.how }))
    })

    const retireCounts = Effect.fn("Removal.retireCounts")(function*(capture: string) {
      const rows = yield* sql<Record<keyof RetireRows, string>>`
        SELECT
          (SELECT count(*) FROM population WHERE capture = ${capture}) AS "population",
          (SELECT count(*) FROM capture_cuts WHERE capture = ${capture}) AS "captureCuts",
          (SELECT count(*) FROM cut_cases WHERE capture = ${capture}) AS "cutCases",
          (SELECT count(*) FROM cut_refusals WHERE capture = ${capture}) AS "cutRefusals",
          (SELECT count(*) FROM cut_refusal_calls WHERE capture = ${capture}) AS "cutRefusalCalls",
          (SELECT count(*) FROM runs WHERE capture = ${capture}) AS "runs",
          (SELECT count(*) FROM case_results cr JOIN runs r ON r.id = cr.run_id WHERE r.capture = ${capture})
            AS "caseResults",
          (SELECT count(*) FROM run_rule_hits h JOIN runs r ON r.id = h.run_id WHERE r.capture = ${capture})
            AS "runRuleHits",
          (SELECT count(*) FROM job_cells WHERE capture = ${capture}) AS "jobCells",
          (SELECT count(*) FROM request_cells WHERE capture = ${capture}) AS "requestCells"
      `
      const row = rows[0]!
      const counts = {} as { -readonly [K in keyof RetireRows]: number }
      for (const table of RETIRE_TABLES) counts[table] = n(row[table])
      return counts as RetireRows
    })

    const runsOf = Effect.fn("Removal.runsOf")(function*(capture: string) {
      const rows = yield* sql<RunRow>`
        SELECT r.id, r.capture, r.lane, r.status, r.bundle_path, r.finished_at, r.job_id, j.status AS job_status
        FROM runs r LEFT JOIN jobs j ON j.id = r.job_id
        WHERE r.capture = ${capture} ORDER BY r.id
      `
      return rows.map(runOf)
    })

    const planOne = Effect.fn("Removal.retirePlanOne")(function*(capture: string) {
      return {
        capture,
        rows: yield* retireCounts(capture),
        runs: yield* runsOf(capture),
        holders: yield* holdersOf(capture)
      } satisfies RetirePlan
    })

    const retirePlan = Effect.fn("Removal.retirePlan")(function*(captures: ReadonlyArray<string>) {
      const plans: Array<RetirePlan> = []
      for (const capture of captures) plans.push(yield* planOne(capture))
      return plans
    })

    const retire = Effect.fn("Removal.retire")(function*(captures: ReadonlyArray<string>) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        const plans = yield* retirePlan(captures)
        const held = plans.filter((plan) => plan.holders.length > 0)
        if (held.length > 0) {
          return yield* Effect.fail(
            new Held(
              held.flatMap((plan) => plan.holders),
              held.map((plan) =>
                `${plan.capture} is held by ${
                  plan.holders.map((one) => `job ${one.jobId} (${one.kind}, ${one.how})`).join(", ")
                }`
              ).join("; ")
            )
          )
        }
        for (const capture of captures) {
          // Cascades take cut_cases, cut_refusals, cut_refusal_calls, case_results and run_rule_hits.
          yield* sql`DELETE FROM job_cells WHERE capture = ${capture}`
          yield* sql`DELETE FROM request_cells WHERE capture = ${capture}`
          yield* sql`DELETE FROM runs WHERE capture = ${capture}`
          yield* sql`DELETE FROM capture_cuts WHERE capture = ${capture}`
          yield* sql`DELETE FROM population WHERE capture = ${capture}`
        }
        return plans
      }))
    })

    const selected = (selector: Selector) => {
      switch (selector._tag) {
        case "ids":
          return selector.ids.length === 0 ? sql`false` : sql`r.id IN ${sql.in(selector.ids)}`
        case "request":
          return sql`j.request_id = ${selector.requestId}`
        case "sweep":
          return sql`r.sweep_id = ${selector.sweepId}`
        case "job":
          return sql`r.job_id = ${selector.jobId}`
        case "older":
          return sql`r.lane = ${selector.lane} AND r.finished_at IS NOT NULL
            AND r.finished_at < now() - make_interval(hours => ${selector.hours})`
      }
    }

    /** Why a run may not go: its job runs, or nothing has closed it yet. */
    const heldWhy = (row: RunRow): string | null =>
      row.job_status === "running"
        ? `its job ${row.job_id} is running`
        : row.finished_at === null
        ? row.job_id === null ? "it is still open and no job answers for it — reap first" : `it is still open under job ${row.job_id}`
        : null

    const deletePlan = Effect.fn("Removal.deletePlan")(function*(selector: Selector) {
      const rows = yield* sql<RunRow & { case_results: string; rule_hits: string }>`
        SELECT r.id, r.capture, r.lane, r.status, r.bundle_path, r.finished_at, r.job_id, j.status AS job_status,
               (SELECT count(*) FROM case_results cr WHERE cr.run_id = r.id) AS case_results,
               (SELECT count(*) FROM run_rule_hits h WHERE h.run_id = r.id) AS rule_hits
        FROM runs r LEFT JOIN jobs j ON j.id = r.job_id
        WHERE ${selected(selector)}
        ORDER BY r.id
      `
      const held: Array<HeldRun> = []
      let caseResults = 0
      let runRuleHits = 0
      for (const row of rows) {
        const why = heldWhy(row)
        if (why !== null) held.push({ id: Number(row.id), why })
        caseResults += n(row.case_results)
        runRuleHits += n(row.rule_hits)
      }
      return {
        runs: rows.map(runOf),
        rows: { runs: rows.length, caseResults, runRuleHits },
        held
      } satisfies DeletePlan
    })

    const deleteRuns = Effect.fn("Removal.deleteRuns")(function*(ids: ReadonlyArray<number>) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        const plan = yield* deletePlan({ _tag: "ids", ids })
        if (plan.held.length > 0) {
          return yield* Effect.fail(
            new Held(plan.held, plan.held.map((one) => `run ${one.id}: ${one.why}`).join("; "))
          )
        }
        if (plan.runs.length > 0) {
          // The cascade takes case_results and run_rule_hits; job_cells keep their outcomes.
          yield* sql`DELETE FROM runs WHERE id IN ${sql.in(plan.runs.map((run) => run.id))}`
        }
        return plan
      }))
    })

    return Service.of({ retirePlan, retire, deletePlan, deleteRuns })
  })
)
