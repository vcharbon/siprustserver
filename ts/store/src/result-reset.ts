/**
 * The RESULT RESET: every run and what hangs off it emptied at once, for a
 * store whose result schema changed and whose old results are not converted.
 *
 * Emptied: `runs`, and with them their case results, verification lines, rule
 * counts and pins; `batches` with their lines; the campaign runs with their
 * cells and what their cases said of themselves; and the deployment's own result
 * tables it names ({@link Request.tables}), with whatever references them.
 * Kept: the frozen ids, the case maps, the population, the cuts and their
 * content, the case documents, the citations and the summaries of cited runs,
 * the jobs and their attachments, sweeps and requests, and the bundles on disk. The sequences never
 * move back, so no run id a citation names is minted again.
 *
 * {@link Interface.plan} is read-only and lists the pins the reset drops.
 * {@link Interface.execute} refuses, removing nothing, while a job runs, a run
 * or a batch is open, or a cited run it drops has no summary; it holds `runs`
 * and `batches` exclusively from its check to its emptying, so none opens
 * between them.
 */
import * as Context from "effect/Context"
import * as Data from "effect/Data"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

export interface Request {
  /** The deployment's own result tables beside the store's; each is emptied with whatever references it. */
  readonly tables?: ReadonlyArray<string>
}

/** A pin the reset drops with its run. */
export interface DroppedPin {
  readonly runId: number
  readonly capture: string
  readonly lane: string
  readonly ticket: string | null
  readonly pinnedAt: string
}

export interface Plan {
  /** The rows emptied, per table of the store's. */
  readonly dropped: {
    readonly runs: number
    readonly caseResults: number
    readonly verifications: number
    readonly batches: number
  }
  readonly pins: ReadonlyArray<DroppedPin>
  /** Every table the reset empties: those it names and, transitively, every table referencing one of them. */
  readonly emptied: ReadonlyArray<string>
  /** The rows kept, for the reader to compare before and after. */
  readonly kept: {
    readonly caseIds: number
    readonly population: number
    readonly captureCuts: number
  }
  /** Dropped runs a citation names (by run, job or sweep) that hold no summary. */
  readonly unsummarised: number
  /** Why the reset may not run now: a running job, an open run, an open batch. */
  readonly holders: ReadonlyArray<string>
}

/** A reset refused, removing nothing. */
export class Refused extends Data.TaggedError("ResultReset.Refused")<{ readonly reasons: ReadonlyArray<string> }> {
  override get message(): string {
    return `the result reset is refused: ${this.reasons.join("; ")}`
  }
}

export interface Interface {
  /** What a reset would empty and keep, read-only. */
  readonly plan: (request: Request) => Effect.Effect<Plan, SqlError>
  /** Empties what the plan names, one transaction; fails `Refused`, removing nothing, where it may not run. */
  readonly execute: (request: Request) => Effect.Effect<Plan, Refused | SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/ResultReset") {}

const IDENTIFIER = /^[a-z_][a-z0-9_]*$/

/** The store's own result tables a reset names; what references them goes with them. */
export const STORE_TABLES: ReadonlyArray<string> = ["runs", "batches", "campaign_runs"]

const n = (value: string | number | null | undefined): number => (value === null || value === undefined ? 0 : Number(value))

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const planIn = (named: ReadonlyArray<string>) => Effect.gen(function*() {
      const reached = yield* sql<{ name: string }>`
        WITH RECURSIVE reached (oid) AS (
          SELECT oid FROM pg_class WHERE relkind = 'r' AND relnamespace = current_schema()::regnamespace
            AND relname IN ${sql.in([...named])}
          UNION
          SELECT c.conrelid FROM pg_constraint c JOIN reached r ON c.confrelid = r.oid WHERE c.contype = 'f'
        )
        SELECT relname AS name FROM pg_class WHERE oid IN (SELECT oid FROM reached) ORDER BY relname
      `
      const counts = yield* sql<Record<string, string | null>>`
        SELECT (SELECT count(*) FROM runs) AS runs,
               (SELECT count(*) FROM case_results) AS case_results,
               (SELECT count(*) FROM verifications) AS verifications,
               (SELECT count(*) FROM batches) AS batches,
               (SELECT count(*) FROM case_ids) AS case_ids,
               (SELECT count(*) FROM population) AS population,
               (SELECT count(*) FROM capture_cuts) AS capture_cuts,
               (SELECT count(*) FROM jobs WHERE status = 'running') AS running_jobs,
               (SELECT count(*) FROM runs WHERE finished_at IS NULL) AS open_runs,
               (SELECT count(*) FROM batches WHERE finished_at IS NULL) AS open_batches,
               (SELECT count(*) FROM runs r
                WHERE NOT EXISTS (SELECT 1 FROM run_summaries s WHERE s.run_id = r.id)
                  AND EXISTS (
                    SELECT 1 FROM citations c
                    WHERE (c.kind = 'run' AND c.ref = r.id::text)
                       OR (c.kind = 'job' AND c.ref = r.job_id::text)
                       OR (c.kind = 'sweep' AND c.ref = r.sweep_id::text)
                  )) AS unsummarised
      `
      const c = counts[0]!
      const pins = yield* sql<{ run_id: string; capture: string; lane: string; ticket: string | null; pinned_at: Date }>`
        SELECT p.run_id, r.capture, r.lane, p.ticket, p.pinned_at
        FROM pins p JOIN runs r ON r.id = p.run_id
        ORDER BY p.run_id
      `
      const holders: Array<string> = []
      if (n(c["running_jobs"]) > 0) holders.push(`${n(c["running_jobs"])} job(s) running`)
      if (n(c["open_runs"]) > 0) holders.push(`${n(c["open_runs"])} run(s) still open: reap them first`)
      if (n(c["open_batches"]) > 0) holders.push(`${n(c["open_batches"])} batch(es) still open: close them first`)
      return {
        dropped: {
          runs: n(c["runs"]),
          caseResults: n(c["case_results"]),
          verifications: n(c["verifications"]),
          batches: n(c["batches"])
        },
        pins: pins.map((pin) => ({
          runId: Number(pin.run_id),
          capture: pin.capture,
          lane: pin.lane,
          ticket: pin.ticket,
          pinnedAt: pin.pinned_at.toISOString()
        })),
        emptied: reached.map((row) => row.name),
        kept: {
          caseIds: n(c["case_ids"]),
          population: n(c["population"]),
          captureCuts: n(c["capture_cuts"])
        },
        unsummarised: n(c["unsummarised"]),
        holders
      } satisfies Plan
    })

    const namedBy = (request: Request): ReadonlyArray<string> => [...STORE_TABLES, ...(request.tables ?? [])]

    const plan = Effect.fn("ResultReset.plan")(function*(request: Request) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`SET TRANSACTION READ ONLY`
        return yield* planIn(namedBy(request))
      }))
    })

    const execute = Effect.fn("ResultReset.execute")(function*(request: Request) {
      const named = namedBy(request)
      const misnamed = named.filter((table) => !IDENTIFIER.test(table))
      if (misnamed.length > 0) return yield* new Refused({ reasons: [`not a table name: ${misnamed.join(", ")}`] })
      return yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`SET LOCAL lock_timeout = '5s'`
        yield* sql`LOCK TABLE runs, batches IN EXCLUSIVE MODE`
        const planned = yield* planIn(named)
        const reasons = [...planned.holders]
        if (planned.unsummarised > 0) {
          reasons.push(`${planned.unsummarised} cited run(s) it drops have no summary: summarise them first`)
        }
        if (reasons.length > 0) return yield* new Refused({ reasons })
        yield* sql.unsafe(`TRUNCATE ${named.join(", ")} CASCADE`)
        return planned
      }))
    })

    return Service.of({ plan, execute })
  })
)
