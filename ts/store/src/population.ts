/**
 * The replay POPULATION: which captures this corpus stands behind.
 *
 * One row per capture, keyed by the pcap basename every other record is keyed
 * by. A capture joins when an operator adds its pcap and leaves only when a
 * human deletes the row — nothing derives membership, which is what makes a
 * no-regression sweep comparable with the one before it.
 *
 * Like the cut and the run records, this holds no judgment: it says which
 * captures the corpus carries and where their pcaps came from, never what any
 * of them proved.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { PopulationMember } from "./model.js"

/** A capture offered to the population, as its adder knows it. */
export interface Entry {
  readonly capture: string
  /** The pcap it was read from on this host; null where the adder has no path. */
  readonly sourcePath: string | null
  /** Whether the artifact store extracted it just now. */
  readonly ingested?: boolean
  /**
   * The instant its standing ingest record names, for a capture found current:
   * fills a row that has no ingest instant of its own, so a store rebuilt over
   * kept artifacts reads them current; never moves an instant the row holds.
   */
  readonly ingestedAt?: string
  /**
   * The extractor CODE behind its standing artifacts: the one that took them
   * just now, or, for a capture found current, the one its ingest record
   * names — restated so the row says what the artifact store says.
   */
  readonly extractorHash?: string
  /** Why its standing extraction refused it; null is an extraction that succeeded. Read with `extractorHash`. */
  readonly ingestFailed?: string | null
}

export interface Interface {
  /**
   * The captures offered, added where they are new and re-pointed at their
   * current pcap where they are not. Answers how many rows were NEW, which is
   * what an operator is told; a capture already held is not an error.
   */
  readonly add: (entries: ReadonlyArray<Entry>) => Effect.Effect<number, SqlError>
  /** Every member, in capture order. */
  readonly list: Effect.Effect<ReadonlyArray<PopulationMember>, SqlError>
  /** Every member's name alone, which is what a work list is built from. */
  readonly captures: Effect.Effect<ReadonlyArray<string>, SqlError>
  readonly size: Effect.Effect<number, SqlError>
  /**
   * The captures a legacy corpus named, adopted as the population — ONCE. A
   * table that already holds a row is left exactly as it is and the answer is
   * 0, so the seed is a migration an operator can re-run without wondering.
   */
  readonly seed: (captures: ReadonlyArray<string>) => Effect.Effect<number, SqlError>
  /** Records that the artifact store extracted this capture just now. */
  readonly ingested: (capture: string) => Effect.Effect<void, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Population") {}

interface Row {
  readonly capture: string
  readonly source_path: string | null
  readonly added_at: Date
  readonly ingested_at: Date | null
  readonly extractor_hash: string | null
  readonly ingest_failed: string | null
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const add = Effect.fn("Population.add")(function*(entries: ReadonlyArray<Entry>) {
      let added = 0
      for (const entry of entries) {
        // `xmax = 0` is postgres' own answer to which branch the upsert took:
        // an inserted row has no update transaction on it, an updated one does.
        // The two extractor facts move together, and only when the adder
        // states them: an extraction taken now, or the standing record read
        // back. A row merely re-pointed at its pcap keeps what it had; the
        // instant moves on an extraction and is otherwise only filled in.
        const stated = entry.extractorHash !== undefined
        const rows = yield* sql<{ inserted: boolean }>`
          INSERT INTO population (capture, source_path, ingested_at, extractor_hash, ingest_failed)
          VALUES (${entry.capture}, ${entry.sourcePath},
                  ${entry.ingested === true ? sql`now()` : entry.ingestedAt ?? null},
                  ${entry.extractorHash ?? null},
                  ${stated ? entry.ingestFailed ?? null : null})
          ON CONFLICT (capture) DO UPDATE SET
            source_path = coalesce(EXCLUDED.source_path, population.source_path),
            ingested_at = CASE WHEN ${entry.ingested === true} THEN EXCLUDED.ingested_at
                               ELSE coalesce(population.ingested_at, EXCLUDED.ingested_at) END,
            extractor_hash = CASE WHEN ${stated} THEN EXCLUDED.extractor_hash ELSE population.extractor_hash END,
            ingest_failed = CASE WHEN ${stated} THEN EXCLUDED.ingest_failed ELSE population.ingest_failed END
          RETURNING (xmax = 0) AS inserted
        `
        if (rows[0]?.inserted === true) added += 1
      }
      return added
    })

    const list = Effect.gen(function*() {
      const rows = yield* sql<Row>`
        SELECT capture, source_path, added_at, ingested_at, extractor_hash, ingest_failed
        FROM population ORDER BY capture
      `
      return rows.map((row) => ({
        capture: row.capture,
        sourcePath: row.source_path,
        addedAt: row.added_at.toISOString(),
        ingestedAt: row.ingested_at === null ? null : row.ingested_at.toISOString(),
        extractorHash: row.extractor_hash,
        ingestFailed: row.ingest_failed
      } satisfies PopulationMember))
    })

    const captures = Effect.gen(function*() {
      const rows = yield* sql<{ capture: string }>`SELECT capture FROM population ORDER BY capture`
      return rows.map((row) => row.capture)
    })

    const size = Effect.gen(function*() {
      const rows = yield* sql<{ size: number }>`SELECT count(*)::int AS size FROM population`
      return rows[0]?.size ?? 0
    })

    // The table is held for the read AND the inserts: two commands racing to
    // seed an empty table would otherwise both read it empty and both fill it.
    const seed = Effect.fn("Population.seed")(function*(named: ReadonlyArray<string>) {
      return yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`LOCK TABLE population IN SHARE ROW EXCLUSIVE MODE`
        const held = yield* size
        if (held > 0 || named.length === 0) return 0
        for (const capture of named) {
          yield* sql`
            INSERT INTO population (capture, source_path) VALUES (${capture}, NULL)
            ON CONFLICT (capture) DO NOTHING
          `
        }
        return named.length
      }))
    })

    const ingested = Effect.fn("Population.ingested")(function*(capture: string) {
      yield* sql`UPDATE population SET ingested_at = now() WHERE capture = ${capture}`
    })

    return Service.of({ add, list, captures, size, seed, ingested })
  })
)
