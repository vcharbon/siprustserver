/**
 * The WIPE: every cut and every run a regeneration remakes, dropped at once;
 * what cannot be remade, or was asked to stay, kept.
 *
 * Kept: the runs of the lanes the caller names (those whose runs cost real
 * time to remake), the pinned runs (`pins.ts`), every run still open, and with
 * them their case results and rule counts; the case documents; the frozen ids
 * (`case-ids.ts`), the case maps (`case-maps.ts`), the citations and the cell
 * summaries (`citations.ts`); the sweeps, jobs, requests and population.
 * Dropped: every other run, every prep with its cut rows, and the content only
 * cuts reference — cases, their assets and ledger rows, flows documents.
 *
 * {@link Interface.plan} is read-only and runs on a store that never took the
 * frozen-ids migration (a table it lacks counts as empty). {@link
 * Interface.execute} refuses, removing nothing, while a job runs or a run is
 * open, while a reading's case map is not written, or while a cited run it
 * drops has no summary: the export comes first, and a wipe cannot lose what a
 * citation names.
 */
import * as Context from "effect/Context"
import * as Data from "effect/Data"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Fragment } from "effect/unstable/sql/Statement"
import type { Citation } from "./citations.js"
import * as Sessions from "./sessions.js"

export interface Request {
  /** The lanes whose runs are kept whatever else holds. */
  readonly keepLanes: ReadonlyArray<string>
  /** Citations to honour beside those the store records: what a dry run reads without recording. */
  readonly citations?: ReadonlyArray<Citation>
}

/** The rows a wipe drops, per table. */
export interface Rows {
  readonly runs: number
  readonly caseResults: number
  readonly runRuleHits: number
  readonly preps: number
  readonly captureCuts: number
  readonly cutCases: number
  readonly cutRefusals: number
  readonly cutRefusalCalls: number
  readonly cases: number
  readonly caseAssets: number
  readonly ledgerRows: number
  readonly flowsDocuments: number
  readonly flowsLegs: number
}

export const ROW_TABLES: ReadonlyArray<keyof Rows> = [
  "runs",
  "caseResults",
  "runRuleHits",
  "preps",
  "captureCuts",
  "cutCases",
  "cutRefusals",
  "cutRefusalCalls",
  "cases",
  "caseAssets",
  "ledgerRows",
  "flowsDocuments",
  "flowsLegs"
]

export interface Plan {
  /** The runs kept, by the first reason that keeps each: its lane, its pin, still open. */
  readonly kept: { readonly lane: number; readonly pinned: number; readonly open: number }
  /** The case results of the kept runs, and how many of them no case map names. */
  readonly keptCells: number
  readonly unmapped: number
  /** Dropped runs a current citation names (by run, job or sweep), and how many of them hold no summary yet. */
  readonly cited: number
  readonly unsummarised: number
  readonly dropped: Rows
  /** The bundle of every dropped run, absolute. */
  readonly bundles: ReadonlyArray<string>
  /** Readings holding cut rows whose case map is not written. */
  readonly unexported: number
  /** Why the wipe may not run now: a running job, an open run. */
  readonly holders: ReadonlyArray<string>
}

/** A wipe refused, removing nothing. */
export class Refused extends Data.TaggedError("Wipe.Refused")<{ readonly reasons: ReadonlyArray<string> }> {
  override get message(): string {
    return `the wipe is refused: ${this.reasons.join("; ")}`
  }
}

export interface Interface {
  /**
   * When the store's ids began, `YYYY-MM-DD`: its first reading's cut, kept
   * or mapped; null for a store that never cut. A citation dated before it
   * names another store's run.
   */
  readonly since: Effect.Effect<string | null, SqlError>
  /** What a wipe would keep and drop, read-only, in a read-only transaction. */
  readonly plan: (request: Request) => Effect.Effect<Plan, SqlError>
  /** Drops what the plan names, one transaction; fails `Refused`, removing nothing, where it may not run. */
  readonly execute: (request: Request) => Effect.Effect<Plan, Refused | SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Wipe") {}

const n = (value: string | number | null | undefined): number => (value === null || value === undefined ? 0 : Number(value))

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    /** Which of the tables a migrated store holds this one has. */
    const tables = Effect.gen(function*() {
      const rows = yield* sql<{ pins: boolean; maps: boolean; citations: boolean; summaries: boolean }>`
        SELECT to_regclass('pins') IS NOT NULL AS pins, to_regclass('case_maps') IS NOT NULL AS maps,
               to_regclass('citations') IS NOT NULL AS citations,
               to_regclass('run_summaries') IS NOT NULL AS summaries
      `
      return rows[0]!
    })

    /** Whether run `r` is kept: its lane, its pin, still open. */
    const keptBy = (request: Request, pins: boolean): Fragment =>
      sql`(${
        request.keepLanes.length === 0 ? sql`false` : sql`r.lane IN ${sql.in(request.keepLanes)}`
      } OR r.finished_at IS NULL OR ${
        pins ? sql`EXISTS (SELECT 1 FROM pins p WHERE p.run_id = r.id)` : sql`false`
      })`

    const planIn = Effect.fn("Wipe.planIn")(function*(request: Request) {
      const has = yield* tables
      const kept = keptBy(request, has.pins)
      const given = JSON.stringify(request.citations ?? [])
      // The citations naming a run of this store: dated no earlier than its first cut.
      const cited = sql`
        WITH since AS (
          SELECT least(
            (SELECT min(cut_at) FROM capture_cuts),
            ${has.maps ? sql`(SELECT min(cut_at) FROM case_maps)` : sql`NULL::timestamptz`}
          )::date AS day
        ),
        named AS (
          SELECT c.kind, c.ref, c."citedOn"::date AS cited_on
          FROM jsonb_to_recordset(${given}::jsonb) AS c(kind text, ref text, "citedOn" text)
          ${has.citations ? sql`UNION ALL SELECT kind, ref, cited_on FROM citations` : sql``}
        )
        SELECT DISTINCT n.kind, n.ref::bigint AS id FROM named n, since
        WHERE n.kind IN ('run', 'job', 'sweep') AND n.ref ~ '^[0-9]{1,18}$'
          AND (n.cited_on IS NULL OR since.day IS NULL OR n.cited_on >= since.day)
      `
      const runs = yield* sql<{
        id: string
        bundle_path: string
        keep: string | null
        cited: boolean
        results: string
        rule_hits: string
        summarised: boolean
        unmapped: string
      }>`
        WITH cited AS MATERIALIZED (${cited}),
        cited_runs AS MATERIALIZED (
          SELECT r.id FROM runs r JOIN cited c ON c.kind = 'run' AND c.id = r.id
          UNION SELECT r.id FROM runs r JOIN cited c ON c.kind = 'job' AND c.id = r.job_id
          UNION SELECT r.id FROM runs r JOIN cited c ON c.kind = 'sweep' AND c.id = r.sweep_id
        )
        SELECT r.id, r.bundle_path,
               CASE
                 WHEN ${request.keepLanes.length === 0 ? sql`false` : sql`r.lane IN ${sql.in(request.keepLanes)}`} THEN 'lane'
                 WHEN ${has.pins ? sql`EXISTS (SELECT 1 FROM pins p WHERE p.run_id = r.id)` : sql`false`} THEN 'pinned'
                 WHEN r.finished_at IS NULL THEN 'open'
               END AS keep,
               r.id IN (SELECT id FROM cited_runs) AS cited,
               (SELECT count(*) FROM case_results cr WHERE cr.run_id = r.id) AS results,
               (SELECT count(*) FROM run_rule_hits h WHERE h.run_id = r.id) AS rule_hits,
               ${has.summaries
                 ? sql`EXISTS (SELECT 1 FROM run_summaries s WHERE s.run_id = r.id)`
                 : sql`false`} AS summarised,
               CASE WHEN ${kept} THEN (
                 SELECT count(*) FROM case_results cr
                 WHERE cr.run_id = r.id AND NOT EXISTS (${
                   has.maps
                     ? sql`SELECT 1 FROM case_maps m WHERE m.capture = r.capture AND m.case_id = cr.case_id`
                     : sql`SELECT 1 WHERE false`
                 })
               ) ELSE 0 END AS unmapped
        FROM runs r
      `
      let lane = 0
      let pinned = 0
      let open = 0
      let keptCells = 0
      let unmapped = 0
      let citedDropped = 0
      let unsummarised = 0
      let droppedRuns = 0
      let caseResults = 0
      let runRuleHits = 0
      const bundles: Array<string> = []
      for (const row of runs) {
        if (row.keep !== null) {
          if (row.keep === "lane") lane += 1
          else if (row.keep === "pinned") pinned += 1
          else open += 1
          keptCells += n(row.results)
          unmapped += n(row.unmapped)
          continue
        }
        droppedRuns += 1
        caseResults += n(row.results)
        runRuleHits += n(row.rule_hits)
        bundles.push(Sessions.fromStored(root, row.bundle_path))
        if (row.cited) {
          citedDropped += 1
          if (!row.summarised) unsummarised += 1
        }
      }
      const content = yield* sql<Record<string, string>>`
        SELECT (SELECT count(*) FROM preps) AS preps,
               (SELECT count(*) FROM capture_cuts) AS capture_cuts,
               (SELECT count(*) FROM cut_cases) AS cut_cases,
               (SELECT count(*) FROM cut_refusals) AS cut_refusals,
               (SELECT count(*) FROM cut_refusal_calls) AS cut_refusal_calls,
               (SELECT count(*) FROM cases) AS cases,
               (SELECT count(*) FROM case_assets) AS case_assets,
               (SELECT count(*) FROM ledger_rows) AS ledger_rows,
               (SELECT count(*) FROM flows_documents) AS flows_documents,
               (SELECT count(*) FROM flows_legs) AS flows_legs,
               (SELECT count(*) FROM capture_cuts cc
                WHERE (EXISTS (SELECT 1 FROM cut_cases c WHERE c.prep_id = cc.prep_id AND c.capture = cc.capture)
                       OR EXISTS (SELECT 1 FROM cut_refusals x WHERE x.prep_id = cc.prep_id AND x.capture = cc.capture))
                  AND ${has.maps
                    ? sql`NOT EXISTS (SELECT 1 FROM case_maps m WHERE m.prep_id = cc.prep_id AND m.capture = cc.capture)`
                    : sql`true`}) AS unexported,
               (SELECT count(*) FROM jobs WHERE status = 'running') AS running_jobs,
               (SELECT count(*) FROM runs WHERE finished_at IS NULL) AS open_runs
      `
      const c = content[0]!
      const holders: Array<string> = []
      if (n(c["running_jobs"]) > 0) holders.push(`${n(c["running_jobs"])} job(s) running`)
      if (n(c["open_runs"]) > 0) holders.push(`${n(c["open_runs"])} run(s) still open: reap them first`)
      return {
        kept: { lane, pinned, open },
        keptCells,
        unmapped,
        cited: citedDropped,
        unsummarised,
        dropped: {
          runs: droppedRuns,
          caseResults,
          runRuleHits,
          preps: n(c["preps"]),
          captureCuts: n(c["capture_cuts"]),
          cutCases: n(c["cut_cases"]),
          cutRefusals: n(c["cut_refusals"]),
          cutRefusalCalls: n(c["cut_refusal_calls"]),
          cases: n(c["cases"]),
          caseAssets: n(c["case_assets"]),
          ledgerRows: n(c["ledger_rows"]),
          flowsDocuments: n(c["flows_documents"]),
          flowsLegs: n(c["flows_legs"])
        },
        bundles,
        unexported: n(c["unexported"]),
        holders
      } satisfies Plan
    })

    const plan = Effect.fn("Wipe.plan")(function*(request: Request) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`SET TRANSACTION READ ONLY`
        return yield* planIn(request)
      }))
    })

    const execute = Effect.fn("Wipe.execute")(function*(request: Request) {
      const done = yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`SET LOCAL lock_timeout = '5s'`
        const planned = yield* planIn(request)
        const reasons = [...planned.holders]
        if (planned.unexported > 0) {
          reasons.push(`${planned.unexported} reading(s) have no case map: export them first`)
        }
        if (planned.unsummarised > 0) {
          reasons.push(`${planned.unsummarised} cited run(s) it drops have no summary: summarise them first`)
        }
        if (reasons.length > 0) return yield* new Refused({ reasons })
        const has = yield* tables
        yield* sql`DELETE FROM runs r WHERE NOT ${keptBy(request, has.pins)}`
        // Every cut and all the content only cuts reference, emptied at once: a
        // row-by-row delete pays a foreign-key probe per case into tables it
        // just emptied, and the case documents stay.
        yield* sql`
          TRUNCATE preps, capture_cuts, cut_cases, cut_refusals, cut_refusal_calls,
                   ledger_rows, case_assets, cases, flows_legs, flows_documents
        `
        return planned
      }))
      yield* sql`VACUUM ANALYZE runs, case_results, run_rule_hits`
      return done
    })

    const since = Effect.gen(function*() {
      const has = yield* tables
      const rows = yield* sql<{ at: Date | null }>`
        SELECT least(
          (SELECT min(cut_at) FROM capture_cuts),
          ${has.maps ? sql`(SELECT min(cut_at) FROM case_maps)` : sql`NULL::timestamptz`}
        ) AS at
      `
      const at = rows[0]?.at ?? null
      return at === null ? null : at.toISOString().slice(0, 10)
    })

    return Service.of({ since, plan, execute })
  })
)
