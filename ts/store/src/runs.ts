/**
 * The run record: opening a run, closing it with its case results, and reading
 * back what a (capture, lane) currently stands at.
 *
 * The id is assigned here and the bundle directory is named after it — that is
 * the whole point of the DB owning run identity, so two concurrent sweeps can
 * never pick the same `runN`. A run also records who made it (`RunIdentity`),
 * and only `@current` runs are ever read as what a call currently stands at
 *.
 *
 * Nothing in this service holds human judgment: the verdict registry in git
 * says what a failure MEANS, this table only says what happened.
 *
 * Every bundle path crossing this interface is ABSOLUTE; the row keeps it
 * relative to the session tree (`sessions.ts`), and the conversion happens here
 * and nowhere else.
 */
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import { SqlClient } from "effect/unstable/sql/SqlClient"
import type { SqlError } from "effect/unstable/sql/SqlError"
import * as path from "node:path"
import { citedRuns } from "./citations.js"
import { hashDocument } from "./hash.js"
import type { CaseOutcome, CaseResult, CaseRun, Deltas, Provenance, RunStatus, SetKind, Standing } from "./model.js"
import { UNKNOWN_PROVENANCE } from "./model.js"
import { type RunIdentity, UNKNOWN_IDENTITY } from "./run-identity.js"
import * as Sessions from "./sessions.js"

export interface OpenRequest {
  readonly capture: string
  readonly lane: string
  readonly mode: string
  /** The triage-session workdir, under the session tree; the run's bundle is `<sessionDir>/run<id>`. */
  readonly sessionDir: string
  readonly provenance?: Provenance
  /** The sweep this run belongs to; absent for a run nobody swept. */
  readonly sweepId?: number
  /** The job that opened this run; absent for a run no job drove. */
  readonly jobId?: number
  /**
   * The cut this run replayed, pinned once by the command that opened it. It is
   * what says which reading of the capture the row answers, so a re-cut landing
   * mid-sweep is recorded rather than invisible. Absent for a run the cut
   * store did not plan — a bundle imported from disk.
   */
  readonly prepId?: number
  /** The case set this run covers; absent is `full`, every case the pinned prep cut for the capture. */
  readonly setKind?: SetKind
  /** Who made the run and under what load; absent is unknown, never `@current`. */
  readonly identity?: RunIdentity
}

export interface OpenedRun {
  readonly id: number
  /** `run<id>` — the directory name the driver must write into. */
  readonly name: string
  readonly bundlePath: string
}

export interface FinishRequest {
  readonly id: number
  readonly status: RunStatus
  readonly detail?: string
  readonly cases: ReadonlyArray<CaseResult>
}

/**
 * A run imported from a bundle that already exists on disk. The id is always
 * freshly assigned, never taken from the directory name: id order is what
 * `standings` reads as recency, and a name-derived id would scramble it.
 */
export interface ImportRequest {
  readonly capture: string
  readonly lane: string
  readonly mode: string
  readonly status: RunStatus
  readonly bundlePath: string
  readonly startedAt?: string
  readonly finishedAt?: string
  readonly provenance?: Provenance
  readonly cases: ReadonlyArray<CaseResult>
}

/** A run still open, and the cut it can still materialise from. */
export interface Unfinished {
  readonly id: number
  /** The prep the run is pinned to; `null` for a run older than the cut store. */
  readonly prepId: number | null
}

/** A bundle a run row names, and what a retention ruling reads of that run. */
export interface RecordedBundle {
  readonly runId: number
  /** Absolute. */
  readonly bundlePath: string
  readonly lane: string
  /** Null for a run still open. */
  readonly finishedAt: string | null
  readonly pinned: boolean
  /** Whether a recorded citation names it, by itself, its job or its sweep (`citations.ts`). */
  readonly cited: boolean
}

/** An unfinished row and the bundle whose mtime is the second half of the evidence. */
export interface Abandonable {
  readonly id: number
  readonly bundlePath: string
}

/** A lane the table holds runs on, and how much of it: the viewer's lane picker. */
export interface Lane {
  readonly lane: string
  readonly runs: number
  readonly captures: number
}

export interface Interface {
  readonly open: (request: OpenRequest) => Effect.Effect<OpenedRun, SqlError>
  /**
   * Many rows opened as one insert, ids assigned in the order given — the
   * order `standings` reads as recency — so a replay over N captures opens N
   * rows in two statements rather than 2N (a row costs ~6 ms on its own).
   */
  readonly openMany: (requests: ReadonlyArray<OpenRequest>) => Effect.Effect<ReadonlyArray<OpenedRun>, SqlError>
  readonly finish: (request: FinishRequest) => Effect.Effect<void, SqlError>
  readonly importRun: (request: ImportRequest) => Effect.Effect<number, SqlError>
  /** Every lane the table has ever recorded a run on, with its size. */
  readonly lanes: Effect.Effect<ReadonlyArray<Lane>, SqlError>
  /** Every capture with a run on the lane, whoever made it, in capture order. */
  readonly ranOn: (lane: string) => Effect.Effect<ReadonlyArray<string>, SqlError>
  /**
   * What each (capture, lane) currently stands at: its latest `@current` run,
   * its latest green full `@current` run, and each case at its current run.
   */
  readonly standings: (lane?: string) => Effect.Effect<ReadonlyArray<Standing>, SqlError>
  /** The same, for ONE capture: `null` where no `@current` run holds it on the lane. */
  readonly standing: (capture: string, lane?: string) => Effect.Effect<Standing | null, SqlError>
  /** Bundle paths cleanup must never delete: every (capture, lane)'s current, last-ok, and every case's current run. */
  readonly protectedBundles: Effect.Effect<ReadonlySet<string>, SqlError>
  /** Every bundle path the table points at, with what a retention ruling reads of its run. */
  readonly allBundles: Effect.Effect<ReadonlyArray<RecordedBundle>, SqlError>
  /**
   * Every run that has not finished, with the prep it pins. What "open" means
   * is one reading taken once — everything from the lowest id up may still be
   * written to, and every prep named here may still be materialised from — and
   * the caller that has just reaped rows is the one that knows which of these
   * are already dead.
   */
  readonly unfinished: Effect.Effect<ReadonlyArray<Unfinished>, SqlError>
  /**
   * Unfinished rows the DATABASE says nobody will close: the run's sweep has
   * already ended — the process that would have closed the row got to the end
   * of its own sweep without doing so — or the row is older than `horizonHours`
   * with no sweep to speak for it. Both arms are candidates only; the disk has
   * the last word (`reap.ts`).
   */
  readonly abandonable: (horizonHours: number) => Effect.Effect<ReadonlyArray<Abandonable>, SqlError>
  /**
   * Closes rows nothing is coming back for, stamping WHY. `finished_at` says
   * when the row was closed, which for these is when it was ruled dead; the
   * status they carry is the `error` they were opened with, because a run that
   * never reported is exactly that.
   */
  readonly abandon: (ids: ReadonlyArray<number>, detail: string) => Effect.Effect<number, SqlError>
  /** The unfinished runs these jobs opened: what a job that ended without closing them leaves behind. */
  readonly unfinishedOf: (jobIds: ReadonlyArray<number>) => Effect.Effect<ReadonlyArray<number>, SqlError>
  /**
   * Records on each run what the lane's post-run cleanup gate found on a batch
   * its cells ran in; a run several dirty batches touched keeps every finding.
   * Answers the rows marked.
   */
  readonly dirty: (ids: ReadonlyArray<number>, finding: string) => Effect.Effect<number, SqlError>
  /** The runs of a sweep still open, with their bundles: what closing a sweep whose process is gone closes. */
  readonly openOfSweep: (sweepId: number) => Effect.Effect<ReadonlyArray<Abandonable>, SqlError>
  /** Wipes every run row whose bundle is under `dir` (the tree, or one session of it); the rebuild's first act. */
  readonly forget: (dir: string) => Effect.Effect<number, SqlError>
}

export class Service extends Context.Service<Service, Interface>()("@sip/store/Runs") {}

interface StandingRow {
  readonly capture: string
  readonly lane: string
  readonly id: number
  readonly status: RunStatus
  readonly prep_id: number | null
  readonly bundle_path: string
  readonly finished_at: Date | null
  readonly cases: number
  readonly failed: number
  readonly skipped: number
  readonly case_ids: ReadonlyArray<string>
  readonly failed_steps: ReadonlyArray<string>
  readonly skip_reasons: ReadonlyArray<string>
  readonly case_rows: ReadonlyArray<CaseOutcome>
  readonly rfc_gating: number | null
  readonly classified: number
  readonly accepted: number | null
  readonly known_bug: number | null
  readonly unlisted: number | null
  readonly unknown: number | null
  readonly compared: number | null
  readonly unreferenced: number | null
  readonly records: number | null
  readonly last_ok_run_id: number | null
  readonly last_ok_bundle_path: string | null
  readonly case_runs: ReadonlyArray<{
    readonly caseId: string
    readonly runId: number
    readonly setKind: SetKind
    readonly passed: boolean
    readonly skipped: string | null
    readonly bundlePath: string
  }>
}

const count = (value: number | null | undefined): number => (value === null || value === undefined ? 0 : Number(value))

const iso = (value: Date | null): string | null => (value === null ? null : value.toISOString())

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const sql = yield* SqlClient
    const root = yield* Sessions.Service

    /**
     * Re-states the run's three rollup counts from the rows just written. It
     * runs in the caller's transaction, so `runs.cases/failed/skipped` is never
     * a stale reading of `case_results` — `standings` reads the run alone.
     */
    const refreshRollup = Effect.fn("Runs.refreshRollup")(function*(runId: number) {
      yield* sql`
        UPDATE runs r
        SET cases = c.cases, failed = c.failed, skipped = c.skipped
        FROM (
          SELECT count(*)::int AS cases,
                 count(*) FILTER (WHERE NOT passed AND skipped IS NULL)::int AS failed,
                 count(*) FILTER (WHERE skipped IS NOT NULL)::int AS skipped
          FROM case_results
          WHERE run_id = ${runId}
        ) c
        WHERE r.id = ${runId}
      `
    })

    const writeCases = Effect.fn("Runs.writeCases")(function*(
      runId: number,
      cases: ReadonlyArray<CaseResult>
    ) {
      for (const entry of cases) {
        let documentHash: string | null = null
        if (entry.document !== null && entry.document !== undefined) {
          const { hash, text } = hashDocument(entry.document)
          yield* sql`
            INSERT INTO case_documents (hash, document)
            VALUES (${hash}, ${text}::jsonb)
            ON CONFLICT (hash) DO NOTHING
          `
          documentHash = hash
        }
        const error = entry.mainError
        yield* sql`
          INSERT INTO case_results
            (run_id, case_id, passed, verdict, skipped, failed_step, rfc_gating, classification, document_hash,
             error_kind, error_rule, error_rfc, error_step, error_expected, error_detail, body_failing)
          VALUES (
            ${runId}, ${entry.caseId}, ${entry.passed}, ${entry.verdict}, ${entry.skipped},
            ${entry.failedStep}, ${entry.rfcGating},
            ${entry.classification === null || entry.classification === undefined
            ? null
            : JSON.stringify(entry.classification)}::jsonb,
            ${documentHash},
            ${error?.kind ?? null}, ${error?.rule ?? null}, ${error?.rfc ?? null},
            ${error?.step ?? null}, ${error?.expected ?? null}, ${error?.detail ?? null},
            ${entry.bodyFailing ?? 0}
          )
          ON CONFLICT (run_id, case_id) DO UPDATE SET
            passed = EXCLUDED.passed,
            verdict = EXCLUDED.verdict,
            skipped = EXCLUDED.skipped,
            failed_step = EXCLUDED.failed_step,
            rfc_gating = EXCLUDED.rfc_gating,
            classification = EXCLUDED.classification,
            document_hash = EXCLUDED.document_hash,
            error_kind = EXCLUDED.error_kind,
            error_rule = EXCLUDED.error_rule,
            error_rfc = EXCLUDED.error_rfc,
            error_step = EXCLUDED.error_step,
            error_expected = EXCLUDED.error_expected,
            error_detail = EXCLUDED.error_detail,
            body_failing = EXCLUDED.body_failing
        `
      }
      yield* refreshRollup(runId)
    })

    const open = Effect.fn("Runs.open")(function*(request: OpenRequest) {
      const [opened] = yield* openMany([request])
      return opened!
    })

    const openMany = Effect.fn("Runs.openMany")(function*(requests: ReadonlyArray<OpenRequest>) {
      if (requests.length === 0) return []
      const rows = requests.map((request, at) => {
        const p = request.provenance ?? UNKNOWN_PROVENANCE
        const who = request.identity ?? UNKNOWN_IDENTITY
        return {
          at,
          capture: request.capture,
          lane: request.lane,
          mode: request.mode,
          sweepId: request.sweepId ?? null,
          jobId: request.jobId ?? null,
          prepId: request.prepId ?? null,
          setKind: request.setKind ?? "full",
          components: p,
          env: who.env,
          branch: who.branch,
          laneHash: who.laneHash,
          concurrency: who.load?.concurrency ?? null,
          batchSize: who.load?.batchSize ?? null,
          startsPerSecond: who.load?.startsPerSecond ?? null,
          maxSpanSeconds: who.load?.maxSpanSeconds ?? null,
          memoryMax: who.load?.memoryMax ?? null
        }
      })
      return yield* sql.withTransaction(Effect.gen(function*() {
        // The ids follow the recordset's own order: the insert reads it sorted.
        const inserted = yield* sql<{ id: string }>`
          INSERT INTO runs (capture, lane, mode, status, bundle_path, sweep_id, job_id, prep_id, set_kind,
                            components,
                            env, branch, lane_hash, concurrency, batch_size, starts_per_second,
                            max_span_seconds, memory_max)
          SELECT r.capture, r.lane, r.mode, 'error', '', r."sweepId", r."jobId", r."prepId", r."setKind",
                 r.components,
                 r.env, r.branch, r."laneHash", r.concurrency, r."batchSize", r."startsPerSecond",
                 r."maxSpanSeconds", r."memoryMax"
          FROM jsonb_to_recordset(${JSON.stringify(rows)}::jsonb)
            AS r(at int, capture text, lane text, mode text, "sweepId" bigint, "jobId" bigint, "prepId" bigint,
                 "setKind" text,
                 components jsonb,
                 env text, branch text, "laneHash" text, concurrency int, "batchSize" int,
                 "startsPerSecond" double precision, "maxSpanSeconds" double precision, "memoryMax" text)
          ORDER BY r.at
          RETURNING id
        `
        const ids = inserted.map((row) => Number(row.id)).sort((a, b) => a - b)
        const opened = requests.map((request, at): OpenedRun => {
          const id = ids[at]!
          const name = `run${id}`
          return { id, name, bundlePath: path.join(request.sessionDir, name) }
        })
        yield* sql`
          UPDATE runs r SET bundle_path = b.path
          FROM jsonb_to_recordset(${
          JSON.stringify(opened.map((run) => ({ id: run.id, path: Sessions.toStored(root, run.bundlePath) })))
        }::jsonb) AS b(id bigint, path text)
          WHERE r.id = b.id
        `
        return opened
      }))
    })

    const finish = Effect.fn("Runs.finish")(function*(request: FinishRequest) {
      yield* sql.withTransaction(Effect.gen(function*() {
        yield* sql`
          UPDATE runs
          SET status = ${request.status},
              detail = ${request.detail ?? null},
              finished_at = now()
          WHERE id = ${request.id}
        `
        yield* writeCases(request.id, request.cases)
      }))
    })

    const importRun = Effect.fn("Runs.importRun")(function*(request: ImportRequest) {
      const p = request.provenance ?? UNKNOWN_PROVENANCE
      return yield* sql.withTransaction(Effect.gen(function*() {
        const rows = yield* sql<{ id: string }>`
          INSERT INTO runs (capture, lane, mode, status, bundle_path, started_at, finished_at, components)
          VALUES (${request.capture}, ${request.lane}, ${request.mode}, ${request.status},
                  ${Sessions.toStored(root, request.bundlePath)},
                  ${request.startedAt ?? null}::timestamptz,
                  ${request.finishedAt ?? null}::timestamptz,
                  ${JSON.stringify(p)}::jsonb)
          RETURNING id
        `
        const id = Number(rows[0]!.id)
        yield* writeCases(id, request.cases)
        return id
      }))
    })

    /**
     * One grouped pass over the `@current` runs of each (capture, lane), then
     * its current cells (the `current_cells` view). `heads` picks
     * the latest `@current` run of any set kind — the run the capture's status
     * reads — and the latest GREEN full one; an `interrupted` run never heads,
     * its landed cells are current through the view. The counts, steps and
     * deltas are the current cells', each at its own latest run, so a cases
     * run moves exactly the calls it replayed. A run of another environment,
     * of no recorded env, or still in flight is never read here.
     */
    const standingRows = (lane: string | undefined, capture: string | undefined) =>
      sql<StandingRow>`
        WITH heads AS (
          SELECT capture, lane,
                 max(id) AS latest_id,
                 max(id) FILTER (
                   WHERE set_kind = 'full' AND status = 'done' AND failed = 0 AND cases > skipped
                 ) AS green_id
          FROM current_runs
          WHERE status <> 'interrupted'
            AND ${lane === undefined ? sql`true` : sql`lane = ${lane}`}
            AND ${capture === undefined ? sql`true` : sql`capture = ${capture}`}
          GROUP BY lane, capture
        ),
        latest AS (
          SELECT r.id, r.capture, r.lane, r.status, r.prep_id, r.bundle_path, r.finished_at, heads.green_id
          FROM heads JOIN runs r ON r.id = heads.latest_id
        ),
        cells AS (
          SELECT c.*
          FROM current_cells c
          WHERE ${lane === undefined ? sql`true` : sql`c.lane = ${lane}`}
            AND ${capture === undefined ? sql`true` : sql`c.capture = ${capture}`}
        ),
        detail AS (
          SELECT cr.capture, cr.lane,
                 count(*)::int AS cases,
                 count(*) FILTER (WHERE NOT cr.passed AND cr.skipped IS NULL)::int AS failed,
                 count(*) FILTER (WHERE cr.skipped IS NOT NULL)::int AS skipped,
                 array_agg(cr.case_id ORDER BY cr.case_id) AS case_ids,
                 coalesce(
                   array_agg(DISTINCT cr.failed_step) FILTER (WHERE cr.failed_step IS NOT NULL),
                   '{}'
                 ) AS failed_steps,
                 coalesce(
                   array_agg(DISTINCT cr.skipped) FILTER (WHERE cr.skipped IS NOT NULL),
                   '{}'
                 ) AS skip_reasons,
                 jsonb_agg(
                   jsonb_build_object(
                     'caseId', cr.case_id,
                     'passed', cr.passed,
                     'failedStep', cr.failed_step,
                     'skipped', cr.skipped
                   ) ORDER BY cr.case_id
                 ) AS case_rows,
                 sum(cr.rfc_gating)::int AS rfc_gating,
                 count(cr.classification)::int AS classified,
                 sum((cr.classification->>'accepted')::int)::int AS accepted,
                 sum((cr.classification->>'known_bug')::int)::int AS known_bug,
                 sum((cr.classification->>'unlisted')::int)::int AS unlisted,
                 sum((cr.classification->>'unknown')::int)::int AS unknown,
                 sum((cr.classification->>'compared')::int)::int AS compared,
                 sum((cr.classification->>'unreferenced')::int)::int AS unreferenced,
                 sum((cr.classification->>'records')::int)::int AS records,
                 jsonb_agg(
                   jsonb_build_object(
                     'caseId', cr.case_id,
                     'runId', cr.run_id,
                     'setKind', cr.set_kind,
                     'passed', cr.passed,
                     'skipped', cr.skipped,
                     'bundlePath', cr.bundle_path
                   ) ORDER BY cr.case_id
                 ) AS case_runs
          FROM cells cr
          GROUP BY cr.capture, cr.lane
        )
        SELECT latest.capture, latest.lane, latest.id, latest.status, latest.prep_id,
               latest.bundle_path, latest.finished_at,
               coalesce(detail.cases, 0) AS cases,
               coalesce(detail.failed, 0) AS failed,
               coalesce(detail.skipped, 0) AS skipped,
               coalesce(detail.case_ids, '{}') AS case_ids,
               coalesce(detail.failed_steps, '{}') AS failed_steps,
               coalesce(detail.skip_reasons, '{}') AS skip_reasons,
               coalesce(detail.case_rows, '[]'::jsonb) AS case_rows,
               detail.rfc_gating,
               coalesce(detail.classified, 0) AS classified,
               detail.accepted, detail.known_bug, detail.unlisted, detail.unknown,
               detail.compared, detail.unreferenced, detail.records,
               green.id AS last_ok_run_id, green.bundle_path AS last_ok_bundle_path,
               coalesce(detail.case_runs, '[]'::jsonb) AS case_runs
        FROM latest
        LEFT JOIN detail ON detail.capture = latest.capture AND detail.lane = latest.lane
        LEFT JOIN runs green ON green.id = latest.green_id
        ORDER BY latest.capture, latest.lane
      `

    const readStandings = Effect.fn("Runs.readStandings")(function*(
      lane: string | undefined,
      capture: string | undefined
    ) {
      const rows = yield* standingRows(lane, capture)
      return rows.map((row): Standing => ({
        capture: row.capture,
        lane: row.lane,
        runId: Number(row.id),
        status: row.status,
        prepId: row.prep_id === null ? null : Number(row.prep_id),
        bundlePath: Sessions.fromStored(root, row.bundle_path),
        finishedAt: iso(row.finished_at),
        cases: Number(row.cases),
        failed: Number(row.failed),
        skipped: Number(row.skipped),
        caseIds: row.case_ids ?? [],
        failedSteps: row.failed_steps ?? [],
        skipReasons: row.skip_reasons ?? [],
        caseResults: row.case_rows ?? [],
        rfcGating: count(row.rfc_gating),
        deltas: {
          cases: count(row.classified),
          accepted: count(row.accepted),
          knownBug: count(row.known_bug),
          unlisted: count(row.unlisted),
          unknown: count(row.unknown),
          compared: count(row.compared),
          unreferenced: count(row.unreferenced),
          records: count(row.records)
        } satisfies Deltas,
        lastOkRunId: row.last_ok_run_id === null ? null : Number(row.last_ok_run_id),
        lastOkBundlePath: row.last_ok_bundle_path === null
          ? null
          : Sessions.fromStored(root, row.last_ok_bundle_path),
        caseRuns: (row.case_runs ?? []).map((entry): CaseRun => ({
          caseId: entry.caseId,
          runId: Number(entry.runId),
          setKind: entry.setKind,
          passed: entry.passed,
          skipped: entry.skipped,
          bundlePath: Sessions.fromStored(root, entry.bundlePath)
        }))
      }))
    })

    const standings = (lane?: string) => readStandings(lane, undefined)

    // One capture, asked for as one capture: the whole-table aggregate is a
    // seconds-long scan, and a viewer opening one study needs one row of it.
    const standing = Effect.fn("Runs.standing")(function*(capture: string, lane?: string) {
      const rows = yield* readStandings(lane, capture)
      return rows[0] ?? null
    })

    const lanes = Effect.gen(function*() {
      // Grouped twice, never `count(DISTINCT capture)`: the distinct count sorts
      // every run row in the table, the two hash passes do not.
      const rows = yield* sql<{ lane: string; runs: number; captures: number }>`
        SELECT lane, sum(n)::int AS runs, count(*)::int AS captures
        FROM (SELECT lane, capture, count(*) AS n FROM runs GROUP BY lane, capture) per_capture
        GROUP BY lane
        ORDER BY lane
      `
      return rows.map((row) => ({
        lane: row.lane,
        runs: Number(row.runs),
        captures: Number(row.captures)
      } satisfies Lane))
    })

    const ranOn = Effect.fn("Runs.ranOn")(function*(lane: string) {
      const rows = yield* sql<{ capture: string }>`
        SELECT DISTINCT capture FROM runs WHERE lane = ${lane} ORDER BY capture
      `
      return rows.map((row) => row.capture)
    })

    const protectedBundles = Effect.gen(function*() {
      const rows = yield* standings()
      const keep = new Set<string>()
      for (const row of rows) {
        keep.add(row.bundlePath)
        if (row.lastOkBundlePath !== null) keep.add(row.lastOkBundlePath)
        for (const entry of row.caseRuns) keep.add(entry.bundlePath)
      }
      return keep as ReadonlySet<string>
    })

    const allBundles = Effect.gen(function*() {
      const rows = yield* sql<{
        id: string
        bundle_path: string
        lane: string
        finished_at: Date | null
        pinned: boolean
        cited: boolean
      }>`
        WITH cited AS MATERIALIZED (${citedRuns(sql)})
        SELECT r.id, r.bundle_path, r.lane, r.finished_at,
               EXISTS (SELECT 1 FROM pins p WHERE p.run_id = r.id) AS pinned,
               r.id IN (SELECT id FROM cited) AS cited
        FROM runs r WHERE r.bundle_path <> '' ORDER BY r.id
      `
      return rows.map((row): RecordedBundle => ({
        runId: Number(row.id),
        bundlePath: Sessions.fromStored(root, row.bundle_path),
        lane: row.lane,
        finishedAt: iso(row.finished_at),
        pinned: row.pinned,
        cited: row.cited
      }))
    })

    const unfinished = Effect.gen(function*() {
      const rows = yield* sql<{ id: string; prep_id: string | null }>`
        SELECT id, prep_id FROM runs WHERE finished_at IS NULL ORDER BY id
      `
      return rows.map((row) => ({
        id: Number(row.id),
        prepId: row.prep_id === null ? null : Number(row.prep_id)
      } satisfies Unfinished))
    })

    const abandonable = Effect.fn("Runs.abandonable")(function*(horizonHours: number) {
      const rows = yield* sql<{ id: string; bundle_path: string }>`
        SELECT r.id, r.bundle_path
        FROM runs r LEFT JOIN sweeps s ON s.id = r.sweep_id
        WHERE r.finished_at IS NULL
          AND (
            s.finished_at IS NOT NULL
            OR r.started_at < now() - make_interval(hours => ${horizonHours})
          )
        ORDER BY r.id
      `
      return rows.map((row) => ({
        id: Number(row.id),
        bundlePath: Sessions.fromStored(root, row.bundle_path)
      }))
    })

    const abandon = Effect.fn("Runs.abandon")(function*(
      ids: ReadonlyArray<number>,
      detail: string
    ) {
      if (ids.length === 0) return 0
      const rows = yield* sql<{ id: string }>`
        UPDATE runs SET finished_at = now(), detail = ${detail}
        WHERE id IN ${sql.in(ids)} AND finished_at IS NULL
        RETURNING id
      `
      return rows.length
    })

    const unfinishedOf = Effect.fn("Runs.unfinishedOf")(function*(jobIds: ReadonlyArray<number>) {
      if (jobIds.length === 0) return []
      const rows = yield* sql<{ id: string }>`
        SELECT id FROM runs WHERE job_id IN ${sql.in(jobIds)} AND finished_at IS NULL ORDER BY id
      `
      return rows.map((row) => Number(row.id))
    })

    const dirty = Effect.fn("Runs.dirty")(function*(ids: ReadonlyArray<number>, finding: string) {
      if (ids.length === 0) return 0
      const rows = yield* sql<{ id: string }>`
        UPDATE runs SET cleanup = CASE WHEN cleanup IS NULL THEN ${finding} ELSE cleanup || '; ' || ${finding} END
        WHERE id IN ${sql.in(ids)}
        RETURNING id
      `
      return rows.length
    })

    const openOfSweep = Effect.fn("Runs.openOfSweep")(function*(sweepId: number) {
      const rows = yield* sql<{ id: string; bundle_path: string }>`
        SELECT id, bundle_path FROM runs WHERE sweep_id = ${sweepId} AND finished_at IS NULL ORDER BY id
      `
      return rows.map((row): Abandonable => ({
        id: Number(row.id),
        bundlePath: Sessions.fromStored(root, row.bundle_path)
      }))
    })

    const forget = Effect.fn("Runs.forget")(function*(dir: string) {
      // The whole tree is every row; a session of it is a stored-prefix match —
      // exact, not LIKE: a directory name may contain `_`, which LIKE reads as
      // a wildcard and would forget a sibling.
      const prefix = path.relative(root.dir, path.resolve(dir)) === "" ? "" : `${Sessions.toStored(root, dir)}/`
      const rows = yield* sql<{ id: string }>`
        DELETE FROM runs WHERE left(bundle_path, length(${prefix})) = ${prefix} RETURNING id
      `
      return rows.length
    })

    return Service.of({
      open,
      openMany,
      finish,
      importRun,
      lanes,
      ranOn,
      standings,
      standing,
      protectedBundles,
      allBundles,
      unfinished,
      abandonable,
      abandon,
      unfinishedOf,
      dirty,
      openOfSweep,
      forget
    })
  })
)

