/**
 * Every prep's CASE MAP: which call each case id of each capture named in that
 * prep, kept in `case_maps` after the prep's own cut rows go.
 *
 * A row is one case id of one (prep, capture) reading: a cut case with its call
 * key, or a refusal with the Call-IDs it names (and its call key where the cut
 * recorded one), and where the call starts in its capture — the time and
 * sockets of the first message of the leg its call key names. `Cuts.record`
 * writes the rows of each reading it records, with where each case's call
 * starts where the cut stated it; {@link exportReadings} copies the readings
 * recorded before that, and {@link Interface.locate} reads where each call
 * starts off the flows document its reading cut, for the rows that do not say.
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

/** One case id of one reading, as the map keeps it. */
export interface Row {
  readonly prepId: number
  readonly capture: string
  readonly caseId: string
  readonly kind: "case" | "refused"
  /** The call key; null for a refusal whose cut recorded none. */
  readonly callKey: string | null
  /** The Call-IDs a refusal names; empty for a case. */
  readonly callIds: ReadonlyArray<string>
}

/** Where one case's call starts, as a cut states it. */
export interface Start {
  readonly caseId: string
  readonly at_us: number
  readonly src: string
  readonly dst: string
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
  /** Every capture the maps name, in order. */
  readonly captures: Effect.Effect<ReadonlyArray<string>, SqlError>
  /** Every row of these captures, by capture, newest prep first, then case id. */
  readonly rows: (captures: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<Row>, SqlError>
  /** The readings of these captures the store still holds a flows document for, newest prep first. */
  readonly readings: (captures: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<Reading>, SqlError>
  /** States where these calls of one reading start, over what stood; answers the rows restated. */
  readonly restate: (prepId: number, capture: string, starts: ReadonlyArray<Start>) => Effect.Effect<number, SqlError>
}

/** One reading of a capture and the flows document it cut. */
export interface Reading {
  readonly prepId: number
  readonly capture: string
  readonly flowsHash: string
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/CaseMaps") {}

/** How many flows documents one locate statement parses. */
const LOCATE_PAGE = 50

/**
 * The map rows of one (prep, capture) reading, replacing what stood, with
 * where each case's call starts where the cut stated it: run by `Cuts.record`
 * inside its transaction, right after the reading's own rows.
 */
export const writeReading = (sql: SqlClient, prepId: number, capture: string, starts: ReadonlyArray<Start>) =>
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
    if (starts.length > 0) {
      yield* sql`
        UPDATE case_maps m
        SET first_at = to_timestamp(s.at_us / 1000000.0), src = s.src, dst = s.dst
        FROM jsonb_to_recordset(${JSON.stringify(starts)}::jsonb)
          AS s("caseId" text, at_us bigint, src text, dst text)
        WHERE m.prep_id = ${prepId} AND m.capture = ${capture} AND m.case_id = s."caseId"
      `
    }
  })

/**
 * Copies every reading the case maps do not hold yet out of the cut rows; a
 * reading counts as exported once any of its rows of a kind is there, being
 * written whole by {@link writeReading} or by this.
 */
export const exportReadings: Effect.Effect<Exported, SqlError, SqlClient> = Effect.gen(function*() {
  const sql = yield* SqlClient
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

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const exportAll = exportReadings.pipe(Effect.provideService(SqlClient, sql))

    // The rows to locate and the Call-ID each names are read first, so the
    // legs join them on equality alone.
    const locatePage = (hashes: ReadonlyArray<string>) =>
      sql<{ n: number }>`
        WITH targets AS MATERIALIZED (
          SELECT m.prep_id, m.capture, m.case_id, cc.flows_hash,
                 coalesce(
                   split_part(m.call_key, '#', 1),
                   (SELECT id FROM unnest(m.call_ids) AS id ORDER BY length(id), id LIMIT 1)
                 ) AS call_id
          FROM case_maps m JOIN capture_cuts cc ON cc.prep_id = m.prep_id AND cc.capture = m.capture
          WHERE m.first_at IS NULL AND cc.flows_hash IN ${sql.in(hashes)}
        ),
        legs AS MATERIALIZED (
          SELECT l.flows_hash, p.leg->>'call_id' AS call_id,
                 (p.leg->'msgs'->0->>'ts_us')::bigint AS ts_us,
                 p.leg->'msgs'->0->>'src' AS src,
                 p.leg->'msgs'->0->>'dst' AS dst
          FROM flows_legs l,
               LATERAL (SELECT regexp_replace(convert_from(l.bytes, 'UTF8'), ',\\s*$', '')::jsonb AS leg) p
          WHERE l.flows_hash IN ${sql.in(hashes)}
        ),
        first AS MATERIALIZED (
          SELECT DISTINCT ON (flows_hash, call_id) flows_hash, call_id, ts_us, src, dst
          FROM legs WHERE ts_us IS NOT NULL
          ORDER BY flows_hash, call_id, ts_us
        ),
        located AS (
          UPDATE case_maps m
          SET first_at = to_timestamp(f.ts_us / 1000000.0), src = f.src, dst = f.dst
          FROM targets t JOIN first f ON f.flows_hash = t.flows_hash AND f.call_id = t.call_id
          WHERE m.prep_id = t.prep_id AND m.capture = t.capture AND m.case_id = t.case_id
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

    const captures = Effect.gen(function*() {
      const rows = yield* sql<{ capture: string }>`SELECT DISTINCT capture FROM case_maps ORDER BY capture`
      return rows.map((row) => row.capture)
    })

    const rows = Effect.fn("CaseMaps.rows")(function*(of: ReadonlyArray<string>) {
      if (of.length === 0) return []
      const found = yield* sql<{
        prep_id: string
        capture: string
        case_id: string
        kind: "case" | "refused"
        call_key: string | null
        call_ids: ReadonlyArray<string>
      }>`
        SELECT prep_id, capture, case_id, kind, call_key, call_ids FROM case_maps
        WHERE capture IN ${sql.in(of)}
        ORDER BY capture, prep_id DESC, case_id
      `
      return found.map((row): Row => ({
        prepId: Number(row.prep_id),
        capture: row.capture,
        caseId: row.case_id,
        kind: row.kind,
        callKey: row.call_key,
        callIds: row.call_ids
      }))
    })

    const readings = Effect.fn("CaseMaps.readings")(function*(of: ReadonlyArray<string>) {
      if (of.length === 0) return []
      const found = yield* sql<{ prep_id: string; capture: string; flows_hash: string }>`
        SELECT cc.prep_id, cc.capture, cc.flows_hash
        FROM capture_cuts cc JOIN flows_documents d ON d.hash = cc.flows_hash
        WHERE cc.capture IN ${sql.in(of)}
        ORDER BY cc.capture, cc.prep_id DESC
      `
      return found.map((row): Reading => ({
        prepId: Number(row.prep_id),
        capture: row.capture,
        flowsHash: row.flows_hash
      }))
    })

    const restate = Effect.fn("CaseMaps.restate")(function*(
      prepId: number,
      capture: string,
      starts: ReadonlyArray<Start>
    ) {
      if (starts.length === 0) return 0
      const done = yield* sql<{ n: number }>`
        WITH restated AS (
          UPDATE case_maps m
          SET first_at = to_timestamp(s.at_us / 1000000.0), src = s.src, dst = s.dst
          FROM jsonb_to_recordset(${JSON.stringify(starts)}::jsonb)
            AS s("caseId" text, at_us bigint, src text, dst text)
          WHERE m.prep_id = ${prepId} AND m.capture = ${capture} AND m.case_id = s."caseId"
          RETURNING 1
        )
        SELECT count(*)::int AS n FROM restated
      `
      return Number(done[0]!.n)
    })

    return Service.of({ export: exportAll, locate, unexported, captures, rows, readings, restate })
  })
)
