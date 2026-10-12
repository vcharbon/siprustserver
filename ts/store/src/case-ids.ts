/**
 * The FROZEN case ids: which id each call of a capture is named by, for good.
 *
 * Two invariants, one per unique key of `case_ids`: a (capture, case id) names
 * one call key, and a (capture, call key) is named by one case id. A cut reads
 * the frozen ids of its captures before it names a call ({@link Interface.of}),
 * and its record freezes what it named ({@link Interface.freeze}, inside
 * `Cuts.record`'s transaction): a reading that names a frozen call by another
 * id, or a frozen id for another call, is refused whole. A capture name is the
 * whole namespace: two captures, generations of one source included, share
 * nothing.
 *
 * Seeding (`mode: "seed"`) offers ids an older record holds: an id or a call
 * already frozen is kept as it stands, and what was offered and not frozen is
 * counted, never an error.
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

/** What a freeze did. */
export interface Frozen {
  /** Entries frozen by this call. */
  readonly added: number
  /** Entries the store already held exactly. */
  readonly held: number
  /** Seed mode: entries not frozen because their id or their call is frozen otherwise. */
  readonly skipped: number
}

export type Mode = "strict" | "seed"

export interface Interface {
  /** Every frozen id of these captures: capture → (call key → case id). A capture with none is absent. */
  readonly of: (
    captures: ReadonlyArray<string>
  ) => Effect.Effect<ReadonlyMap<string, ReadonlyMap<string, string>>, SqlError>
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
    if (entries.length === 0) return { added: 0, held: 0, skipped: 0 } satisfies Frozen
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
    const clashes: Array<Clash> = []
    let held = 0
    for (const entry of entries) {
      const keyOfId = byId.get(entry.caseId)
      const idOfKey = byKey.get(entry.callKey)
      if (keyOfId === entry.callKey) {
        held += 1
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
    if (fresh.length > 0) {
      yield* sql`
        INSERT INTO case_ids (capture, case_id, call_key, origin)
        SELECT ${capture}, e."caseId", e."callKey", ${origin}
        FROM jsonb_to_recordset(${JSON.stringify(fresh)}::jsonb) AS e("caseId" text, "callKey" text)
      `
    }
    return { added: fresh.length, held, skipped: clashes.length } satisfies Frozen
  })

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const of = Effect.fn("CaseIds.of")(function*(captures: ReadonlyArray<string>) {
      const out = new Map<string, Map<string, string>>()
      if (captures.length === 0) return out as ReadonlyMap<string, ReadonlyMap<string, string>>
      const rows = yield* sql<Row & { readonly capture: string }>`
        SELECT capture, case_id, call_key FROM case_ids WHERE capture IN ${sql.in(captures)}
      `
      for (const row of rows) {
        const byKey = out.get(row.capture) ?? new Map<string, string>()
        byKey.set(row.call_key, row.case_id)
        out.set(row.capture, byKey)
      }
      return out as ReadonlyMap<string, ReadonlyMap<string, string>>
    })

    const freeze = (capture: string, entries: ReadonlyArray<Entry>, origin: string, mode: Mode = "strict") =>
      sql.withTransaction(freezeOn(sql, capture, entries, origin, mode)).pipe(
        Effect.withSpan("CaseIds.freeze")
      )

    const count = Effect.gen(function*() {
      const rows = yield* sql<{ ids: number; captures: number }>`
        SELECT count(*)::int AS ids, count(DISTINCT capture)::int AS captures FROM case_ids
      `
      return { ids: Number(rows[0]!.ids), captures: Number(rows[0]!.captures) }
    })

    return Service.of({ of, freeze, count })
  })
)
