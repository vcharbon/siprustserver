/**
 * Every prep's CASE MAP: which call each case id of each capture named in that
 * prep, kept in `case_maps` after the prep's own cut rows go.
 *
 * A row is one case id of one (prep, capture) reading: a cut case with its call
 * key, or a refusal with the Call-IDs it names (and its call key where the cut
 * recorded one). `Cuts.record` writes the rows of each reading it records;
 * {@link Interface.export} copies the readings recorded before that, and
 * {@link Interface.locate} adds where each call starts in its capture — the
 * time and addresses of the first message of the leg its call key names —
 * read off the flows document the reading cut.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** What an export copied. */
export interface Exported {
  readonly cases: number
  readonly refusals: number
}

export interface Interface {
  /** Copies the readings whose rows `case_maps` does not hold yet; answers the rows copied. */
  readonly export: Effect.Effect<Exported, SqlError>
  /**
   * Fills the first message of each row not yet located, from the flows
   * document its reading cut; answers the rows located. A row whose flows
   * document the store no longer holds, or whose call names no leg of it,
   * stays unlocated.
   */
  readonly locate: Effect.Effect<number, SqlError>
  /** The readings holding cut rows whose map is not written: what a removal of cut rows must refuse on. */
  readonly unexported: Effect.Effect<number, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/CaseMaps") {}

/** How many flows documents one locate statement parses. */
const LOCATE_PAGE = 50

/**
 * The map rows of one (prep, capture) reading, replacing what stood: run by
 * `Cuts.record` inside its transaction, right after the reading's own rows.
 */
export const writeReading = (sql: SqlClient, prepId: number, capture: string) =>
  Effect.gen(function*() {
    yield* sql`DELETE FROM case_maps WHERE prep_id = ${prepId} AND capture = ${capture}`
    yield* sql`
      INSERT INTO case_maps (prep_id, capture, case_id, kind, call_key, cut_at)
      SELECT c.prep_id, c.capture, c.case_id, 'case', c.call_key, cc.cut_at
      FROM cut_cases c JOIN capture_cuts cc ON cc.prep_id = c.prep_id AND cc.capture = c.capture
      WHERE c.prep_id = ${prepId} AND c.capture = ${capture}
      ON CONFLICT DO NOTHING
    `
    yield* sql`
      INSERT INTO case_maps (prep_id, capture, case_id, kind, call_key, call_ids, reasons, cut_at)
      SELECT r.prep_id, r.capture, r.case_id, 'refused', r.call_key, r.call_ids, r.reasons, cc.cut_at
      FROM cut_refusals r JOIN capture_cuts cc ON cc.prep_id = r.prep_id AND cc.capture = r.capture
      WHERE r.prep_id = ${prepId} AND r.capture = ${capture}
      ON CONFLICT DO NOTHING
    `
  })

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const exportAll = Effect.gen(function*() {
      // A reading counts as exported once any of its rows is there: it is
      // written whole, by `writeReading` or by this statement.
      const cases = yield* sql<{ n: number }>`
        WITH copied AS (
          INSERT INTO case_maps (prep_id, capture, case_id, kind, call_key, cut_at)
          SELECT c.prep_id, c.capture, c.case_id, 'case', c.call_key, cc.cut_at
          FROM cut_cases c JOIN capture_cuts cc ON cc.prep_id = c.prep_id AND cc.capture = c.capture
          WHERE NOT EXISTS (
            SELECT 1 FROM case_maps m WHERE m.prep_id = c.prep_id AND m.capture = c.capture AND m.kind = 'case'
          )
          ON CONFLICT DO NOTHING
          RETURNING 1
        )
        SELECT count(*)::int AS n FROM copied
      `
      const refusals = yield* sql<{ n: number }>`
        WITH copied AS (
          INSERT INTO case_maps (prep_id, capture, case_id, kind, call_key, call_ids, reasons, cut_at)
          SELECT r.prep_id, r.capture, r.case_id, 'refused', r.call_key, r.call_ids, r.reasons, cc.cut_at
          FROM cut_refusals r JOIN capture_cuts cc ON cc.prep_id = r.prep_id AND cc.capture = r.capture
          WHERE NOT EXISTS (
            SELECT 1 FROM case_maps m WHERE m.prep_id = r.prep_id AND m.capture = r.capture AND m.kind = 'refused'
          )
          ON CONFLICT DO NOTHING
          RETURNING 1
        )
        SELECT count(*)::int AS n FROM copied
      `
      return { cases: Number(cases[0]!.n), refusals: Number(refusals[0]!.n) } satisfies Exported
    })

    const locatePage = (hashes: ReadonlyArray<string>) =>
      sql<{ n: number }>`
        WITH legs AS (
          SELECT l.flows_hash, p.leg->>'call_id' AS call_id,
                 (p.leg->'msgs'->0->>'ts_us')::bigint AS ts_us,
                 p.leg->'msgs'->0->>'src' AS src,
                 p.leg->'msgs'->0->>'dst' AS dst
          FROM flows_legs l,
               LATERAL (SELECT regexp_replace(convert_from(l.bytes, 'UTF8'), ',\\s*$', '')::jsonb AS leg) p
          WHERE l.flows_hash IN ${sql.in(hashes)}
        ),
        first AS (
          SELECT DISTINCT ON (flows_hash, call_id) flows_hash, call_id, ts_us, src, dst
          FROM legs WHERE ts_us IS NOT NULL
          ORDER BY flows_hash, call_id, ts_us
        ),
        located AS (
          UPDATE case_maps m
          SET first_at = to_timestamp(f.ts_us / 1000000.0), src = f.src, dst = f.dst
          FROM capture_cuts cc, first f
          WHERE m.first_at IS NULL
            AND cc.prep_id = m.prep_id AND cc.capture = m.capture AND cc.flows_hash IN ${sql.in(hashes)}
            AND f.flows_hash = cc.flows_hash
            AND f.call_id = coalesce(
              split_part(m.call_key, '#', 1),
              (SELECT id FROM unnest(m.call_ids) AS id ORDER BY length(id), id LIMIT 1)
            )
          RETURNING 1
        )
        SELECT count(*)::int AS n FROM located
      `

    const locate = Effect.gen(function*() {
      const todo = yield* sql<{ flows_hash: string }>`
        SELECT DISTINCT cc.flows_hash
        FROM case_maps m JOIN capture_cuts cc ON cc.prep_id = m.prep_id AND cc.capture = m.capture
        WHERE m.first_at IS NULL AND cc.flows_hash IS NOT NULL
        ORDER BY cc.flows_hash
      `
      const hashes = todo.map((row) => row.flows_hash)
      let located = 0
      for (let at = 0; at < hashes.length; at += LOCATE_PAGE) {
        const rows = yield* locatePage(hashes.slice(at, at + LOCATE_PAGE))
        located += Number(rows[0]!.n)
      }
      return located
    })

    const unexported = Effect.gen(function*() {
      const rows = yield* sql<{ n: number }>`
        SELECT count(*)::int AS n FROM capture_cuts cc
        WHERE (EXISTS (SELECT 1 FROM cut_cases c WHERE c.prep_id = cc.prep_id AND c.capture = cc.capture)
               OR EXISTS (SELECT 1 FROM cut_refusals r WHERE r.prep_id = cc.prep_id AND r.capture = cc.capture))
          AND NOT EXISTS (SELECT 1 FROM case_maps m WHERE m.prep_id = cc.prep_id AND m.capture = cc.capture)
      `
      return Number(rows[0]!.n)
    })

    return Service.of({ export: exportAll, locate, unexported })
  })
)
