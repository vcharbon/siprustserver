/**
 * The CAMPAIGN RECORD (migration `19_campaign_runs`) over the results model
 * (`campaign-results.ts`): a campaign run is a `campaign_runs` row tying each
 * lane's store `runs` row; each cell that ran has a `campaign_cells` row
 * beside its case result (its bundle, its backend's log, its attempts).
 *
 * What the pack states of the run beside the base's columns is its TARGET, a
 * JSON object written at open and merged into later ({@link Interface.target});
 * what each case it compiled says of itself is a JSON value per case `ref`
 * ({@link Interface.describe}). The store keeps both as the pack wrote them and
 * hands them back unread: the pack that wrote them types them.
 */
import type { Verification } from "@sip/contracts"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as path from "node:path"
import { type Attempt, type CampaignRun, checksOf, type Lane, type Log, type Result, type RunSelection } from "./campaign-results.js"
import type { Provenance } from "./model.js"
import * as Sessions from "./sessions.js"
import * as Verifications from "./verifications.js"

/** What the pack states of the target a run drove: a JSON object, its keys the pack's. */
export type Target = Readonly<Record<string, unknown>>

/** What a campaign run row states when opened. */
export interface OpenRequest {
  readonly capture: string
  readonly env: string | null
  readonly branch: string | null
  readonly seed: string | null
  readonly dir: string
  readonly scope: string | null
  readonly selection: RunSelection
  readonly target: Target
}

/** A case result's cell as the store keeps it beside the result: its bundle directory, its log, its attempts. */
export interface CellRecord {
  readonly cell: string
  readonly log: Log | null
  /** Every attempt where the cell was replayed more than once; absent for one attempt. */
  readonly attempts?: ReadonlyArray<Attempt>
}

/** A campaign run as stored: the run, its target and what each case it compiled says of itself, by `ref`. */
export interface StoredRun extends CampaignRun {
  readonly target: Target
  readonly about: Readonly<Record<string, unknown>>
}

/** A case result's kept cell: its directory under the lane run's bundle, the case result it is (`runs.id`, its case id), and its run row's lane. */
export interface KeptCell {
  readonly cell: string
  readonly runId: number
  readonly caseId: string
  readonly lane: string
}

/** A read of a campaign's latest `limit` runs, or of the runs `ids` names. */
export type ReadOptions = LatestOptions | { readonly ids: ReadonlyArray<number> }

export interface LatestOptions {
  readonly limit: number
  readonly ids?: undefined
  readonly env?: string
  readonly include?: number
  /** Only the partial runs (`true`) or only the full ones (`false`); absent, both. */
  readonly partial?: boolean
}

export interface Interface {
  readonly open: (request: OpenRequest) => Effect.Effect<number, SqlError>
  /** Ties a lane's store run row to the campaign run. */
  readonly link: (campaignRunId: number, runId: number) => Effect.Effect<void, SqlError>
  /** Merges `target`'s keys into the run's target, each replacing the one written before. */
  readonly target: (campaignRunId: number, target: Target) => Effect.Effect<void, SqlError>
  /** What each case the run compiled says about itself, by `ref`, replacing any written before. */
  readonly describe: (campaignRunId: number, about: Readonly<Record<string, unknown>>) => Effect.Effect<void, SqlError>
  /** A case result's cell, for a result its run row already holds, replacing any written before. */
  readonly writeCell: (runId: number, caseId: string, cell: CellRecord) => Effect.Effect<void, SqlError>
  readonly finish: (campaignRunId: number) => Effect.Effect<void, SqlError>
  /**
   * The campaign's latest `limit` runs, newest first (of environment `env`
   * where one is named, full or partial only where `partial` says), the run
   * `include` names among them whatever its age; or the runs `ids` names.
   */
  readonly read: (capture: string, options: ReadOptions) => Effect.Effect<ReadonlyArray<StoredRun>, SqlError>
  /** Every campaign the store holds a run of. */
  readonly captures: Effect.Effect<ReadonlyArray<string>, SqlError>
  /** The environments that wrote the campaign's runs, the latest writer first. */
  readonly envs: (capture: string) => Effect.Effect<ReadonlyArray<string>, SqlError>
  /**
   * Every case result of the lane run whose bundle is `bundlePath` (none
   * outside the session tree): its kept attempt's cell directory, by name.
   */
  readonly keptCells: (bundlePath: string) => Effect.Effect<ReadonlyArray<KeptCell>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/CampaignRuns") {}

interface RunRow {
  readonly id: string
  readonly capture: string
  readonly env: string | null
  readonly branch: string | null
  readonly seed: string | null
  readonly dir: string
  readonly scope: string | null
  readonly selection: RunSelection | null
  readonly target: Target
  readonly started_at: Date
  readonly finished_at: Date | null
}

interface AboutRow {
  readonly campaign_run_id: string
  readonly ref: string
  readonly about: unknown
}

interface LaneRow {
  readonly campaign_run_id: string
  readonly run_id: string
  readonly lane: string
  readonly status: string
  readonly detail: string | null
  readonly bundle_path: string
  readonly components: Provenance
}

interface ResultRow {
  readonly run_id: string
  readonly case_id: string
  readonly cell: string | null
  readonly log_records: number | null
  readonly log_missing: string | null
  readonly attempts: ReadonlyArray<Attempt> | null
  readonly passed: boolean
  readonly skipped: string | null
  readonly verdict: string
  readonly error_kind: string | null
  readonly error_rule: string | null
  readonly error_detail: string | null
}

type LineRow = Verifications.Row & { readonly run_id: string; readonly case_id: string }

/** The log a cell row states: its records counted, or why it is missing; null where neither is stated. */
const logOf = (row: Pick<ResultRow, "log_records" | "log_missing">): Log | null =>
  row.log_records !== null
    ? { records: row.log_records }
    : row.log_missing !== null
    ? { missing: row.log_missing }
    : null

/** A case result row with its lines, as the model reads it. */
const resultOf = (row: ResultRow, lines: ReadonlyArray<Verification.Verification>): Result => ({
  caseId: row.case_id,
  cell: row.cell,
  log: logOf(row),
  passed: row.passed,
  skipped: row.skipped,
  verdict: row.verdict,
  errorKind: row.error_kind,
  errorRule: row.error_rule,
  errorDetail: row.error_detail,
  checks: checksOf(lines),
  verifications: lines,
  ...(row.attempts === null ? {} : { attempts: row.attempts })
})

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    const open = Effect.fn("CampaignRuns.open")(function*(request: OpenRequest) {
      const rows = yield* sql<{ id: string }>`
        INSERT INTO campaign_runs (capture, env, branch, seed, dir, scope, selection, target)
        VALUES (${request.capture}, ${request.env}, ${request.branch}, ${request.seed}, ${request.dir},
                ${request.scope}, ${JSON.stringify(request.selection)}::jsonb, ${JSON.stringify(request.target)}::jsonb)
        RETURNING id
      `
      return Number(rows[0]!.id)
    })

    const link = Effect.fn("CampaignRuns.link")(function*(campaignRunId: number, runId: number) {
      yield* sql`
        INSERT INTO campaign_run_lanes (campaign_run_id, run_id) VALUES (${campaignRunId}, ${runId})
        ON CONFLICT DO NOTHING
      `
    })

    const target = Effect.fn("CampaignRuns.target")(function*(campaignRunId: number, stated: Target) {
      yield* sql`
        UPDATE campaign_runs SET target = target || ${JSON.stringify(stated)}::jsonb WHERE id = ${campaignRunId}
      `
    })

    const describe = Effect.fn("CampaignRuns.describe")(
      function*(campaignRunId: number, about: Readonly<Record<string, unknown>>) {
        yield* sql.withTransaction(Effect.gen(function*() {
          yield* sql`DELETE FROM campaign_case_about WHERE campaign_run_id = ${campaignRunId}`
          for (const [ref, one] of Object.entries(about)) {
            yield* sql`
              INSERT INTO campaign_case_about (campaign_run_id, ref, about)
              VALUES (${campaignRunId}, ${ref}, ${JSON.stringify(one)}::jsonb)
            `
          }
        }))
      }
    )

    const writeCell = Effect.fn("CampaignRuns.writeCell")(function*(runId: number, caseId: string, cell: CellRecord) {
      const log = cell.log
      yield* sql`
        INSERT INTO campaign_cells (run_id, case_id, cell, log_records, log_missing, attempts)
        VALUES (${runId}, ${caseId}, ${cell.cell},
                ${log !== null && "records" in log ? log.records : null},
                ${log !== null && "missing" in log ? log.missing : null},
                ${cell.attempts === undefined ? null : JSON.stringify(cell.attempts)}::jsonb)
        ON CONFLICT (run_id, case_id) DO UPDATE SET
          cell = EXCLUDED.cell, log_records = EXCLUDED.log_records, log_missing = EXCLUDED.log_missing,
          attempts = EXCLUDED.attempts
      `
    })

    const finish = Effect.fn("CampaignRuns.finish")(function*(campaignRunId: number) {
      yield* sql`UPDATE campaign_runs SET finished_at = now() WHERE id = ${campaignRunId}`
    })

    /** The rows of the campaign's latest `options.limit` runs, and of the one `options.include` names. */
    const latestRows = (capture: string, options: LatestOptions) => {
      const env = options.env === undefined ? sql`true` : sql`env = ${options.env}`
      const partial = options.partial === undefined
        ? sql`true`
        : options.partial
        ? sql`scope IS NOT NULL`
        : sql`scope IS NULL`
      return sql<RunRow>`
        SELECT * FROM campaign_runs
        WHERE capture = ${capture} AND (
          id IN (
            SELECT id FROM campaign_runs WHERE capture = ${capture} AND ${env} AND ${partial}
            ORDER BY id DESC LIMIT ${options.limit}
          )
          OR id = ${options.include ?? null}
        )
        ORDER BY id DESC
      `
    }

    /** The lines of every cell of the lane runs `runIds`, by cell, in their order. */
    const linesOf = (runIds: ReadonlyArray<string>) =>
      Effect.map(
        sql<LineRow>`
          SELECT run_id, case_id, batch_id, job_id, name, source, answer, message, detail, informative, waived
          FROM verifications WHERE scope = 'cell' AND run_id = ANY(${runIds}::bigint[])
          ORDER BY run_id, case_id, ordinal
        `,
        (rows) => {
          const byCell = new Map<string, Array<Verification.Verification>>()
          for (const row of rows) {
            const key = Verifications.cellKey(Number(row.run_id), row.case_id)
            const list = byCell.get(key) ?? []
            list.push(Verifications.lineOf(row))
            byCell.set(key, list)
          }
          return byCell
        }
      )

    const read = Effect.fn("CampaignRuns.read")(function*(capture: string, options: ReadOptions) {
      const runs = options.ids === undefined
        ? yield* latestRows(capture, options)
        : yield* sql<RunRow>`
            SELECT * FROM campaign_runs WHERE capture = ${capture} AND id = ANY(${options.ids.map(String)}::bigint[])
            ORDER BY id DESC
          `
      const ids = runs.map((run) => run.id)
      if (ids.length === 0) return []
      const abouts = yield* sql<AboutRow>`
        SELECT campaign_run_id, ref, about FROM campaign_case_about WHERE campaign_run_id = ANY(${ids}::bigint[])
      `
      const lanes = yield* sql<LaneRow>`
        SELECT l.campaign_run_id, r.id AS run_id, r.lane, r.status, r.detail, r.bundle_path, r.components
        FROM campaign_run_lanes l JOIN runs r ON r.id = l.run_id
        WHERE l.campaign_run_id = ANY(${ids}::bigint[])
        ORDER BY r.id
      `
      const runIds = lanes.map((lane) => lane.run_id)
      const results = runIds.length === 0 ? [] : yield* sql<ResultRow>`
        SELECT r.run_id, r.case_id, c.cell, c.log_records, c.log_missing, c.attempts,
               r.passed, r.skipped, r.verdict, r.error_kind, r.error_rule, r.error_detail
        FROM case_results r
        LEFT JOIN campaign_cells c ON c.run_id = r.run_id AND c.case_id = r.case_id
        WHERE r.run_id = ANY(${runIds}::bigint[])
        ORDER BY r.run_id, r.case_id
      `
      const lines = runIds.length === 0 ? new Map() : yield* linesOf(runIds)
      const resultsOf = new Map<string, Array<Result>>()
      for (const row of results) {
        const list = resultsOf.get(row.run_id) ?? []
        list.push(resultOf(row, lines.get(Verifications.cellKey(Number(row.run_id), row.case_id)) ?? []))
        resultsOf.set(row.run_id, list)
      }
      return runs.map((run): StoredRun => ({
        id: Number(run.id),
        capture: run.capture,
        env: run.env,
        branch: run.branch,
        seed: run.seed,
        dir: run.dir,
        scope: run.scope,
        selection: run.selection,
        target: run.target,
        about: Object.fromEntries(abouts.filter((row) => row.campaign_run_id === run.id).map((row) => [row.ref, row.about])),
        startedAt: run.started_at.toISOString(),
        finishedAt: run.finished_at === null ? null : run.finished_at.toISOString(),
        lanes: lanes.filter((lane) => lane.campaign_run_id === run.id).map((lane): Lane => ({
          runId: Number(lane.run_id),
          lane: lane.lane,
          status: lane.status,
          detail: lane.detail,
          bundlePath: Sessions.fromStored(root, lane.bundle_path),
          components: lane.components,
          results: resultsOf.get(lane.run_id) ?? []
        }))
      }))
    })

    const captures = Effect.map(
      sql<{ capture: string }>`SELECT DISTINCT capture FROM campaign_runs ORDER BY capture`,
      (rows) => rows.map((row) => row.capture)
    )

    const envs = Effect.fn("CampaignRuns.envs")(function*(capture: string) {
      const rows = yield* sql<{ env: string }>`
        SELECT env FROM campaign_runs WHERE capture = ${capture} AND env IS NOT NULL
        GROUP BY env ORDER BY max(id) DESC
      `
      return rows.map((row) => row.env)
    })

    const keptCells = Effect.fn("CampaignRuns.keptCells")(function*(bundlePath: string) {
      const stored = path.relative(root.dir, path.resolve(bundlePath))
      if (stored === "" || stored.startsWith("..") || path.isAbsolute(stored)) return []
      const rows = yield* sql<{ cell: string; run_id: string; case_id: string; lane: string }>`
        SELECT c.cell, c.run_id, c.case_id, r.lane FROM campaign_cells c JOIN runs r ON r.id = c.run_id
        WHERE r.bundle_path = ${stored} ORDER BY c.cell
      `
      return rows.map((row): KeptCell => ({ cell: row.cell, runId: Number(row.run_id), caseId: row.case_id, lane: row.lane }))
    })

    return Service.of({ open, link, target, describe, writeCell, finish, read, captures, envs, keptCells })
  })
)
