/**
 * The DELTA: cells that pass on one lane and fail on another.
 *
 * The program's stated end is the list of calls green on the fake clock and red
 * on real network + real clock, and every fact it needs is already here: the
 * list is a join of each lane's current cells (`current_cells`, `@current`
 * runs only); `compare @current/@lane:A @current/@lane:B` classes the same
 * cells call by call.
 * Nothing is stored: a committed report would go stale the moment either lane
 * ran again, and this list MOVES — it shrinks as the SUT is fixed and changes
 * whenever either side re-runs.
 *
 * Per CELL, never per capture: a capture that cut several calls is several
 * independent questions, and the step the real lane died at belongs to one of
 * them.
 *
 * A cell the REFERENCE lane skipped is not in the delta — it has no green to be
 * red against — but it is not nothing either: the fake lane blocks cells the
 * strict lanes run, so a failure there could never reach the list at all. Those
 * are answered separately ({@link Interface.unasked}) rather than dropped, which
 * is how five of the six failures of [182]'s ramp would otherwise have gone
 * unlisted.
 *
 * Like every other reader here, this holds no judgment: it says which cells
 * disagree, never what the disagreement means.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** One cell of the delta: green on `against`, failing on `lane`. */
export interface Row {
  readonly capture: string
  readonly caseId: string
  /** The step the failing lane died at; `null` where it named none (a stall, a crash). */
  readonly failedStep: string | null
  readonly verdict: string
  /** The run that says so, and the sweep it belonged to — where to go and look. */
  readonly runId: number
  readonly sweepId: number | null
}

/**
 * What the list was taken over, so a length is never read as a census.
 *
 * `compared` is the cells BOTH lanes ran, which is the only population the
 * question is asked of: the real lane has run a fraction of the corpus, and a
 * delta of 6 means nothing without the 1244 it was drawn from.
 */
export interface Population {
  readonly lane: string
  readonly against: string
  /** Cells both lanes ran, neither skipping. */
  readonly compared: number
  /** Of those, the ones green on `against` — what the delta is a subset of. */
  readonly green: number
  /** The sweeps the failing lane's rows come from: one reading, many invocations. */
  readonly sweeps: ReadonlyArray<number>
  /** Cells `lane` ran that `against` SKIPPED — outside the delta, and not nothing. */
  readonly unasked: number
}

export interface Interface {
  /** The cells green on `against` and failing on `lane`, in capture order. */
  readonly rows: (lane: string, against: string) => Effect.Effect<ReadonlyArray<Row>, SqlError>
  /**
   * Cells failing on `lane` that `against` never ran, in capture order. Not the
   * delta — nothing says these ever passed anywhere — but the only other place a
   * strict-lane failure can be, so a reader who works the delta to zero is not
   * left believing the lane is clean.
   */
  readonly unasked: (lane: string, against: string) => Effect.Effect<ReadonlyArray<Row>, SqlError>
  readonly population: (lane: string, against: string) => Effect.Effect<Population, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Delta") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    /**
     * Each lane's CURRENT cells, crossed on the cells they
     * share: what the corpus stands at — a cell's older runs are how it got
     * here, not what it says now.
     */
    const crossed = (lane: string, against: string) =>
      sql`
        WITH failing AS (
          SELECT capture, run_id, sweep_id, case_id, passed, skipped, failed_step, verdict
          FROM current_cells
          WHERE lane = ${lane}
        ),
        reference AS (
          SELECT capture, case_id, passed, skipped
          FROM current_cells
          WHERE lane = ${against}
        )
        SELECT f.capture, f.case_id, f.failed_step, f.verdict, f.run_id, f.sweep_id,
               f.passed AS lane_passed, f.skipped AS lane_skipped,
               r.passed AS ref_passed, r.skipped AS ref_skipped
        FROM failing f
        JOIN reference r ON r.capture = f.capture AND r.case_id = f.case_id
        WHERE f.skipped IS NULL
      `

    /** `blockedThere` picks the other population: cells `against` never ran. */
    const listing = (lane: string, against: string, blockedThere: boolean) =>
      Effect.map(
        sql<{
          capture: string
          case_id: string
          failed_step: string | null
          verdict: string
          run_id: string
          sweep_id: string | null
        }>`
          SELECT capture, case_id, failed_step, verdict, run_id, sweep_id
          FROM (${crossed(lane, against)}) x
          WHERE NOT lane_passed AND ${
          blockedThere ? sql`ref_skipped IS NOT NULL` : sql`ref_skipped IS NULL AND ref_passed`
        }
          ORDER BY capture, case_id
        `,
        (found) =>
          found.map((row) => ({
            capture: row.capture,
            caseId: row.case_id,
            failedStep: row.failed_step,
            verdict: row.verdict,
            runId: Number(row.run_id),
            sweepId: row.sweep_id === null ? null : Number(row.sweep_id)
          } satisfies Row))
      )

    const rows = Effect.fn("Delta.rows")(function*(lane: string, against: string) {
      return yield* listing(lane, against, false)
    })

    const unasked = Effect.fn("Delta.unasked")(function*(lane: string, against: string) {
      return yield* listing(lane, against, true)
    })

    const population = Effect.fn("Delta.population")(function*(lane: string, against: string) {
      const counted = yield* sql<{ compared: string; green: string; unasked: string }>`
        SELECT count(*) FILTER (WHERE ref_skipped IS NULL)::bigint AS compared,
               count(*) FILTER (WHERE ref_skipped IS NULL AND ref_passed)::bigint AS green,
               count(*) FILTER (WHERE ref_skipped IS NOT NULL AND NOT lane_passed)::bigint AS unasked
        FROM (${crossed(lane, against)}) x
      `
      const sweeps = yield* sql<{ sweep_id: string }>`
        SELECT DISTINCT sweep_id FROM (${crossed(lane, against)}) x
        WHERE sweep_id IS NOT NULL ORDER BY sweep_id
      `
      const row = counted[0]
      return {
        lane,
        against,
        compared: row === undefined ? 0 : Number(row.compared),
        green: row === undefined ? 0 : Number(row.green),
        sweeps: sweeps.map((entry) => Number(entry.sweep_id)),
        unasked: row === undefined ? 0 : Number(row.unasked)
      } satisfies Population
    })

    return Service.of({ rows, unasked, population })
  })
)

/**
 * The list's own shape: how many cells died at each step, widest first.
 *
 * One line, because a hundred cells that all died at `s6` is one question and
 * reading it off a hundred rows is how it gets missed. `-` is a cell that named
 * no step — a stall or a crash, which is a different kind of answer.
 */
const shape = (rows: ReadonlyArray<Row>): string => {
  const counts = new Map<string, number>()
  for (const row of rows) {
    const key = row.failedStep ?? "-"
    counts.set(key, (counts.get(key) ?? 0) + 1)
  }
  return [...counts]
    .sort((left, right) => right[1] - left[1] || (left[0] < right[0] ? -1 : 1))
    .map(([step, count]) => `${step}×${count}`)
    .join("  ")
}

/** The sweeps a reading spans, as a range where it is one and a list where it is few. */
const spanOf = (sweeps: ReadonlyArray<number>): string => {
  if (sweeps.length === 0) return "no sweep"
  if (sweeps.length === 1) return `sweep ${sweeps[0]}`
  return `sweeps ${sweeps[0]}..${sweeps[sweeps.length - 1]} (${sweeps.length})`
}

/**
 * The list, with the population it was drawn from stated above it.
 *
 * The header is not decoration: a count of failing cells is unreadable without
 * how many were asked, and this list is a moving object rather than a census, so
 * every printing says which runs it read.
 */
const listed = (rows: ReadonlyArray<Row>, limit: number): ReadonlyArray<string> => {
  const shown = rows.slice(0, limit)
  const width = Math.max(...shown.map((row) => row.caseId.length))
  const body = shown.map((row) =>
    `  ${row.caseId.padEnd(width)}  ${(row.failedStep ?? "-").padStart(4)}  ` +
    `${row.verdict}  run${row.runId}`
  )
  return rows.length > shown.length
    ? [...body, `  … ${rows.length - shown.length} more (--limit ${rows.length} for all)`]
    : body
}

export const render = (
  population: Population,
  rows: ReadonlyArray<Row>,
  unasked: ReadonlyArray<Row>,
  limit: number
): string => {
  const out = [
    `[delta] green on ${population.against}, red on ${population.lane}: ` +
    `${rows.length} cell(s)`,
    `[delta] over ${population.compared} cell(s) both lanes ran, ` +
    `${population.green} of them green on ${population.against}, from ${spanOf(population.sweeps)}`
  ]
  if (rows.length === 0) out.push("[delta] nothing to work")
  else {
    out.push(`[delta] by step: ${shape(rows)}`)
    out.push(...listed(rows, limit))
  }
  // The other place a strict-lane failure can be. Stated whenever there is one:
  // a reader who works the delta to zero must not read that as a clean lane.
  if (unasked.length > 0) {
    out.push(
      `[delta] BESIDE IT: ${unasked.length} cell(s) failing on ${population.lane} that ` +
      `${population.against} never ran (blocked there), so no delta can ever list them`,
      `[delta] by step: ${shape(unasked)}`,
      ...listed(unasked, limit)
    )
  }
  return out.join("\n")
}
