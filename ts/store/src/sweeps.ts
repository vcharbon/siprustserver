/**
 * The SWEEP: one `evidence` invocation over many captures, as a row of its own.
 *
 * A run says what happened to one capture; a sweep says which invocation asked,
 * over which population, at which concurrency — and, through its runs, on
 * which tree. `selection` states the population as the predicates it was
 * drawn by, so two sweeps of one size over two draws are told apart.
 * `@sweep:N` names a sweep's runs as a run set (`selector.ts`).
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Provenance } from "./model.js"
import { UNKNOWN_PROVENANCE } from "./model.js"

/**
 * Which work list a sweep took: the whole universe, the standing cut alone, the
 * queue, a named list, or a replay request's cells.
 */
export type Universe = "all" | "standing" | "known" | "named" | "request"

export interface OpenRequest {
  readonly lane: string
  readonly universe: Universe
  /** The population as stated: the universe's switches and every predicate that narrowed it. */
  readonly selection: string
  readonly captures: number
  readonly concurrency: number
  readonly batchSize: number
  readonly memoryMax: string
}

/** One sweep as a reader sees it: its knobs, its population, its size, and the code its runs recorded. */
export interface Row {
  readonly id: number
  readonly startedAt: string
  readonly finishedAt: string | null
  readonly lane: string
  readonly universe: string
  /** Null for a sweep recorded before the population was stated. */
  readonly selection: string | null
  readonly captures: number
  readonly concurrency: number
  readonly batchSize: number
  readonly memoryMax: string
  readonly runs: number
  readonly code: Provenance
}

export interface Interface {
  readonly open: (request: OpenRequest) => Effect.Effect<number, SqlError>
  readonly finish: (id: number) => Effect.Effect<void, SqlError>
  readonly list: (limit: number) => Effect.Effect<ReadonlyArray<Row>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Sweeps") {}

interface SweepRow {
  readonly id: string
  readonly started_at: Date
  readonly finished_at: Date | null
  readonly lane: string
  readonly universe: string
  readonly selection: string | null
  readonly captures: number
  readonly concurrency: number
  readonly batch_size: number
  readonly memory_max: string
  readonly runs: number
  readonly codes: number
  readonly components: Provenance | null
}

/**
 * The code a sweep's runs recorded. One process opens every run of a sweep, so
 * a sweep has ONE provenance — and a sweep whose rows disagree is not given the
 * benefit of the doubt: its fingerprints are dropped and a reading of it says
 * `unknown` rather than picking one of them.
 */
const codeOf = (row: SweepRow): Provenance =>
  Number(row.codes) === 1 ? row.components ?? UNKNOWN_PROVENANCE : UNKNOWN_PROVENANCE

const rowOf = (row: SweepRow): Row => ({
  id: Number(row.id),
  startedAt: row.started_at.toISOString(),
  finishedAt: row.finished_at === null ? null : row.finished_at.toISOString(),
  lane: row.lane,
  universe: row.universe,
  selection: row.selection,
  captures: Number(row.captures),
  concurrency: Number(row.concurrency),
  batchSize: Number(row.batch_size),
  memoryMax: row.memory_max,
  runs: Number(row.runs),
  code: codeOf(row)
})

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    /** Every sweep, with its run count and the one code its runs recorded. */
    const summary = sql`
      SELECT s.id, s.started_at, s.finished_at, s.lane, s.universe, s.selection, s.captures,
             s.concurrency, s.batch_size, s.memory_max,
             count(r.id)::int AS runs,
             count(DISTINCT r.components)::int
               AS codes,
             (array_agg(r.components))[1] AS components
      FROM sweeps s LEFT JOIN runs r ON r.sweep_id = s.id
    `

    const open = Effect.fn("Sweeps.open")(function*(request: OpenRequest) {
      const rows = yield* sql<{ id: string }>`
        INSERT INTO sweeps (lane, universe, selection, captures, concurrency, batch_size, memory_max)
        VALUES (${request.lane}, ${request.universe}, ${request.selection}, ${request.captures},
                ${request.concurrency}, ${request.batchSize}, ${request.memoryMax})
        RETURNING id
      `
      return Number(rows[0]!.id)
    })

    const finish = Effect.fn("Sweeps.finish")(function*(id: number) {
      yield* sql`UPDATE sweeps SET finished_at = now() WHERE id = ${id}`
    })

    const list = Effect.fn("Sweeps.list")(function*(limit: number) {
      const found = yield* sql<SweepRow>`
        ${summary} GROUP BY s.id ORDER BY s.id DESC LIMIT ${limit}
      `
      return found.map(rowOf)
    })

    return Service.of({ open, finish, list })
  })
)
