/**
 * The JOB: one launched command as a row, whoever launched it. The
 * CLI and the viewer create the same rows, so a viewer restart loses nothing
 * and both read one queue.
 *
 * A row carries two kinds of fact. The LAUNCH facts are written by whoever
 * started the process — the pid, the log, where the hook logs go — and the
 * PROGRESS facts by the running phase itself: its sweep, the batch it is on,
 * the lock holder it waits for. Its outcome is written by the phase on the way
 * out, or by the reaper when the process is gone and never wrote one.
 *
 * Progress is READ, never pushed: what the job's cells came to is
 * `job_cells`, what its runs proved is `case_results`, and one statement
 * over both answers the bar.
 *
 * A STOP is written on the row before the process is signalled — when
 * it was asked and how long its in-flight cells get — so the phase reads its
 * grace off the row and the page reads that a stop is under way. A RESTART
 * is a new row naming the one it runs the request of again.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import type { Fragment } from "effect/unstable/sql/Statement"
import type { MainError, MainErrorKind } from "./model.js"
import type { Outcome } from "./requests.js"

/** What a job does: a request replay, a cut, an ingest, a source retired, runs deleted. */
export type Kind = "replay" | "cut" | "ingest" | "retire" | "delete"

export const KINDS: ReadonlyArray<Kind> = ["replay", "cut", "ingest", "retire", "delete"]

/**
 * `queued` is the row before its process exists; `running` while the pid is
 * alive; the four ends: `done` and `failed` as the phase reported, `interrupted`
 * on a handled stop, `died` when the process was gone with no exit recorded.
 */
export type Status = "queued" | "running" | "done" | "failed" | "interrupted" | "died"

/** How a job that is no longer running ended. */
export type End = Exclude<Status, "queued" | "running">

export interface CreateRequest {
  readonly kind: Kind
  readonly requestId?: number
  /** The lane a replay drives; a cut or an ingest has none. */
  readonly lane?: string
  /** The knobs the command was given, as it was given them. */
  readonly args: Readonly<Record<string, unknown>>
  readonly createdBy: string
  /** The job this one restarts, where it is a restart. */
  readonly restartOf?: number
  /** The units the job sets out to work, where the launcher knows them: a replay's request cells. */
  readonly planned?: number
}

export interface StartedRequest {
  readonly pid: number
  /** The whole stdout+stderr of the process; absent for a job run in the foreground. */
  readonly logPath?: string
  /** Where the hook logs of this job land. */
  readonly hooksPath?: string
}

export interface CloseRequest {
  readonly status: End
  /** The process exit code, where one was read. */
  readonly exit: number | null
}

export interface Row {
  readonly id: number
  readonly kind: Kind
  readonly requestId: number | null
  /** Null for a job that drives no lane (a cut, an ingest). */
  readonly lane: string | null
  readonly args: Readonly<Record<string, unknown>>
  readonly status: Status
  readonly createdAt: string
  readonly createdBy: string
  readonly pid: number | null
  readonly startedAt: string | null
  readonly finishedAt: string | null
  readonly exit: number | null
  readonly logPath: string | null
  readonly hooksPath: string | null
  /** The lock holder's line while the job waits for it; null otherwise. */
  readonly waitingOn: string | null
  readonly sweepId: number | null
  /** The batch the phase is on, 1-based, and how many there are. */
  readonly batchAt: number | null
  readonly batches: number | null
  /** When a stop was asked for; null where none was. */
  readonly stopRequestedAt: string | null
  /** The seconds the stop gave the in-flight cells; null where none was asked. */
  readonly stopGrace: number | null
  /** The job this one restarts; null where it is not a restart. */
  readonly restartOf: number | null
  /**
   * How many units the job set out to work: captures for a cut, an ingest or a
   * retire; runs for a delete; a replay's cells until `job_cells` say.
   */
  readonly planned: number | null
}

/**
 * A job's units and results, counted: what the bar reads. A replay's
 * units are its cells; a cut's are the captures its preps cut, over the
 * captures it planned (`planned`); an ingest's are what it planned alone.
 */
export interface Progress {
  /** Every cell the job resolved, gone ones included; a cut's planned captures. */
  readonly total: number
  readonly landed: number
  readonly pending: number
  readonly goneRefused: number
  readonly goneMissing: number
  /** From `case_results` of the job's runs: written as each capture's row closes. */
  readonly passed: number
  readonly failed: number
  readonly skipped: number
  readonly runs: number
  readonly openRuns: number
}

export interface Detail {
  readonly job: Row
  readonly progress: Progress
}

/** A job as the list shows it: the row, its units landed over total, and what landed red or was gone. */
export interface Listed extends Row {
  readonly landed: number
  readonly total: number
  /** `case_results` failed under the job's runs. */
  readonly failed: number
  /** Cells the resolution found no case for, either way. */
  readonly gone: number
}

/** Which rows a page lists: one status, `active` (running and queued), `ended` (the four ends), or every row. */
export type StatusFilter = Status | "active" | "ended"

export interface PageQuery {
  readonly status?: StatusFilter | undefined
  readonly limit: number
  readonly offset: number
}

/** One page of the queue, counted: running rows first, then queued, then the ended newest first. */
export interface Page {
  readonly rows: ReadonlyArray<Listed>
  readonly total: number
}

/** A job in a restart chain: enough to link it and say how it ran the request. */
export interface Kin {
  readonly id: number
  readonly status: Status
  /** Whether it ran the request's unrun cells alone (`resume`) or every cell (`again`); null for a first run. */
  readonly mode: "resume" | "again" | null
}

/** The jobs a restart chain ties this one to: what it restarts, nearest first, and what restarted it. */
export interface Lineage {
  readonly ancestors: ReadonlyArray<Kin>
  readonly descendants: ReadonlyArray<Kin>
}

/** A cell the resolution found no case for, with the cut's reasons where it refused the call. */
export interface GoneCell {
  readonly capture: string
  readonly callKey: string
  readonly outcome: Outcome
  readonly reasons: ReadonlyArray<string>
}

/** A case that failed under one of the job's runs: enough for its study link and its main error. */
export interface RedCell {
  readonly capture: string
  readonly lane: string
  readonly caseId: string
  readonly runId: number
  readonly error: MainError | null
  /** The body rows no rule accepts, whatever `error` names. */
  readonly bodyFailing: number
}

/** A slice of a job's cells of one kind, with how many there are. */
export interface Cells<A> {
  readonly cells: ReadonlyArray<A>
  readonly total: number
}

export interface Interface {
  /** A `queued` row; the process comes after. */
  readonly create: (request: CreateRequest) => Effect.Effect<number, SqlError>
  /** The process exists: `running` (unless it has already ended), with what the launcher knows of it. */
  readonly started: (id: number, request: StartedRequest) => Effect.Effect<void, SqlError>
  /** The end, and the exit code where one was read. */
  readonly close: (id: number, request: CloseRequest) => Effect.Effect<void, SqlError>
  /** The lock holder the job waits for; null once it has the lock. */
  readonly waiting: (id: number, holder: string | null) => Effect.Effect<void, SqlError>
  readonly batch: (id: number, at: number, of: number) => Effect.Effect<void, SqlError>
  readonly sweep: (id: number, sweepId: number) => Effect.Effect<void, SqlError>
  /** A stop asked for, with the grace its in-flight cells get, written before the process is signalled. */
  readonly stop: (id: number, graceSeconds: number) => Effect.Effect<void, SqlError>
  /** How many units the job set out to work, once it knows. */
  readonly planned: (id: number, units: number) => Effect.Effect<void, SqlError>
  /**
   * How many of its planned units the job decided with nothing to land (an
   * ingest's captures that stand current or were refused): they count landed.
   */
  readonly settle: (id: number, units: number) => Effect.Effect<void, SqlError>
  /** The row and its progress, ONE statement; null where no job has the id. */
  readonly show: (id: number) => Effect.Effect<Detail | null, SqlError>
  /** The planner's account of that statement (`EXPLAIN ANALYZE`), one line per plan node: how E5 is checked. */
  readonly explain: (id: number) => Effect.Effect<ReadonlyArray<string>, SqlError>
  readonly list: (limit: number) => Effect.Effect<ReadonlyArray<Listed>, SqlError>
  /** One page of the queue under a status filter: running first, queued, then the ended newest first. */
  readonly page: (query: PageQuery) => Effect.Effect<Page, SqlError>
  /** The restart chain around a job. */
  readonly lineage: (id: number) => Effect.Effect<Lineage, SqlError>
  /** The job's gone cells with their reasons, in cell order, `limit` at most. */
  readonly gone: (id: number, limit: number) => Effect.Effect<Cells<GoneCell>, SqlError>
  /** The cases that failed under the job's runs so far, in run then case order, `limit` at most. */
  readonly red: (id: number, limit: number) => Effect.Effect<Cells<RedCell>, SqlError>
  /** Every `running` row, oldest first: what the liveness rule examines. */
  readonly running: Effect.Effect<ReadonlyArray<Row>, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Jobs") {}

interface JobRow {
  readonly id: string
  readonly kind: Kind
  readonly request_id: string | null
  readonly lane: string | null
  readonly args: Readonly<Record<string, unknown>>
  readonly status: Status
  readonly created_at: Date
  readonly created_by: string
  readonly pid: number | null
  readonly started_at: Date | null
  readonly finished_at: Date | null
  readonly exit: number | null
  readonly log_path: string | null
  readonly hooks_path: string | null
  readonly waiting_on: string | null
  readonly sweep_id: string | null
  readonly batch_at: number | null
  readonly batches: number | null
  readonly stop_requested_at: Date | null
  readonly stop_grace: number | null
  readonly restart_of: string | null
  readonly planned: number | null
}

interface ProgressRow {
  readonly total: number
  readonly landed: number
  readonly pending: number
  readonly gone_refused: number
  readonly gone_missing: number
  readonly passed: number
  readonly failed: number
  readonly skipped: number
  readonly runs: number
  readonly open_runs: number
}

const numberOrNull = (value: string | number | null): number | null => (value === null ? null : Number(value))

const instantOrNull = (value: Date | null): string | null => (value === null ? null : value.toISOString())

const rowOf = (row: JobRow): Row => ({
  id: Number(row.id),
  kind: row.kind,
  requestId: numberOrNull(row.request_id),
  lane: row.lane,
  args: row.args,
  status: row.status,
  createdAt: row.created_at.toISOString(),
  createdBy: row.created_by,
  pid: row.pid,
  startedAt: instantOrNull(row.started_at),
  finishedAt: instantOrNull(row.finished_at),
  exit: row.exit,
  logPath: row.log_path,
  hooksPath: row.hooks_path,
  waitingOn: row.waiting_on,
  sweepId: numberOrNull(row.sweep_id),
  batchAt: row.batch_at,
  batches: row.batches,
  stopRequestedAt: instantOrNull(row.stop_requested_at),
  stopGrace: row.stop_grace,
  restartOf: numberOrNull(row.restart_of),
  planned: row.planned
})

const progressOf = (row: ProgressRow): Progress => ({
  total: Number(row.total),
  landed: Number(row.landed),
  pending: Number(row.pending),
  goneRefused: Number(row.gone_refused),
  goneMissing: Number(row.gone_missing),
  passed: Number(row.passed),
  failed: Number(row.failed),
  skipped: Number(row.skipped),
  runs: Number(row.runs),
  openRuns: Number(row.open_runs)
})

const modeOf = (args: Readonly<Record<string, unknown>>, restartOf: string | null): Kin["mode"] =>
  restartOf === null ? null : args["unran"] === true ? "resume" : "again"

interface ErrorColumns {
  readonly error_kind: string | null
  readonly error_rule: string | null
  readonly error_rfc: string | null
  readonly error_step: string | null
  readonly error_expected: string | null
  readonly error_detail: string | null
}

const errorOf = (row: ErrorColumns): MainError | null =>
  row.error_kind === null
    ? null
    : {
      kind: row.error_kind as MainErrorKind,
      rule: row.error_rule,
      rfc: row.error_rfc,
      step: row.error_step,
      expected: row.error_expected,
      detail: row.error_detail ?? ""
    }

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient

    const create = Effect.fn("Jobs.create")(function*(request: CreateRequest) {
      const rows = yield* sql<{ id: string }>`
        INSERT INTO jobs (kind, request_id, lane, args, status, created_by, restart_of, planned)
        VALUES (${request.kind}, ${request.requestId ?? null}, ${request.lane ?? null},
                ${JSON.stringify(request.args)}::jsonb, 'queued', ${request.createdBy}, ${request.restartOf ?? null},
                ${request.planned ?? null})
        RETURNING id
      `
      return Number(rows[0]!.id)
    })

    // The status moves from `queued` alone: a process that ended before its
    // launcher wrote this line has closed the row already, and keeps its end.
    const started = Effect.fn("Jobs.started")(function*(id: number, request: StartedRequest) {
      yield* sql`
        UPDATE jobs SET status = CASE WHEN status = 'queued' THEN 'running' ELSE status END,
                        pid = ${request.pid}, started_at = now(),
                        log_path = ${request.logPath ?? null}, hooks_path = ${request.hooksPath ?? null}
        WHERE id = ${id}
      `
    })

    const close = Effect.fn("Jobs.close")(function*(id: number, request: CloseRequest) {
      yield* sql`
        UPDATE jobs SET status = ${request.status}, exit = ${request.exit}, finished_at = now(), waiting_on = NULL
        WHERE id = ${id}
      `
    })

    const waiting = Effect.fn("Jobs.waiting")(function*(id: number, holder: string | null) {
      yield* sql`UPDATE jobs SET waiting_on = ${holder} WHERE id = ${id}`
    })

    const batch = Effect.fn("Jobs.batch")(function*(id: number, at: number, of: number) {
      yield* sql`UPDATE jobs SET batch_at = ${at}, batches = ${of} WHERE id = ${id}`
    })

    const sweep = Effect.fn("Jobs.sweep")(function*(id: number, sweepId: number) {
      yield* sql`UPDATE jobs SET sweep_id = ${sweepId} WHERE id = ${id}`
    })

    const stop = Effect.fn("Jobs.stop")(function*(id: number, graceSeconds: number) {
      yield* sql`UPDATE jobs SET stop_requested_at = now(), stop_grace = ${graceSeconds} WHERE id = ${id}`
    })

    const planned = Effect.fn("Jobs.planned")(function*(id: number, units: number) {
      yield* sql`UPDATE jobs SET planned = ${units} WHERE id = ${id}`
    })

    const settle = Effect.fn("Jobs.settle")(function*(id: number, units: number) {
      yield* sql`UPDATE jobs SET settled = ${units} WHERE id = ${id}`
    })

    /**
     * One statement: the row, its cells by outcome off `job_cells (job_id,
     * outcome)`, its results as one aggregate per run under `LATERAL` — the
     * planner loops the job's runs off `runs (job_id)` into the `case_results`
     * primary key, never a join it may hash over the whole table — and — for a cut, or an ingest that cuts — the captures its
     * preps cut off `preps (job_id)`, counted against what it planned; an
     * ingest that cuts nothing, a retire and a delete land their planned
     * units when they are done; what a job settled with nothing to land
     * (`settle`) counts landed, and a non-replay job that ended has nothing
     * pending. A replay with no `job_cells` yet (waiting on a lock or on a job
     * ahead) counts what it planned as its total; its planned cells stay
     * pending after it ends, which is what a resume replays.
     * The progress is a fact of the store, so a poll costs the index scans
     * and nothing is cached.
     */
    const progress = (id: number) =>
      sql<JobRow & ProgressRow>`
        WITH job AS (
          SELECT *, kind IN ('retire', 'delete')
                    OR kind = 'ingest' AND NOT coalesce((args->>'cut')::boolean, false) AS lands_on_done
          FROM jobs WHERE id = ${id}
        ), cut AS (
          SELECT count(*)::int AS landed
          FROM capture_cuts cc JOIN preps p ON p.id = cc.prep_id
          WHERE p.job_id = ${id}
        ), units AS (
          SELECT coalesce(job.planned, cut.landed) AS total,
                 CASE WHEN job.lands_on_done THEN CASE WHEN job.status = 'done' THEN coalesce(job.planned, 0) ELSE 0 END
                      ELSE cut.landed + coalesce(job.settled, 0) END AS landed,
                 job.status IN ('queued', 'running') AS live
          FROM job CROSS JOIN cut
        ), cells AS (
          SELECT CASE job.kind WHEN 'replay' THEN coalesce(nullif(count(c.*), 0), units.total) ELSE units.total END::int AS total,
                 CASE job.kind WHEN 'replay' THEN count(c.*) FILTER (WHERE c.outcome = 'ran') ELSE units.landed END::int AS landed,
                 CASE job.kind
                   WHEN 'replay' THEN CASE WHEN count(c.*) = 0 THEN units.total ELSE count(c.*) FILTER (WHERE c.outcome = 'planned') END
                   ELSE CASE WHEN units.live THEN greatest(units.total - units.landed, 0) ELSE 0 END END::int AS pending,
                 count(c.*) FILTER (WHERE c.outcome = 'gone: refused')::int AS gone_refused,
                 count(c.*) FILTER (WHERE c.outcome = 'gone: not in this cut')::int AS gone_missing
          FROM job CROSS JOIN units LEFT JOIN job_cells c ON c.job_id = job.id
          GROUP BY job.kind, units.total, units.landed, units.live
        ), results AS (
          SELECT coalesce(sum(a.passed), 0)::int AS passed,
                 coalesce(sum(a.failed), 0)::int AS failed,
                 coalesce(sum(a.skipped), 0)::int AS skipped,
                 count(r.id)::int AS runs,
                 count(r.id) FILTER (WHERE r.finished_at IS NULL)::int AS open_runs
          FROM runs r
          LEFT JOIN LATERAL (
            SELECT count(*) FILTER (WHERE cr.skipped IS NULL AND cr.passed) AS passed,
                   count(*) FILTER (WHERE cr.skipped IS NULL AND NOT cr.passed) AS failed,
                   count(*) FILTER (WHERE cr.skipped IS NOT NULL) AS skipped
            FROM case_results cr WHERE cr.run_id = r.id
          ) a ON true
          WHERE r.job_id = ${id}
        )
        SELECT j.*, cells.*, results.*
        FROM jobs j, cells, results
        WHERE j.id = ${id}
      `

    const show = Effect.fn("Jobs.show")(function*(id: number) {
      const rows = yield* progress(id)
      const row = rows[0]
      if (row === undefined) return null
      return { job: rowOf(row), progress: progressOf(row) } satisfies Detail
    })

    // The statement `show` runs, compiled as the client sends it, under the planner.
    const explain = Effect.fn("Jobs.explain")(function*(id: number) {
      const [text, params] = progress(id).compile()
      const rows = yield* sql.unsafe<{ "QUERY PLAN": string }>(`EXPLAIN (ANALYZE, BUFFERS) ${text}`, params)
      return rows.map((row) => row["QUERY PLAN"])
    })

    /** The list's columns: the row, its units landed over total (a replay's planned cells until its `job_cells` say), its failed results and its gone cells. */
    const listed = (where: Fragment, order: Fragment, limit: number, offset: number) =>
      sql<JobRow & { landed: number; total: number; failed: number; gone: number; count: string }>`
        SELECT j.*,
               CASE
                 WHEN j.kind = 'replay' THEN (SELECT count(*)::int FROM job_cells c WHERE c.job_id = j.id AND c.outcome = 'ran')
                 WHEN j.kind IN ('retire', 'delete') OR j.kind = 'ingest' AND NOT coalesce((j.args->>'cut')::boolean, false)
                   THEN CASE WHEN j.status = 'done' THEN coalesce(j.planned, 0) ELSE 0 END
                 ELSE (SELECT count(*)::int FROM capture_cuts cc JOIN preps p ON p.id = cc.prep_id WHERE p.job_id = j.id)
                      + coalesce(j.settled, 0)
               END AS landed,
               CASE j.kind
                 WHEN 'replay' THEN coalesce(nullif((SELECT count(*)::int FROM job_cells c WHERE c.job_id = j.id), 0), j.planned, 0)
                 ELSE coalesce(j.planned, 0)
               END AS total,
               (SELECT count(*)::int FROM runs r JOIN case_results cr ON cr.run_id = r.id
                WHERE r.job_id = j.id AND cr.skipped IS NULL AND NOT cr.passed) AS failed,
               (SELECT count(*)::int FROM job_cells c WHERE c.job_id = j.id AND c.outcome LIKE 'gone:%') AS gone,
               count(*) OVER () AS count
        FROM jobs j
        WHERE ${where}
        ORDER BY ${order} LIMIT ${limit} OFFSET ${offset}
      `

    const listedOf = (row: JobRow & { landed: number; total: number; failed: number; gone: number }): Listed => ({
      ...rowOf(row),
      landed: Number(row.landed),
      total: Number(row.total),
      failed: Number(row.failed),
      gone: Number(row.gone)
    })

    const list = Effect.fn("Jobs.list")(function*(limit: number) {
      const rows = yield* listed(sql`true`, sql`j.id DESC`, limit, 0)
      return rows.map(listedOf)
    })

    const filter = (status: StatusFilter | undefined): Fragment =>
      status === undefined
        ? sql`true`
        : status === "active"
        ? sql`j.status IN ('running', 'queued')`
        : status === "ended"
        ? sql`j.status IN ('done', 'failed', 'interrupted', 'died')`
        : sql`j.status = ${status}`

    // Running rows lead whatever the filter: the page's first group is what is happening now.
    const page = Effect.fn("Jobs.page")(function*(query: PageQuery) {
      const rows = yield* listed(
        filter(query.status),
        sql`CASE j.status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END, j.id DESC`,
        query.limit,
        query.offset
      )
      const total = rows.length > 0
        ? Number(rows[0]!.count)
        : query.offset === 0
        ? 0
        : Number((yield* sql<{ count: string }>`SELECT count(*) AS count FROM jobs j WHERE ${filter(query.status)}`)[0]!.count)
      return { rows: rows.map(listedOf), total } satisfies Page
    })

    const lineage = Effect.fn("Jobs.lineage")(function*(id: number) {
      type KinRow = { id: string; status: Status; args: Readonly<Record<string, unknown>>; restart_of: string | null }
      const kinOf = (row: KinRow): Kin => ({ id: Number(row.id), status: row.status, mode: modeOf(row.args, row.restart_of) })
      const up = yield* sql<KinRow & { depth: number }>`
        WITH RECURSIVE chain AS (
          SELECT j.id, j.status, j.args, j.restart_of, 0 AS depth FROM jobs j WHERE j.id = ${id}
          UNION ALL
          SELECT j.id, j.status, j.args, j.restart_of, chain.depth + 1
          FROM jobs j JOIN chain ON j.id = chain.restart_of WHERE chain.depth < 100
        )
        SELECT * FROM chain WHERE depth > 0 ORDER BY depth
      `
      const down = yield* sql<KinRow>`
        SELECT j.id, j.status, j.args, j.restart_of FROM jobs j WHERE j.restart_of = ${id} ORDER BY j.id
      `
      return { ancestors: up.map(kinOf), descendants: down.map(kinOf) } satisfies Lineage
    })

    const gone = Effect.fn("Jobs.gone")(function*(id: number, limit: number) {
      const rows = yield* sql<{ capture: string; call_key: string; outcome: Outcome; reasons: ReadonlyArray<string>; count: string }>`
        SELECT capture, call_key, outcome, reasons, count(*) OVER () AS count
        FROM job_cells WHERE job_id = ${id} AND outcome LIKE 'gone:%'
        ORDER BY capture, call_key LIMIT ${limit}
      `
      return {
        cells: rows.map((row): GoneCell => ({
          capture: row.capture,
          callKey: row.call_key,
          outcome: row.outcome,
          reasons: row.reasons
        })),
        total: rows.length > 0 ? Number(rows[0]!.count) : 0
      } satisfies Cells<GoneCell>
    })

    const red = Effect.fn("Jobs.red")(function*(id: number, limit: number) {
      const rows = yield* sql<
        ErrorColumns & { capture: string; lane: string; case_id: string; run_id: string; body_failing: number; count: string }
      >`
        SELECT r.capture, r.lane, r.id AS run_id, cr.case_id,
               cr.error_kind, cr.error_rule, cr.error_rfc, cr.error_step, cr.error_expected, cr.error_detail,
               cr.body_failing,
               count(*) OVER () AS count
        FROM runs r JOIN case_results cr ON cr.run_id = r.id
        WHERE r.job_id = ${id} AND cr.skipped IS NULL AND NOT cr.passed
        ORDER BY r.id, cr.case_id LIMIT ${limit}
      `
      return {
        cells: rows.map((row): RedCell => ({
          capture: row.capture,
          lane: row.lane,
          caseId: row.case_id,
          runId: Number(row.run_id),
          error: errorOf(row),
          bodyFailing: Number(row.body_failing)
        })),
        total: rows.length > 0 ? Number(rows[0]!.count) : 0
      } satisfies Cells<RedCell>
    })

    const running = Effect.gen(function*() {
      const rows = yield* sql<JobRow>`SELECT * FROM jobs WHERE status = 'running' ORDER BY id`
      return rows.map(rowOf)
    })

    return Service.of({
      create,
      started,
      close,
      waiting,
      batch,
      sweep,
      stop,
      planned,
      settle,
      show,
      explain,
      list,
      page,
      lineage,
      gone,
      red,
      running
    })
  })
)
