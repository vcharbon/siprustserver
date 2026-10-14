/**
 * FORGETTING captures: by explicit decision, every row any table keys by a
 * capture name, the rows that hang off them, and the content only they
 * named. Unlike a retire, the history goes too: the frozen ids, the case
 * maps, the run and cell summaries and the seeded mark, so a cited id of a
 * forgotten capture resolves to nothing afterwards.
 *
 * Dropped, per capture: its population row; its cuts with their cut cases,
 * refusals and refusal calls; its runs with their case results, rule hits
 * and pins; the job and request cells naming it; its case maps, frozen ids,
 * seeded mark, run summaries and cell summaries. Then the content rows no
 * kept row names any more: cases with their assets and ledger rows, case
 * documents, flows documents with their legs. Preps, jobs, requests and
 * sweeps keep their rows: each is an invocation that happened.
 *
 * {@link Interface.plan} is read-only. {@link Interface.execute} plans again
 * inside its one transaction and refuses, removing nothing, where
 * {@link refusals} names a reason.
 */
import * as Context from "effect/Context"
import * as Data from "effect/Data"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Citation } from "./citations.js"
import * as Sessions from "./sessions.js"

/** Every table a forget drops rows of, in the order a report lists them. */
export const TABLES = [
  "population",
  "capture_cuts",
  "cut_cases",
  "cut_refusals",
  "cut_refusal_calls",
  "runs",
  "case_results",
  "run_rule_hits",
  "pins",
  "job_cells",
  "request_cells",
  "case_maps",
  "case_ids",
  "frozen_captures",
  "run_summaries",
  "cell_summaries",
  "cases",
  "case_assets",
  "ledger_rows",
  "case_documents",
  "flows_documents",
  "flows_legs"
] as const

export type Table = (typeof TABLES)[number]

/** The rows a forget drops, per table. */
export type Rows = Readonly<Record<Table, number>>

export interface Request {
  /** The capture names, exactly as the tables key them. */
  readonly captures: ReadonlyArray<string>
  /** Citations to report beside those the store records. */
  readonly citations?: ReadonlyArray<Citation>
  /** Whether captures still in the population may be forgotten too. */
  readonly population?: boolean
}

/** A run a forget drops. */
export interface Run {
  readonly id: number
  readonly capture: string
  /** Absolute. */
  readonly bundlePath: string
}

/** A pin a forget drops with its run. */
export interface Pin {
  readonly runId: number
  readonly ticket: string | null
}

/** A citation of a run, job, sweep or prep whose rows of a forgotten capture go. */
export interface Cited {
  readonly kind: "run" | "job" | "sweep" | "prep"
  readonly ref: string
  readonly citedBy: string
}

export interface Plan {
  /** The captures asked for, sorted, each once. */
  readonly captures: ReadonlyArray<string>
  /** Those of them still in the population. */
  readonly population: ReadonlyArray<string>
  readonly rows: Rows
  readonly runs: ReadonlyArray<Run>
  readonly pins: ReadonlyArray<Pin>
  readonly cited: ReadonlyArray<Cited>
  /** Why a forget may not run now: a running job, an unfinished prep, an open run of a capture it drops. */
  readonly holders: ReadonlyArray<string>
}

/** A forget refused, removing nothing. */
export class Refused extends Data.TaggedError("Forget.Refused")<{ readonly reasons: ReadonlyArray<string> }> {
  override get message(): string {
    return `the forget is refused: ${this.reasons.join("; ")}`
  }
}

/** Why a forget of `plan` may not run: no capture, a population capture not allowed, a holder. */
export const refusals = (plan: Plan, population: boolean): ReadonlyArray<string> => [
  ...(plan.captures.length === 0 ? ["no capture named: the names resolve to nothing"] : []),
  ...(plan.population.length > 0 && !population
    ? [`${plan.population.length} capture(s) still in the population, not allowed: ${plan.population.join(", ")}`]
    : []),
  ...plan.holders
]

export interface Interface {
  /** Every capture name a table keys a row by, sorted. */
  readonly names: Effect.Effect<ReadonlyArray<string>, SqlError>
  /** What forgetting the captures drops, read-only, in a read-only transaction. */
  readonly plan: (request: Request) => Effect.Effect<Plan, SqlError>
  /** Drops what the plan names, one transaction; fails `Refused`, removing nothing, where it may not run. */
  readonly execute: (request: Request) => Effect.Effect<Plan, Refused | SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Forget") {}

const n = (value: string | number | null | undefined): number => (value === null || value === undefined ? 0 : Number(value))

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    /** The names asked for, one row each. */
    const named = (captures: ReadonlyArray<string>) =>
      sql`SELECT DISTINCT value AS capture FROM jsonb_array_elements_text(${JSON.stringify(captures)}::jsonb)`

    /** The runs of the captures in relation `f`. */
    const runsOf = (f: string) => sql`SELECT id FROM runs WHERE capture IN (SELECT capture FROM ${sql(f)})`

    /** The case hashes only the captures in `f` name: no other cut case or job cell does. */
    const casesOf = (f: string) =>
      sql`
        SELECT case_hash AS hash FROM cut_cases WHERE capture IN (SELECT capture FROM ${sql(f)})
        EXCEPT SELECT case_hash FROM cut_cases WHERE capture NOT IN (SELECT capture FROM ${sql(f)})
        EXCEPT SELECT case_hash FROM job_cells
               WHERE case_hash IS NOT NULL AND capture NOT IN (SELECT capture FROM ${sql(f)})
      `

    /** The flows hashes only the cuts of the captures in `f` name. */
    const flowsOf = (f: string) =>
      sql`
        SELECT flows_hash AS hash FROM capture_cuts
        WHERE flows_hash IS NOT NULL AND capture IN (SELECT capture FROM ${sql(f)})
        EXCEPT SELECT flows_hash FROM capture_cuts
               WHERE flows_hash IS NOT NULL AND capture NOT IN (SELECT capture FROM ${sql(f)})
      `

    /** The case documents only the cases `gc` and the runs `fr` name. */
    const documentsOf = (fr: string, gc: string) =>
      sql`
        (SELECT document_hash AS hash FROM cases WHERE hash IN (SELECT hash FROM ${sql(gc)})
         UNION SELECT document_hash FROM case_results
               WHERE document_hash IS NOT NULL AND run_id IN (SELECT id FROM ${sql(fr)}))
        EXCEPT SELECT document_hash FROM cases WHERE hash NOT IN (SELECT hash FROM ${sql(gc)})
        EXCEPT SELECT document_hash FROM case_results
               WHERE document_hash IS NOT NULL AND run_id NOT IN (SELECT id FROM ${sql(fr)})
      `

    const names = Effect.gen(function*() {
      const rows = yield* sql<{ capture: string }>`
        SELECT capture FROM population
        UNION SELECT capture FROM capture_cuts
        UNION SELECT capture FROM runs
        UNION SELECT capture FROM job_cells
        UNION SELECT capture FROM request_cells
        UNION SELECT capture FROM case_maps
        UNION SELECT capture FROM case_ids
        UNION SELECT capture FROM frozen_captures
        UNION SELECT capture FROM run_summaries
        UNION SELECT capture FROM cell_summaries
        ORDER BY capture
      `
      return rows.map((row) => row.capture)
    })

    const planIn = Effect.fn("Forget.planIn")(function*(request: Request) {
      const captures = [...new Set(request.captures)].sort()
      const counted = yield* sql<Record<Table, string>>`
        WITH f AS MATERIALIZED (${named(captures)}),
        fr AS MATERIALIZED (${runsOf("f")}),
        gc AS MATERIALIZED (${casesOf("f")}),
        gf AS MATERIALIZED (${flowsOf("f")}),
        gd AS MATERIALIZED (${documentsOf("fr", "gc")})
        SELECT
          (SELECT count(*) FROM population WHERE capture IN (SELECT capture FROM f)) AS population,
          (SELECT count(*) FROM capture_cuts WHERE capture IN (SELECT capture FROM f)) AS capture_cuts,
          (SELECT count(*) FROM cut_cases WHERE capture IN (SELECT capture FROM f)) AS cut_cases,
          (SELECT count(*) FROM cut_refusals WHERE capture IN (SELECT capture FROM f)) AS cut_refusals,
          (SELECT count(*) FROM cut_refusal_calls WHERE capture IN (SELECT capture FROM f)) AS cut_refusal_calls,
          (SELECT count(*) FROM fr) AS runs,
          (SELECT count(*) FROM case_results WHERE run_id IN (SELECT id FROM fr)) AS case_results,
          (SELECT count(*) FROM run_rule_hits WHERE run_id IN (SELECT id FROM fr)) AS run_rule_hits,
          (SELECT count(*) FROM pins WHERE run_id IN (SELECT id FROM fr)) AS pins,
          (SELECT count(*) FROM job_cells WHERE capture IN (SELECT capture FROM f)) AS job_cells,
          (SELECT count(*) FROM request_cells WHERE capture IN (SELECT capture FROM f)) AS request_cells,
          (SELECT count(*) FROM case_maps WHERE capture IN (SELECT capture FROM f)) AS case_maps,
          (SELECT count(*) FROM case_ids WHERE capture IN (SELECT capture FROM f)) AS case_ids,
          (SELECT count(*) FROM frozen_captures WHERE capture IN (SELECT capture FROM f)) AS frozen_captures,
          (SELECT count(*) FROM run_summaries WHERE capture IN (SELECT capture FROM f)) AS run_summaries,
          (SELECT count(*) FROM cell_summaries WHERE capture IN (SELECT capture FROM f)) AS cell_summaries,
          (SELECT count(*) FROM gc) AS cases,
          (SELECT count(*) FROM case_assets WHERE case_hash IN (SELECT hash FROM gc)) AS case_assets,
          (SELECT count(*) FROM ledger_rows WHERE case_hash IN (SELECT hash FROM gc)) AS ledger_rows,
          (SELECT count(*) FROM gd) AS case_documents,
          (SELECT count(*) FROM gf) AS flows_documents,
          (SELECT count(*) FROM flows_legs WHERE flows_hash IN (SELECT hash FROM gf)) AS flows_legs
      `
      const row = counted[0]!
      const rows = Object.fromEntries(TABLES.map((table) => [table, n(row[table])])) as Rows

      const population = yield* sql<{ capture: string }>`
        SELECT capture FROM population WHERE capture IN (${named(captures)}) ORDER BY capture
      `
      const runs = yield* sql<{ id: string; capture: string; bundle_path: string }>`
        SELECT id, capture, bundle_path FROM runs WHERE capture IN (${named(captures)}) ORDER BY id
      `
      const pins = yield* sql<{ run_id: string; ticket: string | null }>`
        SELECT p.run_id, p.ticket FROM pins p JOIN runs r ON r.id = p.run_id
        WHERE r.capture IN (${named(captures)}) ORDER BY p.run_id
      `
      const given = JSON.stringify(request.citations ?? [])
      // A citation dated before the store's first cut names another store's id.
      const cited = yield* sql<{ kind: Cited["kind"]; ref: string; cited_by: string }>`
        WITH f AS MATERIALIZED (${named(captures)}),
        since AS (
          SELECT least((SELECT min(cut_at) FROM capture_cuts), (SELECT min(cut_at) FROM case_maps))::date AS day
        ),
        given AS (
          SELECT c.kind, c.ref, c."citedBy" AS cited_by, c."citedOn"::date AS cited_on
          FROM jsonb_to_recordset(${given}::jsonb) AS c(kind text, ref text, "citedBy" text, "citedOn" text)
          UNION SELECT kind, ref, cited_by, cited_on FROM citations
        ),
        ids AS (
          SELECT DISTINCT g.kind, g.ref, g.cited_by, g.ref::bigint AS id FROM given g, since
          WHERE g.kind IN ('run', 'job', 'sweep', 'prep') AND g.ref ~ '^[0-9]{1,18}$'
            AND (g.cited_on IS NULL OR since.day IS NULL OR g.cited_on >= since.day)
        ),
        fruns AS MATERIALIZED (
          SELECT id, job_id, sweep_id, prep_id FROM runs WHERE capture IN (SELECT capture FROM f)
          UNION ALL
          SELECT run_id, job_id, sweep_id, prep_id FROM run_summaries WHERE capture IN (SELECT capture FROM f)
        ),
        fjobs AS (
          SELECT job_id AS id FROM fruns
          UNION SELECT job_id FROM cell_summaries WHERE capture IN (SELECT capture FROM f)
          UNION SELECT job_id FROM job_cells WHERE capture IN (SELECT capture FROM f)
        ),
        fsweeps AS (
          SELECT sweep_id AS id FROM fruns
          UNION SELECT sweep_id FROM cell_summaries WHERE capture IN (SELECT capture FROM f)
        ),
        fpreps AS (
          SELECT prep_id AS id FROM fruns
          UNION SELECT prep_id FROM capture_cuts WHERE capture IN (SELECT capture FROM f)
          UNION SELECT prep_id FROM case_maps WHERE capture IN (SELECT capture FROM f)
        )
        SELECT i.kind, i.ref, i.cited_by FROM ids i
        WHERE (i.kind = 'run' AND i.id IN (SELECT id FROM fruns))
           OR (i.kind = 'job' AND i.id IN (SELECT id FROM fjobs WHERE id IS NOT NULL))
           OR (i.kind = 'sweep' AND i.id IN (SELECT id FROM fsweeps WHERE id IS NOT NULL))
           OR (i.kind = 'prep' AND i.id IN (SELECT id FROM fpreps WHERE id IS NOT NULL))
        ORDER BY i.kind, i.id, i.cited_by
      `
      const held = yield* sql<{ jobs: string | null; preps: string | null; runs: string | null }>`
        SELECT (SELECT string_agg(id::text, ', ' ORDER BY id) FROM jobs WHERE status = 'running') AS jobs,
               (SELECT string_agg(id::text, ', ' ORDER BY id) FROM preps WHERE finished_at IS NULL) AS preps,
               (SELECT string_agg(id::text, ', ' ORDER BY id) FROM runs
                WHERE finished_at IS NULL AND capture IN (${named(captures)})) AS runs
      `
      const h = held[0]!
      const holders: Array<string> = []
      if (h.jobs !== null) holders.push(`job(s) ${h.jobs} running`)
      if (h.preps !== null) holders.push(`prep(s) ${h.preps} not finished: close them first`)
      if (h.runs !== null) holders.push(`run(s) ${h.runs} of a capture it drops still open: reap them first`)
      return {
        captures,
        population: population.map((one) => one.capture),
        rows,
        runs: runs.map((one): Run => ({
          id: Number(one.id),
          capture: one.capture,
          bundlePath: Sessions.fromStored(root, one.bundle_path)
        })),
        pins: pins.map((one): Pin => ({ runId: Number(one.run_id), ticket: one.ticket })),
        cited: cited.map((one): Cited => ({ kind: one.kind, ref: one.ref, citedBy: one.cited_by })),
        holders
      } satisfies Plan
    })

    const plan = Effect.fn("Forget.plan")(function*(request: Request) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`SET TRANSACTION READ ONLY`
        return yield* planIn(request)
      }))
    })

    const execute = Effect.fn("Forget.execute")(function*(request: Request) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`SET LOCAL lock_timeout = '5s'`
        const planned = yield* planIn(request)
        const reasons = refusals(planned, request.population === true)
        if (reasons.length > 0) return yield* new Refused({ reasons })
        // The content sets are read before any row they are read from goes.
        yield* sql`CREATE TEMP TABLE forget_captures (capture text PRIMARY KEY) ON COMMIT DROP`
        yield* sql`INSERT INTO forget_captures ${named(planned.captures)}`
        yield* sql`CREATE TEMP TABLE forget_runs ON COMMIT DROP AS ${runsOf("forget_captures")}`
        yield* sql`CREATE TEMP TABLE forget_cases ON COMMIT DROP AS ${casesOf("forget_captures")}`
        yield* sql`CREATE TEMP TABLE forget_flows ON COMMIT DROP AS ${flowsOf("forget_captures")}`
        yield* sql`CREATE TEMP TABLE forget_documents ON COMMIT DROP AS ${documentsOf("forget_runs", "forget_cases")}`
        const of = sql`(SELECT capture FROM forget_captures)`
        yield* sql`DELETE FROM job_cells WHERE capture IN ${of}`
        yield* sql`DELETE FROM request_cells WHERE capture IN ${of}`
        // Cascades take case_results, run_rule_hits and pins.
        yield* sql`DELETE FROM runs WHERE id IN (SELECT id FROM forget_runs)`
        // Cascades take cut_cases, cut_refusals and cut_refusal_calls.
        yield* sql`DELETE FROM capture_cuts WHERE capture IN ${of}`
        yield* sql`DELETE FROM population WHERE capture IN ${of}`
        yield* sql`DELETE FROM case_maps WHERE capture IN ${of}`
        yield* sql`DELETE FROM case_ids WHERE capture IN ${of}`
        yield* sql`DELETE FROM frozen_captures WHERE capture IN ${of}`
        yield* sql`DELETE FROM run_summaries WHERE capture IN ${of}`
        yield* sql`DELETE FROM cell_summaries WHERE capture IN ${of}`
        yield* sql`DELETE FROM case_assets WHERE case_hash IN (SELECT hash FROM forget_cases)`
        yield* sql`DELETE FROM ledger_rows WHERE case_hash IN (SELECT hash FROM forget_cases)`
        yield* sql`DELETE FROM cases WHERE hash IN (SELECT hash FROM forget_cases)`
        yield* sql`DELETE FROM case_documents WHERE hash IN (SELECT hash FROM forget_documents)`
        // The cascade takes flows_legs.
        yield* sql`DELETE FROM flows_documents WHERE hash IN (SELECT hash FROM forget_flows)`
        return planned
      }))
    })

    return Service.of({ names, plan, execute })
  })
)
