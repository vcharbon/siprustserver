/**
 * What the STANDING CUT declares: how many of its case documents owe a failure,
 * and under which census rule each was derived.
 *
 * A declaration has no accept/reject axis — it is not a verdict on a run — so
 * the only thing measurable about one is how many cases carry it. Read off the
 * documents the newest cut of each capture produced, never off every document
 * ever stored: a rule retired two cuts ago would otherwise still read as live.
 * Counted once per distinct DOCUMENT, because a declaration lives in the
 * document and two cases of one document declare it once.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** One census rule, and the standing cut's cases it declared a failure on. */
export interface Declared {
  /** The census rule the declaration was derived from. */
  readonly rule: string
  /** `must_fail` entries carrying it. */
  readonly entries: number
  /** Distinct case documents carrying it. */
  readonly cases: number
}

/** How wide the reading is, so a zero is never mistaken for a clean sheet. */
export interface Population {
  readonly captures: number
  readonly cases: number
}

export interface Interface {
  /** Every rule the standing cut's documents declare a failure under. */
  readonly declared: Effect.Effect<ReadonlyArray<Declared>, SqlError>
  /** The standing cut itself: what the declarations were counted over. */
  readonly population: Effect.Effect<Population, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Declared") {}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    /** The newest cut of each capture: what the corpus stands cut as. */
    const standing = sql`
      SELECT DISTINCT ON (capture) prep_id, capture
      FROM capture_cuts
      ORDER BY capture, prep_id DESC
    `

    const declared = Effect.gen(function*() {
      const rows = yield* sql<{ rule: string; entries: string; cases: string }>`
        WITH standing AS (${standing}),
        held AS (
          SELECT DISTINCT cs.document_hash AS hash
          FROM cut_cases c
          JOIN standing s ON s.prep_id = c.prep_id AND s.capture = c.capture
          JOIN cases cs ON cs.hash = c.case_hash
        )
        SELECT e->>'derived_from' AS rule,
               count(*)::bigint AS entries,
               count(DISTINCT d.hash)::bigint AS cases
        FROM held h
        JOIN case_documents d ON d.hash = h.hash,
             jsonb_array_elements(d.document->'must_fail') e
        GROUP BY 1
      `
      return rows.map((row) => ({
        rule: row.rule,
        entries: Number(row.entries),
        cases: Number(row.cases)
      } satisfies Declared))
    })

    const population = Effect.gen(function*() {
      const rows = yield* sql<{ captures: string; cases: string }>`
        WITH standing AS (${standing})
        SELECT (SELECT count(*) FROM standing)::bigint AS captures,
               count(*)::bigint AS cases
        FROM cut_cases c
        JOIN standing s ON s.prep_id = c.prep_id AND s.capture = c.capture
      `
      const row = rows[0]
      return {
        captures: row === undefined ? 0 : Number(row.captures),
        cases: row === undefined ? 0 : Number(row.cases)
      } satisfies Population
    })

    return Service.of({ declared, population })
  })
)
