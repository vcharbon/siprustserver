/**
 * The SOURCE LIST: every capture the population holds, each with
 * its ingest, its standing cut and its standing on every lane, served paged
 * and counted in ONE statement — the population joined to the newest
 * `capture_cuts` reading of each capture, that reading's case and refusal
 * counts and longest call, and the latest full `@current` run per lane.
 *
 * Staleness is a comparison of columns: the extractor hash the row was
 * ingested under against the one the caller's tree runs (`ingestState`, the
 * ONE ingest judgement — the campaign's `needs ingest` reads the same row
 * through it), the cut fingerprint the standing reading carries against the
 * caller's, and the ingest instant against the cut's. What the store cannot
 * see — a pcap whose bytes moved on disk — is the ingest's own question
 * (`ingest.ts staleness`), asked after this one when the source is added
 * again. A capture whose ingest failed is a row with its failure, never an
 * absence.
 *
 * Nothing here holds judgment: a row says what the ingest, the cut and the
 * runs recorded.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Fragment } from "effect/unstable/sql/Statement"
import type { RunStatus } from "./model.js"

/** How a capture's ingest stands against the caller's extractor. */
export type IngestState = "current" | "stale" | "failed"

/** What a population row records of its ingest: the three facts `ingestState` judges. */
export interface IngestFacts {
  readonly ingestedAt: string | Date | null
  readonly extractorHash: string | null
  readonly ingestFailed: string | null
}

/**
 * The ingest judgement, off the population row alone: `stale` where the row
 * was never ingested or names another extractor than the caller's — a
 * failure recorded under an older extractor is stale, since this one may take
 * the capture; `failed` where the caller's own extractor refused it; else
 * `current`. Every reader of a source's ingest state applies this and nothing
 * else to the row.
 */
export const ingestState = (row: IngestFacts, extractorHash: string): IngestState =>
  row.ingestedAt === null || row.extractorHash !== extractorHash
    ? "stale"
    : row.ingestFailed !== null
    ? "failed"
    : "current"

/** How a capture's standing cut stands against the caller's cut code and its ingest. */
export type CutState = "current" | "stale" | "broke" | "never"

/** A (capture, lane)'s standing as the list carries it: the latest full `@current` run. */
export interface LaneStanding {
  readonly lane: string
  readonly runId: number
  readonly status: RunStatus
  readonly cases: number
  readonly failed: number
  readonly skipped: number
  readonly finishedAt: string | null
}

export interface Row {
  readonly capture: string
  readonly sourcePath: string | null
  readonly addedAt: string
  readonly ingestedAt: string | null
  readonly extractorHash: string | null
  readonly ingest: IngestState
  /** The failure the ingest recorded; null unless `ingest` is `failed`. */
  readonly ingestFailed: string | null
  /** The newest prep that cut the capture; null where none did. */
  readonly prepId: number | null
  readonly cutAt: string | null
  readonly cut: CutState
  /** Why the standing cut broke; null unless `cut` is `broke`. */
  readonly cutBroke: string | null
  readonly cases: number
  readonly refusals: number
  /** The longest call of the standing cut, in seconds; null where no case states a span. */
  readonly spanSeconds: number | null
  readonly standings: ReadonlyArray<LaneStanding>
}

export interface Query {
  /** Only captures that need ingest or need cut. */
  readonly stale?: boolean
  /** A capture-name substring, matched case-insensitively. */
  readonly q?: string
  readonly limit: number
  readonly offset: number
  /** The extractor CODE the caller's tree runs: a row ingested under another is stale. */
  readonly extractorHash: string
  /** The cut CODE the caller's tree runs: a reading taken under another is stale. */
  readonly cutFingerprint: string
}

export interface Page {
  readonly rows: ReadonlyArray<Row>
  /** Every row the filter matches, the page's own included. */
  readonly total: number
}

export interface Interface {
  /** The page and its count, one statement. */
  readonly list: (query: Query) => Effect.Effect<Page, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Sources") {}

interface SourceRow {
  readonly capture: string
  readonly source_path: string | null
  readonly added_at: Date
  readonly ingested_at: Date | null
  readonly extractor_hash: string | null
  readonly ingest_failed: string | null
  readonly prep_id: string | null
  readonly cut_at: Date | null
  readonly cut_fingerprint: string | null
  readonly broke: string | null
  readonly cases: number
  readonly refusals: number
  readonly span_ms: string | null
  readonly standings: ReadonlyArray<{
    readonly lane: string
    readonly run_id: number
    readonly status: RunStatus
    readonly cases: number | null
    readonly failed: number | null
    readonly skipped: number | null
    readonly finished_at: string | null
  }> | null
  readonly total: string
}

const instantOrNull = (value: Date | null): string | null => (value === null ? null : value.toISOString())

const cutOf = (row: SourceRow, cutFingerprint: string): CutState => {
  if (row.prep_id === null || row.cut_at === null) return "never"
  if (row.broke !== null) return "broke"
  if (row.cut_fingerprint !== cutFingerprint) return "stale"
  if (row.ingested_at !== null && row.ingested_at > row.cut_at) return "stale"
  return "current"
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const list = Effect.fn("Sources.list")(function*(query: Query) {
      const like = query.q === undefined || query.q === "" ? sql`true` : sql`p.capture ILIKE ${`%${query.q}%`}`
      // The stale predicate is the same rule `ingestState` and `cutOf` read a
      // row by, stated once more in SQL so the page and its count agree.
      const stale = query.stale !== true ? sql`true` : sql`(
        p.ingested_at IS NULL OR p.extractor_hash IS DISTINCT FROM ${query.extractorHash}
        OR s.prep_id IS NULL
        OR s.broke IS NOT NULL
        OR s.cut_fingerprint IS DISTINCT FROM ${query.cutFingerprint}
        OR (p.ingested_at IS NOT NULL AND p.ingested_at > s.cut_at)
      )`
      const rows = yield* sql<SourceRow>`
        WITH standing_cut AS (
          SELECT DISTINCT ON (capture) capture, prep_id, cut_at, cut_fingerprint, broke
          FROM capture_cuts
          ORDER BY capture, prep_id DESC
        ),
        matched AS (
          SELECT p.capture, p.source_path, p.added_at, p.ingested_at, p.extractor_hash, p.ingest_failed,
                 s.prep_id, s.cut_at, s.cut_fingerprint, s.broke,
                 count(*) OVER () AS total
          FROM population p
          LEFT JOIN standing_cut s ON s.capture = p.capture
          WHERE ${like} AND ${stale}
          ORDER BY p.capture
          LIMIT ${query.limit} OFFSET ${query.offset}
        ),
        counted AS (
          SELECT m.capture,
                 (SELECT count(*)::int FROM cut_cases c WHERE c.prep_id = m.prep_id AND c.capture = m.capture) AS cases,
                 (SELECT count(*)::int FROM cut_refusals r WHERE r.prep_id = m.prep_id AND r.capture = m.capture) AS refusals,
                 (SELECT max((d.document->'timing'->>'capture_span_ms')::numeric)::text
                  FROM cut_cases c
                  JOIN cases cs ON cs.hash = c.case_hash
                  JOIN case_documents d ON d.hash = cs.document_hash
                  WHERE c.prep_id = m.prep_id AND c.capture = m.capture) AS span_ms
          FROM matched m
        ),
        lanes AS (
          SELECT r.capture, jsonb_agg(jsonb_build_object(
                   'lane', r.lane, 'run_id', r.id, 'status', r.status,
                   'cases', r.cases, 'failed', r.failed, 'skipped', r.skipped, 'finished_at', r.finished_at
                 ) ORDER BY r.lane) AS standings
          FROM (
            SELECT DISTINCT ON (capture, lane) *
            FROM current_runs
            WHERE set_kind = 'full' AND status <> 'interrupted'
              AND capture IN (SELECT capture FROM matched)
            ORDER BY capture, lane, id DESC
          ) r
          GROUP BY r.capture
        )
        SELECT m.*, c.cases, c.refusals, c.span_ms, l.standings
        FROM matched m
        JOIN counted c ON c.capture = m.capture
        LEFT JOIN lanes l ON l.capture = m.capture
        ORDER BY m.capture
      `
      const page = rows.map((row): Row => ({
        capture: row.capture,
        sourcePath: row.source_path,
        addedAt: row.added_at.toISOString(),
        ingestedAt: instantOrNull(row.ingested_at),
        extractorHash: row.extractor_hash,
        ingest: ingestState(
          { ingestedAt: row.ingested_at, extractorHash: row.extractor_hash, ingestFailed: row.ingest_failed },
          query.extractorHash
        ),
        ingestFailed: row.ingest_failed,
        prepId: row.prep_id === null ? null : Number(row.prep_id),
        cutAt: instantOrNull(row.cut_at),
        cut: cutOf(row, query.cutFingerprint),
        cutBroke: row.broke,
        cases: Number(row.cases),
        refusals: Number(row.refusals),
        spanSeconds: row.span_ms === null ? null : Number(row.span_ms) / 1000,
        standings: (row.standings ?? []).map((one) => ({
          lane: one.lane,
          runId: Number(one.run_id),
          status: one.status,
          cases: one.cases ?? 0,
          failed: one.failed ?? 0,
          skipped: one.skipped ?? 0,
          finishedAt: one.finished_at === null ? null : new Date(one.finished_at).toISOString()
        }))
      }))
      // The count rides every row of the page; a page past the end carries none.
      const total = rows.length > 0 ? Number(rows[0]!.total) : query.offset === 0 ? 0 : yield* count(like, stale)
      return { rows: page, total } satisfies Page
    })

    /** The filter's count alone, for a page the filter left empty. */
    const count = (like: Fragment, stale: Fragment) =>
      Effect.map(
        sql<{ total: string }>`
          WITH standing_cut AS (
            SELECT DISTINCT ON (capture) capture, prep_id, cut_at, cut_fingerprint, broke
            FROM capture_cuts ORDER BY capture, prep_id DESC
          )
          SELECT count(*) AS total FROM population p LEFT JOIN standing_cut s ON s.capture = p.capture
          WHERE ${like} AND ${stale}
        `,
        (rows) => Number(rows[0]?.total ?? 0)
      )

    return Service.of({ list })
  })
)
