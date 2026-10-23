/**
 * The PER-RULE record: what each named rule of the deployment's roster came to
 * on one run, and the aggregate a global rules view reads back.
 *
 * It is written per run rather than re-read from the tree because a bundle is
 * retained only while it is a (capture, lane)'s latest or last-green one. A
 * reading taken off disk answers for the survivors alone; a row answers forever.
 *
 * Nothing here knows what a rule MEANS or which rules exist — the roster is
 * the deployment's code, the counts are rows, and the view joins the two by id.
 * So a rule the roster has and the corpus never reached has no row at all, and
 * that is exactly the zero the join must render loudly rather than skip.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as Sessions from "./sessions.js"

/** One rule's count on one run, under the one subdivision it was counted in. */
export interface Hit {
  readonly family: string
  readonly rule: string
  /**
   * The count's own subdivision, open per family: a confrontation record class
   * (`accepted` / `known-bug` / `unlisted` / `unknown`), `shape` for records of
   * that kind whatever their class, `blessed` for cells a bless restated citing
   * the rule, `fired` for the bless premise itself.
   */
  readonly bucket: string
  /** Records, refusals or cells the rule named — whatever the bucket counts. */
  readonly hits: number
  /** Cells the bucket landed on: `hits` says how loud, this says how wide. */
  readonly cells: number
  /** One cell of it by name, so a count is reproduced without the bundle. */
  readonly sample: string
}

/** One rule's count over a whole population, and a handful of its cells. */
export interface Total {
  readonly family: string
  readonly rule: string
  readonly bucket: string
  readonly hits: number
  readonly cells: number
  /** Up to five cells by name. */
  readonly samples: ReadonlyArray<string>
}

/**
 * How much of the population the totals actually speak for. A rule reading zero
 * over a population only 2% of which was measured is not a finding, and the two
 * must never be told apart by eye.
 */
export interface Coverage {
  /** Runs in the population the totals were taken over. */
  readonly runs: number
  /** Those of them whose per-rule counts were recorded. */
  readonly measured: number
}

/** A run whose bundle may still be on disk, and which lane it answers for. */
export interface Unmeasured {
  readonly id: number
  readonly lane: string
  readonly bundlePath: string
}

export interface Interface {
  /** Records one run's counts and marks the run measured, in one transaction. */
  readonly record: (runId: number, hits: ReadonlyArray<Hit>) => Effect.Effect<void, SqlError>
  /**
   * Runs whose per-rule counts were never taken, newest first. Whether the
   * bundle survives is the caller's question — the table does not watch disk.
   */
  readonly unmeasured: (limit: number) => Effect.Effect<ReadonlyArray<Unmeasured>, SqlError>
  /**
   * The totals over the latest `@current` run of each (capture, lane) — the
   * corpus as it stands, never a sum over every re-run a capture has ever had.
   */
  readonly standingTotals: (lane?: string) => Effect.Effect<ReadonlyArray<Total>, SqlError>
  /** How many standing runs the totals were taken over, and how many were measured. */
  readonly standingCoverage: (lane?: string) => Effect.Effect<Coverage, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/RuleHits") {}

interface TotalRow {
  readonly family: string
  readonly rule: string
  readonly bucket: string
  readonly hits: string
  readonly cells: string
  readonly samples: ReadonlyArray<string>
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    const record = Effect.fn("RuleHits.record")(function*(
      runId: number,
      hits: ReadonlyArray<Hit>
    ) {
      yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`DELETE FROM run_rule_hits WHERE run_id = ${runId}`
        for (const hit of hits) {
          yield* sql`
            INSERT INTO run_rule_hits (run_id, family, rule, bucket, hits, cells, sample)
            VALUES (${runId}, ${hit.family}, ${hit.rule}, ${hit.bucket},
                    ${hit.hits}, ${hit.cells}, ${hit.sample})
            ON CONFLICT (run_id, family, rule, bucket) DO UPDATE SET
              hits = EXCLUDED.hits, cells = EXCLUDED.cells, sample = EXCLUDED.sample
          `
        }
        // Marked measured whatever the row count: a run whose cells differed
        // from their capture in nothing has no rows and is not unmeasured.
        yield* sql`UPDATE runs SET rule_hits = true WHERE id = ${runId}`
      }))
    })

    const unmeasured = Effect.fn("RuleHits.unmeasured")(function*(limit: number) {
      const rows = yield* sql<{ id: string; lane: string; bundle_path: string }>`
        SELECT id, lane, bundle_path FROM runs
        WHERE NOT rule_hits AND bundle_path <> ''
        ORDER BY id DESC
        LIMIT ${limit}
      `
      return rows.map((row) => ({
        id: Number(row.id),
        lane: row.lane,
        bundlePath: Sessions.fromStored(root, row.bundle_path)
      } satisfies Unmeasured))
    })

    const standingTotals = Effect.fn("RuleHits.standingTotals")(function*(lane?: string) {
      const rows = yield* sql<TotalRow>`
        WITH standing AS (
          SELECT DISTINCT ON (capture, lane) id
          FROM current_runs
          WHERE ${lane === undefined ? sql`true` : sql`lane = ${lane}`}
          ORDER BY capture, lane, id DESC
        )
        SELECT h.family, h.rule, h.bucket,
               sum(h.hits)::bigint  AS hits,
               sum(h.cells)::bigint AS cells,
               (array_remove(array_agg(DISTINCT h.sample), ''))[1:5] AS samples
        FROM run_rule_hits h JOIN standing s ON s.id = h.run_id
        GROUP BY h.family, h.rule, h.bucket
      `
      return rows.map((row) => ({
        family: row.family,
        rule: row.rule,
        bucket: row.bucket,
        hits: Number(row.hits),
        cells: Number(row.cells),
        samples: row.samples
      } satisfies Total))
    })

    const standingCoverage = Effect.fn("RuleHits.standingCoverage")(function*(lane?: string) {
      const rows = yield* sql<{ runs: string; measured: string }>`
        WITH standing AS (
          SELECT DISTINCT ON (capture, lane) id, rule_hits
          FROM current_runs
          WHERE ${lane === undefined ? sql`true` : sql`lane = ${lane}`}
          ORDER BY capture, lane, id DESC
        )
        SELECT count(*)::bigint AS runs,
               count(*) FILTER (WHERE rule_hits)::bigint AS measured
        FROM standing
      `
      const row = rows[0]
      return {
        runs: row === undefined ? 0 : Number(row.runs),
        measured: row === undefined ? 0 : Number(row.measured)
      } satisfies Coverage
    })

    return Service.of({ record, unmeasured, standingTotals, standingCoverage })
  })
)
