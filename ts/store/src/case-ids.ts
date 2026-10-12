/**
 * The FROZEN case ids: which id each call of a capture is named by, for good.
 *
 * Two invariants, one per unique key of `case_ids`: a (capture, case id) names
 * one call key, and a (capture, call key) is named by one case id. A cut reads
 * the frozen ids of its captures before it names a call ({@link Interface.of}),
 * and its record freezes what it named ({@link freezeOn}, inside
 * `Cuts.record`'s transaction): a reading that names a frozen call by another
 * id, or a frozen id for another call, is refused whole — unless it states the
 * frozen key it re-keys the id from (`Entry.replaces`), and then every
 * request's cell of that key moves with it. A capture name is the
 * whole namespace: two captures, generations of one source included, share
 * nothing.
 *
 * A capture whose readings predate its frozen ids is SEEDED from them before
 * any new reading of it is recorded: {@link seedFromReadings} freezes every
 * keyed id of every reading, newest reading first, and a deployment that keys
 * the rest marks the capture seeded ({@link Interface.markSeeded}). Until it
 * is, `Cuts.record` refuses its readings, so no name a cut makes up before the
 * seed can take an id or a call for good. Seeding (`mode: "seed"`) keeps an id
 * or a call already frozen as it stands and counts what it skipped.
 */
import * as Context from "effect/Context"
import * as Data from "effect/Data"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"

/** One id a record names a call by. */
export interface Entry {
  readonly caseId: string
  readonly callKey: string
  /** What named it, where it is not the freeze's own origin. */
  readonly origin?: string
  /** The frozen key this entry re-keys its id from. */
  readonly replaces?: string
}

/** Where a call starts in its capture, as the case maps located it. */
export interface First {
  readonly at_us: number
  readonly src: string
  readonly dst: string
}

/** One frozen id, and where its call starts where the store located it. */
export interface FrozenId {
  readonly caseId: string
  readonly callKey: string
  readonly first: First | null
}

/** Where a freeze stops: the id or the call is frozen otherwise. */
export interface Clash {
  readonly caseId: string
  readonly callKey: string
  /** What the frozen record says instead. */
  readonly frozen: Entry
}

/** A strict freeze refused: the record names a frozen call or id otherwise. */
export class Conflict extends Data.TaggedError("CaseIds.Conflict")<{
  readonly capture: string
  readonly clashes: ReadonlyArray<Clash>
}> {
  override get message(): string {
    const shown = this.clashes.slice(0, 3).map((clash) =>
      `${clash.caseId} → ${clash.callKey} where ${clash.frozen.caseId} → ${clash.frozen.callKey} is frozen`
    )
    const more = this.clashes.length > 3 ? ` (and ${this.clashes.length - 3} more)` : ""
    return `${this.capture}: ${this.clashes.length} id(s) clash with the frozen ids: ${shown.join("; ")}${more}`
  }
}

/** A reading of a capture whose earlier readings were never seeded into the frozen ids. */
export class Unseeded extends Data.TaggedError("CaseIds.Unseeded")<{ readonly capture: string }> {
  override get message(): string {
    return `${this.capture}: its earlier readings' ids are not frozen yet — seed them first, ` +
      "or a name this reading makes up could take one of their calls for good"
  }
}

/** What a freeze did. */
export interface Frozen {
  /** Entries frozen by this call. */
  readonly added: number
  /** Entries the store already held exactly. */
  readonly held: number
  /** Seed mode: entries not frozen because their id or their call is frozen otherwise. */
  readonly skipped: number
  /** Ids re-keyed from the key their entry replaces. */
  readonly rekeyed: number
}

export type Mode = "strict" | "seed"

export interface Interface {
  /** Every frozen id of these captures, with where its call starts. A capture with none is absent. */
  readonly of: (captures: ReadonlyArray<string>) => Effect.Effect<ReadonlyMap<string, ReadonlyArray<FrozenId>>, SqlError>
  /**
   * Freezes `entries` of one capture under `origin` (what named them). Strict:
   * fails `Conflict`, freezing nothing, where one clashes. Seed: freezes what
   * does not clash and counts the rest.
   */
  readonly freeze: (
    capture: string,
    entries: ReadonlyArray<Entry>,
    origin: string,
    mode?: Mode
  ) => Effect.Effect<Frozen, Conflict | SqlError>
  /** States these captures seeded: their earlier readings' ids are all frozen. */
  readonly markSeeded: (captures: ReadonlyArray<string>) => Effect.Effect<void, SqlError>
  /** Those of these captures that hold readings and are not seeded. */
  readonly unseeded: (captures: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<string>, SqlError>
  /** How many ids are frozen, over how many captures. */
  readonly count: Effect.Effect<{ readonly ids: number; readonly captures: number }, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/CaseIds") {}

interface Row {
  readonly case_id: string
  readonly call_key: string
}

/**
 * Freezes `entries` of one capture on `sql`, in the caller's transaction where
 * it holds one: what {@link Interface.freeze} does, and what `Cuts.record`
 * runs inside its own.
 */
export const freezeOn = (
  sql: SqlClient,
  capture: string,
  entries: ReadonlyArray<Entry>,
  origin: string,
  mode: Mode = "strict"
): Effect.Effect<Frozen, Conflict | SqlError> =>
  Effect.gen(function*() {
    if (entries.length === 0) return { added: 0, held: 0, skipped: 0, rekeyed: 0 } satisfies Frozen
    // The entries travel as one JSON list, whatever their number.
    const offered = JSON.stringify(entries.map((entry) => ({ caseId: entry.caseId, callKey: entry.callKey })))
    const standing = yield* sql<Row>`
      WITH e AS (SELECT * FROM jsonb_to_recordset(${offered}::jsonb) AS e("caseId" text, "callKey" text))
      SELECT c.case_id, c.call_key FROM case_ids c
      WHERE c.capture = ${capture}
        AND (c.case_id IN (SELECT "caseId" FROM e) OR c.call_key IN (SELECT "callKey" FROM e))
    `
    const byId = new Map(standing.map((row) => [row.case_id, row.call_key]))
    const byKey = new Map(standing.map((row) => [row.call_key, row.case_id]))
    const fresh: Array<Entry> = []
    const rekeys: Array<Entry> = []
    const clashes: Array<Clash> = []
    let held = 0
    for (const entry of entries) {
      const keyOfId = byId.get(entry.caseId)
      const idOfKey = byKey.get(entry.callKey)
      if (keyOfId === entry.callKey) {
        held += 1
        continue
      }
      if (keyOfId !== undefined && keyOfId === entry.replaces && idOfKey === undefined) {
        byId.set(entry.caseId, entry.callKey)
        byKey.delete(keyOfId)
        byKey.set(entry.callKey, entry.caseId)
        rekeys.push(entry)
        continue
      }
      if (keyOfId !== undefined) {
        clashes.push({ ...entry, frozen: { caseId: entry.caseId, callKey: keyOfId } })
        continue
      }
      if (idOfKey !== undefined) {
        clashes.push({ ...entry, frozen: { caseId: idOfKey, callKey: entry.callKey } })
        continue
      }
      // Claimed here, so two entries of one record cannot both take an id or a call.
      byId.set(entry.caseId, entry.callKey)
      byKey.set(entry.callKey, entry.caseId)
      fresh.push(entry)
    }
    if (mode === "strict" && clashes.length > 0) return yield* new Conflict({ capture, clashes })
    if (rekeys.length > 0) {
      const moved = JSON.stringify(rekeys)
      yield* sql`
        UPDATE case_ids c SET call_key = e."callKey"
        FROM jsonb_to_recordset(${moved}::jsonb) AS e("caseId" text, "callKey" text)
        WHERE c.capture = ${capture} AND c.case_id = e."caseId"
      `
      // A request names a call by its key and survives every re-cut: its
      // cells move with the key, one cell per call where it named both.
      yield* sql`
        DELETE FROM request_cells c
        USING jsonb_to_recordset(${moved}::jsonb) AS e("callKey" text, replaces text)
        WHERE c.capture = ${capture} AND c.call_key = e.replaces
          AND EXISTS (
            SELECT 1 FROM request_cells d
            WHERE d.request_id = c.request_id AND d.capture = c.capture AND d.call_key = e."callKey"
          )
      `
      yield* sql`
        UPDATE request_cells c SET call_key = e."callKey"
        FROM jsonb_to_recordset(${moved}::jsonb) AS e("callKey" text, replaces text)
        WHERE c.capture = ${capture} AND c.call_key = e.replaces
      `
    }
    if (fresh.length > 0) {
      const rows = fresh.map((entry) => ({ caseId: entry.caseId, callKey: entry.callKey, origin: entry.origin ?? origin }))
      yield* sql`
        INSERT INTO case_ids (capture, case_id, call_key, origin)
        SELECT ${capture}, e."caseId", e."callKey", e.origin
        FROM jsonb_to_recordset(${JSON.stringify(rows)}::jsonb) AS e("caseId" text, "callKey" text, origin text)
      `
    }
    return { added: fresh.length, held, skipped: clashes.length, rekeyed: rekeys.length } satisfies Frozen
  })

/**
 * Freezes every keyed id of every reading the case maps hold, each capture's
 * newest reading first, keeping what is frozen already: what a store whose
 * readings predate its frozen ids runs once, in the migration that adds them.
 * Seeds nothing a reading names with no key — the deployment keys those and
 * marks the capture seeded.
 */
export const seedFromReadings: Effect.Effect<number, SqlError, SqlClient> = Effect.gen(function*() {
  const sql = yield* SqlClient
  const rows = yield* sql<{ n: number }>`
    WITH seeded AS (
      INSERT INTO case_ids (capture, case_id, call_key, origin)
      SELECT capture, case_id, call_key, 'prep' || prep_id FROM (
        SELECT DISTINCT ON (capture, case_id) capture, case_id, call_key, prep_id
        FROM case_maps WHERE call_key IS NOT NULL
        ORDER BY capture, case_id, prep_id DESC
      ) newest
      ORDER BY capture, prep_id DESC, case_id
      ON CONFLICT DO NOTHING
      RETURNING 1
    )
    SELECT count(*)::int AS n FROM seeded
  `
  return Number(rows[0]!.n)
})

/**
 * Whether `capture` holds a reading and is not seeded: a strict freeze of a
 * new reading of it must wait for the seed.
 */
export const unseededOn = (sql: SqlClient, capture: string) =>
  Effect.map(
    sql<{ unseeded: boolean }>`
      SELECT EXISTS (SELECT 1 FROM case_maps WHERE capture = ${capture})
             AND NOT EXISTS (SELECT 1 FROM frozen_captures WHERE capture = ${capture}) AS unseeded
    `,
    (rows) => rows[0]!.unseeded
  )

/** States `capture` seeded, on `sql`. */
export const markSeededOn = (sql: SqlClient, captures: ReadonlyArray<string>) =>
  captures.length === 0
    ? Effect.void
    : sql`
      INSERT INTO frozen_captures (capture)
      SELECT c FROM jsonb_array_elements_text(${JSON.stringify(captures)}::jsonb) c
      ON CONFLICT DO NOTHING
    `.pipe(Effect.asVoid)

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const of = Effect.fn("CaseIds.of")(function*(captures: ReadonlyArray<string>) {
      const out = new Map<string, Array<FrozenId>>()
      if (captures.length === 0) return out as ReadonlyMap<string, ReadonlyArray<FrozenId>>
      const rows = yield* sql<Row & {
        readonly capture: string
        readonly first_us: string | null
        readonly src: string | null
        readonly dst: string | null
      }>`
        SELECT i.capture, i.case_id, i.call_key, m.first_us, m.src, m.dst
        FROM case_ids i
        LEFT JOIN LATERAL (
          SELECT round(extract(epoch FROM first_at) * 1000000)::bigint AS first_us, src, dst
          FROM case_maps
          WHERE capture = i.capture AND case_id = i.case_id AND first_at IS NOT NULL
          ORDER BY prep_id DESC LIMIT 1
        ) m ON true
        WHERE i.capture IN ${sql.in(captures)}
      `
      for (const row of rows) {
        const own = out.get(row.capture) ?? []
        own.push({
          caseId: row.case_id,
          callKey: row.call_key,
          first: row.first_us === null
            ? null
            : { at_us: Number(row.first_us), src: row.src ?? "", dst: row.dst ?? "" }
        })
        out.set(row.capture, own)
      }
      return out as ReadonlyMap<string, ReadonlyArray<FrozenId>>
    })

    const freeze = (capture: string, entries: ReadonlyArray<Entry>, origin: string, mode: Mode = "strict") =>
      sql.withTransaction(freezeOn(sql, capture, entries, origin, mode)).pipe(
        Effect.withSpan("CaseIds.freeze")
      )

    const markSeeded = (captures: ReadonlyArray<string>) => markSeededOn(sql, captures)

    const unseeded = Effect.fn("CaseIds.unseeded")(function*(captures: ReadonlyArray<string>) {
      if (captures.length === 0) return []
      const rows = yield* sql<{ capture: string }>`
        SELECT DISTINCT m.capture FROM case_maps m
        WHERE m.capture IN ${sql.in(captures)}
          AND NOT EXISTS (SELECT 1 FROM frozen_captures f WHERE f.capture = m.capture)
        ORDER BY m.capture
      `
      return rows.map((row) => row.capture)
    })

    const count = Effect.gen(function*() {
      const rows = yield* sql<{ ids: number; captures: number }>`
        SELECT count(*)::int AS ids, count(DISTINCT capture)::int AS captures FROM case_ids
      `
      return { ids: Number(rows[0]!.ids), captures: Number(rows[0]!.captures) }
    })

    return Service.of({ of, freeze, markSeeded, unseeded, count })
  })
)
