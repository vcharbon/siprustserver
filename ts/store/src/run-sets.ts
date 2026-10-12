/**
 * RUN SETS: what a selector (`selector.ts`) names in the store — its runs, and
 * the cells a comparison reads off two of them.
 *
 * A set's cell of a (capture, case) is the latest run of the set holding it,
 * whatever that run's set kind, interrupted runs' landed cells included, no
 * older than the set's latest FINISHED full reading of the capture on the lane where
 * the set holds one — the rule `current_cells` states for `@current`, so
 * `compare @current` and what the queue calls current never disagree.
 *
 * A pair of sets is read narrowed to each other: only the captures both sets
 * ran are read at all, and only the calls both hold are loaded; what one side
 * alone holds is counted in the same pass, so `@env:X` against `@current`
 * costs X's captures, not the lane.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Fragment } from "effect/unstable/sql/Statement"
import type { Cell, Elsewhere, Side, SideSweep } from "./compare.js"
import { NoCurrentPopulation } from "./current.js"
import type { MainErrorKind, Provenance, RunStatus, SetKind } from "./model.js"
import type { Selector, Term } from "./selector.js"
import * as Sessions from "./sessions.js"

/** One run a selector names, as a listing shows it. */
export interface RunRow {
  readonly id: number
  readonly capture: string
  readonly lane: string
  readonly setKind: SetKind
  readonly status: RunStatus
  readonly env: string | null
  readonly branch: string | null
  /** The code that made the run, component by component. */
  readonly code: Provenance
  readonly sweepId: number | null
  readonly jobId: number | null
  readonly startedAt: string
  readonly cases: number
  readonly failed: number
  /** What the lane's post-run cleanup gate found on a batch its cells ran in; null where clean or not judged. */
  readonly cleanup: string | null
}

/** Two sides read against each other. */
export interface Pair {
  readonly before: Side
  readonly after: Side
}

export interface Interface {
  /**
   * Why a selector names no run set the store can answer for, or null: a
   * commit prefix that names more than one commit, listing them, or the
   * current term where the store states no current population.
   */
  readonly refusal: (selector: Selector) => Effect.Effect<string | null, SqlError>
  /** The runs a selector names, newest first, at most `limit`. */
  readonly runs: (selector: Selector, limit: number) => Effect.Effect<ReadonlyArray<RunRow>, SqlError>
  /**
   * Two sides narrowed to each other: each holds the cells of the calls both
   * sets ran, and counts what it alone holds (`only` keys listed at most).
   */
  readonly pair: (before: Selector, after: Selector, only: number) => Effect.Effect<Pair, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/RunSets") {}

interface CellRow {
  readonly side: "before" | "after"
  readonly capture: string
  readonly case_id: string
  readonly run_id: string
  readonly lane: string
  readonly env: string | null
  readonly branch: string | null
  readonly lane_hash: string | null
  readonly prep_id: string | null
  readonly bundle_path: string
  readonly passed: boolean
  readonly verdict: string
  readonly skipped: string | null
  readonly failed_step: string | null
  readonly document_hash: string | null
  readonly error_kind: string | null
  readonly error_rule: string | null
  readonly error_rfc: string | null
  readonly error_step: string | null
  readonly error_expected: string | null
  readonly error_detail: string | null
  readonly classification: Record<string, unknown> | null
  readonly components: Provenance | null
}

/** A side's size and the sweeps its runs belong to. */
interface Head {
  readonly runs: number
  readonly sweeps: ReadonlyArray<SideSweep>
}

interface OnlyRow {
  readonly side: "before" | "after"
  readonly captures: string
  readonly calls: string
  readonly keys: ReadonlyArray<string> | null
}

const count = (record: Record<string, unknown>, field: string): number => {
  const value = record[field]
  return typeof value === "number" ? value : Number(value ?? 0)
}

const cellOf = (root: Sessions.Root, row: CellRow): Cell => ({
  capture: row.capture,
  caseId: row.case_id,
  runId: Number(row.run_id),
  lane: row.lane,
  env: row.env,
  branch: row.branch,
  laneHash: row.lane_hash,
  prepId: row.prep_id === null ? null : Number(row.prep_id),
  bundlePath: Sessions.fromStored(root, row.bundle_path),
  passed: row.passed,
  verdict: row.verdict,
  skipped: row.skipped,
  failedStep: row.failed_step,
  documentHash: row.document_hash,
  mainError: row.error_kind === null
    ? null
    : {
      kind: row.error_kind as MainErrorKind,
      rule: row.error_rule,
      rfc: row.error_rfc,
      step: row.error_step,
      expected: row.error_expected,
      detail: row.error_detail ?? ""
    },
  deltas: row.classification === null
    ? null
    : {
      accepted: count(row.classification, "accepted"),
      knownBug: count(row.classification, "known_bug"),
      unlisted: count(row.classification, "unlisted"),
      unknown: count(row.classification, "unknown")
    },
  code: row.components ?? []
})

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    /** A run no component of which was dirty: its commits are its code. */
    const clean = sql`NOT EXISTS (SELECT 1 FROM jsonb_array_elements(r.components) k WHERE k->>'dirty' = 'true')`

    const termOf = (term: Term): Fragment => {
      switch (term._tag) {
        case "current":
          return sql`r.id IN (SELECT id FROM current_runs)`
        case "env":
          return sql`r.env = ${term.env}`
        case "run":
          return sql`r.id = ${term.id}`
        case "sha":
          return sql`(EXISTS (SELECT 1 FROM jsonb_array_elements(r.components) k
                              WHERE left(k->>'sha', ${term.prefix.length}) = ${term.prefix}) AND ${clean})`
        case "lane":
          return sql`r.lane = ${term.lane}`
        case "sweep":
          return sql`r.sweep_id = ${term.id}`
        case "job":
          return sql`r.job_id = ${term.id}`
      }
    }

    const where = (selector: Selector): Fragment => sql.and(selector.terms.map(termOf))

    const refusal = Effect.fn("RunSets.refusal")(function*(selector: Selector) {
      for (const term of selector.terms) {
        if (term._tag === "current") {
          const stated = yield* sql<{ one: boolean }>`SELECT one FROM current_population`
          if (stated.length === 0) return new NoCurrentPopulation().message
          continue
        }
        if (term._tag !== "sha") continue
        const shas = yield* sql<{ sha: string }>`
          SELECT DISTINCT k->>'sha' AS sha
          FROM runs r, jsonb_array_elements(r.components) k
          WHERE left(k->>'sha', ${term.prefix.length}) = ${term.prefix}
          ORDER BY sha
        `
        if (shas.length > 1) {
          return `@sha:${term.prefix} names ${shas.length} commits — ${shas.map((row) => row.sha).join(", ")}`
        }
      }
      return null
    })

    const runs = Effect.fn("RunSets.runs")(function*(selector: Selector, limit: number) {
      const rows = yield* sql<{
        id: string
        capture: string
        lane: string
        set_kind: SetKind
        status: RunStatus
        env: string | null
        branch: string | null
        components: Provenance
        sweep_id: string | null
        job_id: string | null
        started_at: Date
        cases: number
        failed: number
        cleanup: string | null
      }>`
        SELECT r.id, r.capture, r.lane, r.set_kind, r.status, r.env, r.branch, r.components,
               r.sweep_id, r.job_id, r.started_at, r.cases, r.failed, r.cleanup
        FROM runs r
        WHERE ${where(selector)}
        ORDER BY r.id DESC
        LIMIT ${limit}
      `
      return rows.map((row): RunRow => ({
        id: Number(row.id),
        capture: row.capture,
        lane: row.lane,
        setKind: row.set_kind,
        status: row.status,
        env: row.env,
        branch: row.branch,
        code: row.components,
        sweepId: row.sweep_id === null ? null : Number(row.sweep_id),
        jobId: row.job_id === null ? null : Number(row.job_id),
        startedAt: row.started_at.toISOString(),
        cases: Number(row.cases),
        failed: Number(row.failed),
        cleanup: row.cleanup
      }))
    })

    /** The runs, sweeps and size of one side, read off the runs table alone. */
    const headOf = Effect.fn("RunSets.headOf")(function*(selector: Selector) {
      const head = yield* sql<{ runs: string }>`SELECT count(*) AS runs FROM runs r WHERE ${where(selector)}`
      const sweeps = yield* sql<{ id: string; universe: string; selection: string | null; captures: number }>`
        SELECT s.id, s.universe, s.selection, s.captures
        FROM sweeps s
        WHERE s.id IN (SELECT DISTINCT r.sweep_id FROM runs r WHERE ${where(selector)} AND r.sweep_id IS NOT NULL)
        ORDER BY s.id
      `
      return {
        runs: Number(head[0]?.runs ?? 0),
        sweeps: sweeps.map((row): SideSweep => ({
          id: Number(row.id),
          universe: row.universe,
          selection: row.selection,
          captures: Number(row.captures)
        }))
      } satisfies Head
    })

    /**
     * The two sides' runs, the captures both ran, each side's live runs of
     * those captures (no older than its latest finished full reading per lane), each
     * side's cells of them, and the calls both hold.
     */
    const narrowed = (before: Selector, after: Selector) =>
      sql`
        a_runs AS (SELECT r.id, r.capture, r.lane, r.set_kind, r.status, r.finished_at FROM runs r WHERE ${where(before)}),
        b_runs AS (SELECT r.id, r.capture, r.lane, r.set_kind, r.status, r.finished_at FROM runs r WHERE ${where(after)}),
        shared AS (SELECT capture FROM a_runs INTERSECT SELECT capture FROM b_runs),
        a_live AS (
          SELECT x.id FROM (
            SELECT a.id,
                   max(a.id) FILTER (WHERE a.set_kind = 'full' AND a.status <> 'interrupted' AND a.finished_at IS NOT NULL)
                     OVER (PARTITION BY a.capture, a.lane) AS floor_id
            FROM a_runs a WHERE a.capture IN (SELECT capture FROM shared)
          ) x
          WHERE x.id >= coalesce(x.floor_id, 0)
        ),
        b_live AS (
          SELECT x.id FROM (
            SELECT b.id,
                   max(b.id) FILTER (WHERE b.set_kind = 'full' AND b.status <> 'interrupted' AND b.finished_at IS NOT NULL)
                     OVER (PARTITION BY b.capture, b.lane) AS floor_id
            FROM b_runs b WHERE b.capture IN (SELECT capture FROM shared)
          ) x
          WHERE x.id >= coalesce(x.floor_id, 0)
        ),
        a_cells AS (
          SELECT DISTINCT ON (r.capture, cr.case_id) r.id AS run_id, r.capture, cr.case_id
          FROM a_live l JOIN runs r ON r.id = l.id JOIN case_results cr ON cr.run_id = r.id
          ORDER BY r.capture, cr.case_id, r.id DESC
        ),
        b_cells AS (
          SELECT DISTINCT ON (r.capture, cr.case_id) r.id AS run_id, r.capture, cr.case_id
          FROM b_live l JOIN runs r ON r.id = l.id JOIN case_results cr ON cr.run_id = r.id
          ORDER BY r.capture, cr.case_id, r.id DESC
        ),
        both_hold AS (SELECT capture, case_id FROM a_cells INTERSECT SELECT capture, case_id FROM b_cells)
      `

    const pair = Effect.fn("RunSets.pair")(function*(before: Selector, after: Selector, only: number) {
      const cells = yield* sql<CellRow>`
        WITH ${narrowed(before, after)},
        picked AS (
          SELECT 'before'::text AS side, a.run_id, a.capture, a.case_id FROM a_cells a
          WHERE (a.capture, a.case_id) IN (SELECT capture, case_id FROM both_hold)
          UNION ALL
          SELECT 'after', b.run_id, b.capture, b.case_id FROM b_cells b
          WHERE (b.capture, b.case_id) IN (SELECT capture, case_id FROM both_hold)
        )
        SELECT p.side, p.capture, p.case_id, r.id AS run_id, r.lane, r.env, r.branch, r.lane_hash, r.prep_id,
               r.bundle_path, cr.passed, cr.verdict, cr.skipped, cr.failed_step, cr.document_hash,
               cr.error_kind, cr.error_rule, cr.error_rfc, cr.error_step, cr.error_expected, cr.error_detail,
               cr.classification,
               r.components
        FROM picked p
        JOIN runs r ON r.id = p.run_id
        JOIN case_results cr ON cr.run_id = p.run_id AND cr.case_id = p.case_id
        ORDER BY p.side, p.capture, p.case_id
      `
      const alone = yield* sql<OnlyRow>`
        WITH ${narrowed(before, after)}
        SELECT 'before'::text AS side,
               (SELECT count(DISTINCT capture) FROM a_runs WHERE capture NOT IN (SELECT capture FROM shared)) AS captures,
               (SELECT count(*) FROM (SELECT capture, case_id FROM a_cells EXCEPT SELECT * FROM both_hold) k) AS calls,
               (SELECT array_agg(capture || '/' || case_id ORDER BY capture, case_id) FROM (
                  SELECT capture, case_id FROM a_cells EXCEPT SELECT * FROM both_hold
                  ORDER BY capture, case_id LIMIT ${only}) k) AS keys
        UNION ALL
        SELECT 'after',
               (SELECT count(DISTINCT capture) FROM b_runs WHERE capture NOT IN (SELECT capture FROM shared)),
               (SELECT count(*) FROM (SELECT capture, case_id FROM b_cells EXCEPT SELECT * FROM both_hold) k),
               (SELECT array_agg(capture || '/' || case_id ORDER BY capture, case_id) FROM (
                  SELECT capture, case_id FROM b_cells EXCEPT SELECT * FROM both_hold
                  ORDER BY capture, case_id LIMIT ${only}) k)
      `
      const elsewhere = (side: "before" | "after"): Elsewhere => {
        const row = alone.find((one) => one.side === side)
        return {
          captures: Number(row?.captures ?? 0),
          calls: Number(row?.calls ?? 0),
          keys: row?.keys ?? []
        }
      }
      const sideOf = (selector: Selector, side: "before" | "after", head: Head): Side => ({
        selector: selector.text,
        runs: head.runs,
        sweeps: head.sweeps,
        cells: cells.filter((row) => row.side === side).map((row) => cellOf(root, row)),
        elsewhere: elsewhere(side)
      })
      return {
        before: sideOf(before, "before", yield* headOf(before)),
        after: sideOf(after, "after", yield* headOf(after))
      } satisfies Pair
    })

    return Service.of({ refusal, runs, pair })
  })
)
